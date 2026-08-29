// SPDX-License-Identifier: AGPL-3.0-only
//! Reserved `log.md` activity-log projection seam (`pgokf.bundle_log`).
//!
//! # What this projects
//!
//! OKF reserves two per-directory files that are never concepts: `index.md`
//! (which may carry the bundle `okf_version`) and `log.md`, a per-directory
//! **activity log**. Discovery has always skipped both. This module projects
//! the second: during a sync the engine reads every `log.md` in the snapshot
//! through the existing [`crate::catalog::sync::ByteSource`] - without ever
//! staging it as a concept - parses it defensively into ordered entries, and
//! writes them into `pgokf.bundle_log`, keyed by the containing directory and a
//! zero-based ordinal. `index.md` is unchanged; only `log.md` is now projected.
//!
//! # Parsing
//!
//! A `log.md` is Markdown, so it is parsed line by line (the lossless unit an
//! activity log is written in): each non-blank line becomes one entry whose
//! text is the trimmed line stored verbatim, and a leading ISO 8601 timestamp
//! (after any Markdown list bullet or heading marker) is lifted into
//! `logged_at` when present (else `NULL`). Parsing never fails, so a malformed
//! or non-UTF-8 `log.md` degrades to whatever entries it can yield and can
//! never abort the sync.
//!
//! # Refresh semantics
//!
//! [`project`] replaces the whole bundle's log rows (delete-then-insert inside
//! the sync transaction), so editing, adding, or removing a `log.md` is
//! reflected on the next sync and a bundle with no `log.md` simply has no rows.
//! The table cascades from `pgokf.bundles`, so unregistering a bundle drops its
//! log.
//!
//! # Reader surface
//!
//! `pgokf.bundle_log` is a public projection table with the same opt-in
//! multi-tenant row-level security as `pgokf.links`; the reader-granted
//! [`list_bundle_log`](pgokf::list_bundle_log) is an INVOKER function (so the
//! caller's RLS applies) returning `pgokf.bundle_log_entry` ordered by
//! directory then ordinal.

use std::path::Path;

use pgrx::datum::TimestampWithTimeZone;
use pgrx::heap_tuple::PgHeapTuple;
use pgrx::{AllocatedByRust, Spi, extension_sql};

use crate::catalog::batch::BATCH_SIZE;
use crate::catalog::iso8601::parse_iso8601_epoch;
use crate::catalog::spi_read::RowReader;
use crate::errors::CatalogError;
use crate::security;

extension_sql!(
    r"
CREATE TABLE pgokf.bundle_log (
    bundle_id bigint      NOT NULL,
    tenant_id text        NOT NULL DEFAULT 'default',
    directory text        NOT NULL,
    ordinal   integer     NOT NULL,
    logged_at timestamptz,
    entry     text        NOT NULL,
    CONSTRAINT bundle_log_pkey PRIMARY KEY (bundle_id, directory, ordinal),
    CONSTRAINT bundle_log_bundle_fk
        FOREIGN KEY (bundle_id)
        REFERENCES pgokf.bundles (id)
        ON DELETE CASCADE
);

-- Multi-tenant isolation (see pgokf.bundles): opt-in-by-usage RLS on the
-- denormalized tenant_id. Not forced, so the SECURITY DEFINER sync path bypasses
-- it to project a single-tenant bundle's log entries.
ALTER TABLE pgokf.bundle_log ENABLE ROW LEVEL SECURITY;
CREATE POLICY bundle_log_tenant_isolation ON pgokf.bundle_log
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

COMMENT ON TABLE pgokf.bundle_log IS
    'Projection of the reserved OKF per-directory log.md activity logs of a bundle: one row per parsed log entry, keyed by the containing directory and a zero-based ordinal. Reserved log.md files are never concepts; this table is the only place they are projected. Replaced wholesale on every sync so it tracks the files, and cascades from pgokf.bundles.';
COMMENT ON COLUMN pgokf.bundle_log.bundle_id IS
    'Bundle the log entry belongs to (references pgokf.bundles.id; ON DELETE CASCADE).';
COMMENT ON COLUMN pgokf.bundle_log.tenant_id IS
    'Multi-tenant owner, denormalized from the entry''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';
COMMENT ON COLUMN pgokf.bundle_log.directory IS
    'Bundle-relative directory that contained the log.md this entry came from; the empty string for a root-level log.md. Part of the primary key.';
COMMENT ON COLUMN pgokf.bundle_log.ordinal IS
    'Zero-based position of the entry within its directory''s log.md, in file order; part of the primary key.';
COMMENT ON COLUMN pgokf.bundle_log.logged_at IS
    'The entry''s leading ISO 8601 timestamp (after any Markdown bullet/heading marker), parsed to timestamptz; NULL when the entry carries no parseable leading timestamp.';
COMMENT ON COLUMN pgokf.bundle_log.entry IS
    'The log entry text, stored losslessly as the trimmed source line (including any leading timestamp).';

GRANT SELECT ON pgokf.bundle_log TO pgokf_reader;
",
    name = "bundle_log_table",
    requires = ["catalog_tables"]
);

/// One parsed entry of a `log.md`, prior to projection.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LogEntry {
    /// Zero-based position of the entry within its `log.md`, in file order.
    ordinal: i32,
    /// The entry's leading ISO 8601 instant as epoch seconds, when present.
    logged_at: Option<f64>,
    /// The trimmed source line, stored losslessly.
    entry: String,
}

/// Strip a leading Markdown list bullet (`- `/`* `/`+ `) or ATX heading marker
/// (`#`+ plus spaces) from an entry, returning the remainder used to look for a
/// leading timestamp. The stored `entry` text keeps the marker; only the
/// timestamp probe sees it stripped.
fn timestamp_candidate(entry: &str) -> &str {
    let trimmed = entry.trim_start();
    if trimmed.starts_with('#') {
        return trimmed.trim_start_matches('#').trim_start();
    }
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return rest.trim_start();
        }
    }
    trimmed
}

/// Extract the leading ISO 8601 instant from a timestamp candidate, tolerating
/// the space-separated `YYYY-MM-DD HH:MM[:SS]` form.
///
/// A `log.md` entry may lead with either a single self-contained token
/// (`2026-08-01T09:30:00Z rest…`) or a space-separated date and time
/// (`2026-08-01 09:30 rest…`). Passing only the first whitespace token to
/// [`parse_iso8601_epoch`] would silently truncate the latter to its date and
/// project **midnight** - wrong data, not `NULL`. So the first two tokens are
/// joined and preferred, falling back to the first token alone. That first
/// fallback covers both a date-only lead (`2026-08-01 did X` → the date at
/// midnight, as before) and a self-contained single-token instant carrying
/// trailing prose (whose two-token join would fold the following word into the
/// zone/time and fail to parse). A candidate with no parseable leading
/// timestamp - the common case for prose entries - yields `None`.
fn leading_timestamp(candidate: &str) -> Option<f64> {
    let mut tokens = candidate.split_whitespace();
    let first = tokens.next()?;
    if let Some(second) = tokens.next()
        && let Some(epoch) = parse_iso8601_epoch(&format!("{first} {second}"))
    {
        return Some(epoch);
    }
    parse_iso8601_epoch(first)
}

/// Parse a `log.md`'s raw bytes into ordered [`LogEntry`]s, defensively.
///
/// The bytes are decoded lossily (so a non-UTF-8 log still yields entries), then
/// split into lines. Every non-blank line becomes one entry: its text is the
/// trimmed line stored verbatim (lossless), and its `logged_at` is the leading
/// ISO 8601 timestamp - after any Markdown bullet/heading marker, and honoring
/// the space-separated `YYYY-MM-DD HH:MM[:SS]` form via [`leading_timestamp`] -
/// parsed with [`parse_iso8601_epoch`], or `None` when there is no parseable
/// timestamp. Blank lines are skipped and do not consume an ordinal. This never
/// fails.
pub(crate) fn parse_log(bytes: &[u8]) -> Vec<LogEntry> {
    let text = String::from_utf8_lossy(bytes);
    let mut entries = Vec::new();
    let mut ordinal: i32 = 0;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let logged_at = leading_timestamp(timestamp_candidate(trimmed));
        entries.push(LogEntry {
            ordinal,
            logged_at,
            entry: trimmed.to_owned(),
        });
        // Bounded by max_file_bytes, so overflow is unreachable; saturate
        // defensively rather than panic inside a backend.
        ordinal = ordinal.saturating_add(1);
    }
    entries
}

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// Delete every projected log row for one bundle, so the projection can be
/// rebuilt wholesale from the current `log.md` files.
fn delete_bundle_logs(bundle_id: i64) -> Result<(), CatalogError> {
    Spi::run_with_args(
        "DELETE FROM pgokf.bundle_log WHERE bundle_id = $1",
        &[bundle_id.into()],
    )
    .map_err(|error| spi_error("failed to clear bundle log", &error))?;
    Ok(())
}

/// The current `log.md` entries of one bundle, flattened into the column-major
/// arrays bound by the bulk `pgokf.bundle_log` `INSERT`.
#[derive(Debug, Default)]
struct LogColumns {
    directories: Vec<String>,
    ordinals: Vec<i32>,
    logged_ats: Vec<Option<f64>>,
    entries: Vec<String>,
}

/// Flatten every directory's parsed log entries into the insert columns, in the
/// order the directories were discovered.
fn flatten_logs(directory_logs: &[(String, Vec<LogEntry>)]) -> LogColumns {
    let capacity = directory_logs.iter().map(|(_, e)| e.len()).sum();
    let mut columns = LogColumns {
        directories: Vec::with_capacity(capacity),
        ordinals: Vec::with_capacity(capacity),
        logged_ats: Vec::with_capacity(capacity),
        entries: Vec::with_capacity(capacity),
    };
    for (directory, entries) in directory_logs {
        for entry in entries {
            columns.directories.push(directory.clone());
            columns.ordinals.push(entry.ordinal);
            columns.logged_ats.push(entry.logged_at);
            columns.entries.push(entry.entry.clone());
        }
    }
    columns
}

/// Insert the flattened log entries in bounded [`BATCH_SIZE`] chunks.
fn insert_bundle_logs(bundle_id: i64, columns: &LogColumns) -> Result<(), CatalogError> {
    const INSERT: &str = "
        INSERT INTO pgokf.bundle_log
            (bundle_id, tenant_id, directory, ordinal, logged_at, entry)
        SELECT
            $1,
            (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
            d.directory, d.ordinal, pg_catalog.to_timestamp(d.logged_at), d.entry
        FROM unnest($2::text[], $3::integer[], $4::float8[], $5::text[])
             AS d(directory, ordinal, logged_at, entry)";

    let total = columns.entries.len();
    for start in (0..total).step_by(BATCH_SIZE) {
        let end = usize::min(start + BATCH_SIZE, total);
        Spi::run_with_args(
            INSERT,
            &[
                bundle_id.into(),
                columns.directories[start..end].to_vec().into(),
                columns.ordinals[start..end].to_vec().into(),
                columns.logged_ats[start..end].to_vec().into(),
                columns.entries[start..end].to_vec().into(),
            ],
        )
        .map_err(|error| spi_error("failed to insert bundle log", &error))?;
    }
    Ok(())
}

/// Project a bundle's reserved-`log.md` activity logs into `pgokf.bundle_log`.
///
/// Invoked inside the sync transaction. The bundle's existing log rows are
/// cleared and the current `log.md` entries - one `(directory, entries)` pair
/// per discovered log file - are re-inserted, so the projection tracks the
/// files: an edited log updates, a removed log drops, and a bundle with no
/// `log.md` ends with no rows. Both phases are set-based and chunked at
/// [`BATCH_SIZE`].
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure, aborting the surrounding sync
/// transaction so a partial projection is never committed.
pub(crate) fn project(
    bundle_id: i64,
    directory_logs: &[(String, Vec<LogEntry>)],
) -> Result<(), CatalogError> {
    delete_bundle_logs(bundle_id)?;
    let columns = flatten_logs(directory_logs);
    insert_bundle_logs(bundle_id, &columns)?;
    Ok(())
}

/// Qualified SQL name of the log-entry composite type.
const BUNDLE_LOG_ENTRY_TYPE: &str = "pgokf.bundle_log_entry";

/// One `pgokf.bundle_log` row projected onto the `bundle_log_entry` shape.
struct BundleLogRow {
    bundle_id: i64,
    directory: String,
    ordinal: i32,
    logged_at: Option<TimestampWithTimeZone>,
    entry: String,
}

fn composite_error(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build {BUNDLE_LOG_ENTRY_TYPE} composite: {error}"),
        Path::new(""),
    )
}

/// Pack a [`BundleLogRow`] into a `pgokf.bundle_log_entry` heap tuple.
fn entry_tuple(row: BundleLogRow) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple =
        PgHeapTuple::new_composite_type(BUNDLE_LOG_ENTRY_TYPE).map_err(composite_error)?;
    tuple
        .set_by_name("bundle_id", row.bundle_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("directory", row.directory)
        .map_err(composite_error)?;
    tuple
        .set_by_name("ordinal", row.ordinal)
        .map_err(composite_error)?;
    tuple
        .set_by_name("logged_at", row.logged_at)
        .map_err(composite_error)?;
    tuple
        .set_by_name("entry", row.entry)
        .map_err(composite_error)?;
    Ok(tuple)
}

/// Validate `max_rows` and map it to the SQL `LIMIT` argument.
///
/// A negative bound is a caller error (SQLSTATE `22023`); `0` is accepted and
/// returns no rows.
fn validate_max_rows(max_rows: i32) -> Result<i64, CatalogError> {
    if max_rows < 0 {
        return Err(CatalogError::invalid_parameter(
            format!("max_rows must be greater than or equal to 0, got {max_rows}"),
            Path::new(""),
        ));
    }
    Ok(i64::from(max_rows))
}

/// Read one bundle's log entries, optionally scoped to a single directory.
fn list_bundle_log_impl(
    bundle_id: i64,
    directory: Option<&str>,
    max_rows: i32,
) -> Result<Vec<BundleLogRow>, CatalogError> {
    const QUERY: &str = "
        SELECT bundle_id, directory, ordinal, logged_at, entry
        FROM pgokf.bundle_log
        WHERE bundle_id = $1
          AND ($2::text IS NULL OR directory = $2)
        ORDER BY directory, ordinal
        LIMIT $3";
    // Reader-tier over a public projection table: INVOKER rights, so the
    // caller's own row-level security (the opt-in pgokf.tenant filter) applies
    // to pgokf.bundle_log exactly as it does to a direct SELECT.
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    let limit = validate_max_rows(max_rows)?;
    Spi::connect(|client| {
        let table = client
            .select(
                QUERY,
                None,
                &[bundle_id.into(), directory.into(), limit.into()],
            )
            .map_err(|error| spi_error("failed to read bundle log", &error))?;
        let mut rows = Vec::with_capacity(table.len());
        for row in table {
            let reader = RowReader::new(
                &row,
                "failed to read bundle_log_entry column",
                "bundle_log_entry",
            );
            rows.push(BundleLogRow {
                bundle_id: reader.required(1, "bundle_id")?,
                directory: reader.required(2, "directory")?,
                ordinal: reader.required(3, "ordinal")?,
                logged_at: reader.optional(4)?,
                entry: reader.required(5, "entry")?,
            });
        }
        Ok(rows)
    })
}

/// SQL-facing bundle-log projection, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::iter::SetOfIterator;
    use pgrx::{default, extension_sql, pg_extern};

    use super::{entry_tuple, list_bundle_log_impl};

    extension_sql!(
        r"
CREATE TYPE pgokf.bundle_log_entry AS (
    bundle_id bigint,
    directory text,
    ordinal   integer,
    logged_at timestamptz,
    entry     text
);

COMMENT ON TYPE pgokf.bundle_log_entry IS
    'One reserved-log.md activity-log entry from pgokf.list_bundle_log: the bundle, the containing directory (empty string at the root), the zero-based in-file ordinal, the parsed leading timestamp (NULL when absent), and the lossless entry text.';
",
        name = "bundle_log_entry_type",
        requires = ["catalog_tables"]
    );

    /// List a bundle's reserved-`log.md` activity-log entries.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Pass
    /// `directory` to scope to one directory's log (the empty string for the
    /// bundle root), or leave it `NULL` for every directory. `max_rows` bounds
    /// the rows returned (must be `>= 0`; SQLSTATE `22023` otherwise). Rows are
    /// ordered by directory then ordinal.
    #[pg_extern(stable, parallel_safe, requires = ["bundle_log_entry_type", "bundle_log_table"])]
    fn list_bundle_log(
        bundle_id: i64,
        directory: default!(Option<&str>, "NULL"),
        max_rows: default!(i32, 500),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.bundle_log_entry")> {
        let rows = list_bundle_log_impl(bundle_id, directory, max_rows)
            .unwrap_or_else(|error| error.raise());
        let tuples: Vec<_> = rows
            .into_iter()
            .map(|row| entry_tuple(row).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(tuples)
    }

    extension_sql!(
        r"
REVOKE ALL ON FUNCTION pgokf.list_bundle_log(bigint, text, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.list_bundle_log(bigint, text, integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.list_bundle_log(bigint, text, integer) IS
    'List a bundle''s reserved-log.md activity-log entries as pgokf.bundle_log_entry, ordered by directory then ordinal and bounded by max_rows. Reader-level, STABLE, invoker rights (the caller''s tenant row-level security applies); optionally scoped to one directory. Raises 22023 when max_rows < 0.';
",
        name = "bundle_log_function_hardening",
        requires = [list_bundle_log]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_log_extracts_ordered_entries_and_leading_timestamps() {
        // Arrange: a bulleted activity log with ISO-8601-prefixed entries and a
        // blank line that must not consume an ordinal.
        let log = b"# Activity\n\
                    \n\
                    - 2026-07-01T12:00:00Z Registered the bundle\n\
                    \n\
                    - 2026-07-02T09:30:00Z Refreshed after an edit\n";

        // Act
        let entries = parse_log(log);

        // Assert: three entries (heading + two bullets), contiguous ordinals,
        // timestamps lifted from the two dated bullets, heading has none.
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].ordinal, 0);
        assert_eq!(entries[0].entry, "# Activity");
        assert_eq!(entries[0].logged_at, None);
        assert_eq!(entries[1].ordinal, 1);
        assert_eq!(
            entries[1].entry,
            "- 2026-07-01T12:00:00Z Registered the bundle"
        );
        assert_eq!(
            entries[1].logged_at,
            parse_iso8601_epoch("2026-07-01T12:00:00Z")
        );
        assert_eq!(entries[2].ordinal, 2);
        assert_eq!(
            entries[2].logged_at,
            parse_iso8601_epoch("2026-07-02T09:30:00Z")
        );
    }

    #[test]
    fn parse_log_reads_a_space_separated_date_and_time_as_the_real_instant() {
        // Arrange: a bulleted entry whose leading timestamp is a space-separated
        // `YYYY-MM-DD HH:MM` (not a single `T`-joined token). Taking only the
        // first whitespace token would silently parse it to midnight; the entry
        // must instead resolve to 09:30 of that day.
        let log = b"- 2026-08-01 09:30 did the thing\n";

        // Act
        let entries = parse_log(log);

        // Assert: the parsed instant is the real 09:30 time, not midnight, and
        // the source line is preserved verbatim.
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry, "- 2026-08-01 09:30 did the thing");
        assert_eq!(
            entries[0].logged_at,
            parse_iso8601_epoch("2026-08-01 09:30"),
            "a space-separated date and time must parse to the real instant",
        );
        assert_ne!(
            entries[0].logged_at,
            parse_iso8601_epoch("2026-08-01"),
            "it must not collapse to the date's midnight",
        );
    }

    #[test]
    fn parse_log_reads_a_space_separated_date_time_with_seconds() {
        // Arrange: the same shape carrying an explicit seconds field.
        let log = b"2026-08-01 09:30:15 released\n";

        // Act
        let entries = parse_log(log);

        // Assert
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].logged_at,
            parse_iso8601_epoch("2026-08-01 09:30:15"),
        );
    }

    #[test]
    fn parse_log_reads_a_date_only_lead_as_midnight() {
        // Arrange: a leading date followed only by prose (no time). This still
        // resolves to the date's midnight - the two-token join fails to parse and
        // falls back to the date token alone.
        let log = b"2026-08-01 shipped the release\n";

        // Act
        let entries = parse_log(log);

        // Assert
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].logged_at, parse_iso8601_epoch("2026-08-01"));
    }

    #[test]
    fn parse_log_stores_untimestamped_entries_losslessly() {
        // Arrange: a plain-prose log line with no timestamp.
        let log = b"Did some maintenance today.\n";

        // Act
        let entries = parse_log(log);

        // Assert: the line is kept verbatim with a NULL timestamp.
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry, "Did some maintenance today.");
        assert_eq!(entries[0].logged_at, None);
    }

    #[test]
    fn parse_log_is_empty_for_a_blank_log() {
        // Arrange: only whitespace.
        let log = b"\n   \n\t\n";

        // Act
        let entries = parse_log(log);

        // Assert
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_log_tolerates_non_utf8_bytes() {
        // Arrange: an invalid UTF-8 byte sequence must not panic or fail.
        let log = &[b'-', b' ', 0xff, 0xfe, b' ', b'x', b'\n'];

        // Act
        let entries = parse_log(log);

        // Assert: one lossily-decoded entry, no timestamp.
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].logged_at, None);
    }

    #[test]
    fn timestamp_candidate_strips_bullets_and_headings() {
        assert_eq!(timestamp_candidate("- 2026-07-01 x"), "2026-07-01 x");
        assert_eq!(timestamp_candidate("* 2026-07-01"), "2026-07-01");
        assert_eq!(
            timestamp_candidate("## 2026-07-01 heading"),
            "2026-07-01 heading"
        );
        assert_eq!(timestamp_candidate("plain line"), "plain line");
    }

    #[test]
    fn flatten_logs_preserves_directory_and_entry_order() {
        // Arrange: two directories, each with two entries.
        let directory_logs = vec![
            (
                String::new(),
                vec![
                    LogEntry {
                        ordinal: 0,
                        logged_at: Some(1.0),
                        entry: "root-a".to_owned(),
                    },
                    LogEntry {
                        ordinal: 1,
                        logged_at: None,
                        entry: "root-b".to_owned(),
                    },
                ],
            ),
            (
                "nested".to_owned(),
                vec![LogEntry {
                    ordinal: 0,
                    logged_at: None,
                    entry: "nested-a".to_owned(),
                }],
            ),
        ];

        // Act
        let columns = flatten_logs(&directory_logs);

        // Assert
        assert_eq!(columns.directories, vec!["", "", "nested"]);
        assert_eq!(columns.ordinals, vec![0, 1, 0]);
        assert_eq!(columns.entries, vec!["root-a", "root-b", "nested-a"]);
    }

    #[test]
    fn validate_max_rows_rejects_negative() {
        let error = validate_max_rows(-1).expect_err("negative max_rows must be rejected");
        assert_eq!(error.sqlstate(), "22023");
    }
}
