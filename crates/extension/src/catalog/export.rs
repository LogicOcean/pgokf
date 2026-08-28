//! Parquet snapshot export of the catalog projection (`pgokf.export_parquet`).
//!
//! # What this wave adds
//!
//! A single admin-only entry point, `pgokf.export_parquet(bundle_id,
//! dest_dir)`, that writes one Apache Parquet file per catalog table for the
//! requested bundle — `concepts.parquet`, `concept_metadata.parquet`,
//! `links.parquet`, and `concept_provenance.parquet` — into a validated
//! server-side directory, and returns a `pgokf.export_result` composite with
//! the per-file row counts and the total bytes written. Everything lives in
//! **this file only**; the sync engine and the base schema are untouched.
//!
//! # Security model
//!
//! The function is `SECURITY DEFINER` with a pinned `search_path`, restricted
//! to `pgokf_admin` ([`crate::security::Operation::Register`]) because it
//! reads the full catalog and, uniquely for this extension, *writes files*
//! from inside the server process. The output directory is therefore
//! validated as strictly as a bundle input root:
//!
//! - it must be absolute, NUL-free, and traversal-free
//!   ([`crate::security::validate_path_syntax`]);
//! - it is canonicalized so symlinks cannot redirect the write;
//! - when `pgokf.allowed_roots` is configured, the canonical directory must be
//!   contained within one of the configured roots
//!   ([`crate::security::canonicalize_contained_path`], which resolves
//!   symlinks on both sides). **Residual risk:** when no roots are configured,
//!   the interim policy accepts *any* absolute, canonical, traversal-free,
//!   writable directory on the server filesystem — which is precisely why the
//!   function is gated to `pgokf_admin`. Operators who want a hard boundary
//!   should configure `allowed_roots`;
//! - the directory must already exist and be writable; the function never
//!   creates a directory and never writes outside the validated one.
//!
//! Validation failures are reported as SQLSTATE `22023`
//! ([`crate::errors::ErrorKind::InvalidParameter`]) for a bad or missing
//! directory and `42501`
//! ([`crate::errors::ErrorKind::InsufficientPrivilege`]) for a directory the
//! server process cannot write.
//!
//! # Memory bounds
//!
//! The catalog is never materialized in memory. Each table is streamed with
//! keyset pagination over its primary key ([`build_batch_query`]): a bounded
//! [`EXPORT_BATCH_ROWS`]-row batch is read in its own short SPI session (so
//! the previous batch's tuple table is freed before the next is read), packed
//! into one Arrow [`RecordBatch`], written as a Parquet row group, and
//! flushed. Peak memory is therefore one batch of rows plus one row group,
//! independent of catalog size. Every query is scoped to the requested
//! `bundle_id`, so no other bundle's rows can leak into the export.
//!
//! # `PostgreSQL` type → Arrow type mapping
//!
//! [`arrow_data_type`] is the single source of truth, exercised directly by
//! the unit tests:
//!
//! | `PostgreSQL` column                | Arrow `DataType`                       |
//! | ---------------------------------- | -------------------------------------- |
//! | `bigint`                           | `Int64`                                |
//! | `integer`                          | `Int32`                                |
//! | `text`                             | `Utf8`                                 |
//! | `boolean`                          | `Boolean`                              |
//! | `timestamptz`                      | `Timestamp(Microsecond, "UTC")`        |
//! | `text[]`                           | `List<Utf8>`                           |
//! | `jsonb`                            | `Utf8` (the canonical JSON text)       |
//!
//! `timestamptz` values are converted to microseconds since the Unix epoch in
//! SQL (`EXTRACT(EPOCH …) * 1000000`, exact `numeric` arithmetic) so the Rust
//! side reads a plain `i64`; the `tsvector` search column carries no portable
//! value and is deliberately excluded from the export.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arrow::array::builder::{ListBuilder, StringBuilder};
use arrow::array::{
    ArrayRef, BooleanArray, Int32Array, Int64Array, ListArray, RecordBatch, StringArray,
    TimestampMicrosecondArray,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use pgrx::datum::DatumWithOid;
use pgrx::heap_tuple::PgHeapTuple;
use pgrx::spi::SpiHeapTupleData;
use pgrx::{AllocatedByRust, Spi};

use crate::catalog::config;
use crate::catalog::spi_read;
use crate::errors::CatalogError;
use crate::security;

/// Qualified SQL name of the export-result composite type.
const EXPORT_RESULT_TYPE: &str = "pgokf.export_result";

/// Rows read per keyset batch and written per Parquet row group.
///
/// A small constant keeps peak memory bounded regardless of catalog size;
/// each batch is read in its own SPI session and flushed as one row group.
const EXPORT_BATCH_ROWS: i64 = 2_048;

/// The `PostgreSQL` column types the exporter knows how to project onto Arrow.
///
/// This is the closed set of physical types used by the catalog base tables;
/// [`arrow_data_type`] maps each onto its Arrow `DataType`, and
/// [`ColumnData`] onto its columnar accumulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PgType {
    /// `bigint` → `Int64`.
    Bigint,
    /// `integer` → `Int32`.
    Integer,
    /// `text` → `Utf8`.
    Text,
    /// `boolean` → `Boolean`.
    Boolean,
    /// `timestamptz`, exported as epoch microseconds → `Timestamp(µs, UTC)`.
    TimestamptzMicros,
    /// `text[]` → `List<Utf8>`.
    TextArray,
    /// `jsonb`, exported as its canonical JSON text → `Utf8`.
    Jsonb,
}

/// Fixed timezone metadata attached to every exported timestamp column.
const TIMESTAMP_TZ: &str = "UTC";

/// Map a catalog column's `PostgreSQL` type onto its Arrow `DataType`.
///
/// The single source of truth for the export schema and the property the
/// unit tests pin: `text[]` becomes a nullable-item `List<Utf8>`, `jsonb`
/// becomes `Utf8` (the JSON text produced by the `::text` cast in SQL), and
/// `timestamptz` becomes a UTC microsecond timestamp.
fn arrow_data_type(pg_type: PgType) -> DataType {
    match pg_type {
        PgType::Bigint => DataType::Int64,
        PgType::Integer => DataType::Int32,
        PgType::Text | PgType::Jsonb => DataType::Utf8,
        PgType::Boolean => DataType::Boolean,
        PgType::TimestamptzMicros => {
            DataType::Timestamp(TimeUnit::Microsecond, Some(TIMESTAMP_TZ.into()))
        }
        PgType::TextArray => DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
    }
}

/// One exported column: its Arrow field name, `PostgreSQL` type, nullability,
/// and the SQL expression that produces the value (aliased to `name`).
///
/// `select_expr` is a fixed, code-owned fragment — never caller input — so it
/// carries expressions such as the `jsonb`-to-text cast or the timestamp
/// microsecond conversion without any injection surface.
struct ColumnSpec {
    /// Arrow field name and SELECT alias.
    name: &'static str,
    /// Source `PostgreSQL` type, driving the Arrow mapping and accumulator.
    pg_type: PgType,
    /// Whether the Arrow field is nullable.
    nullable: bool,
    /// SQL expression producing the column, aliased `AS name` in the SELECT.
    select_expr: &'static str,
}

/// One exported table: its output file, source relation, ordered columns, and
/// the column positions that form the keyset ordering key.
struct TableSpec {
    /// Output file name within the validated destination directory.
    file_name: &'static str,
    /// Fully qualified source relation.
    table: &'static str,
    /// Columns in projection order; ordinal `i` maps to SPI position `i + 1`.
    columns: &'static [ColumnSpec],
    /// Positions within [`Self::columns`] of the keyset ordering columns, in
    /// order. Must reference [`PgType::Text`] or [`PgType::Integer`] columns
    /// (the primary-key columns, which are never null).
    key_indices: &'static [usize],
}

impl TableSpec {
    /// The ordering/keyset column names, in key order.
    fn key_names(&self) -> Vec<&'static str> {
        self.key_indices
            .iter()
            .map(|&i| self.columns[i].name)
            .collect()
    }
}

/// `pgokf.concepts` projection. The `tsvector` search column is excluded; the
/// primary key is `(bundle_id, id)`, so `id` alone orders within one bundle.
static CONCEPTS_SPEC: TableSpec = TableSpec {
    file_name: "concepts.parquet",
    table: "pgokf.concepts",
    columns: &[
        ColumnSpec {
            name: "bundle_id",
            pg_type: PgType::Bigint,
            nullable: false,
            select_expr: "bundle_id",
        },
        ColumnSpec {
            name: "id",
            pg_type: PgType::Text,
            nullable: false,
            select_expr: "id",
        },
        ColumnSpec {
            name: "path",
            pg_type: PgType::Text,
            nullable: false,
            select_expr: "path",
        },
        ColumnSpec {
            name: "type",
            pg_type: PgType::Text,
            nullable: true,
            select_expr: "type",
        },
        ColumnSpec {
            name: "title",
            pg_type: PgType::Text,
            nullable: true,
            select_expr: "title",
        },
        ColumnSpec {
            name: "description",
            pg_type: PgType::Text,
            nullable: true,
            select_expr: "description",
        },
        ColumnSpec {
            name: "tags",
            pg_type: PgType::TextArray,
            nullable: true,
            select_expr: "tags",
        },
        ColumnSpec {
            name: "resource",
            pg_type: PgType::Text,
            nullable: true,
            select_expr: "resource",
        },
        ColumnSpec {
            name: "body_text",
            pg_type: PgType::Text,
            nullable: false,
            select_expr: "body_text",
        },
        ColumnSpec {
            name: "file_hash",
            pg_type: PgType::Text,
            nullable: false,
            select_expr: "file_hash",
        },
        ColumnSpec {
            name: "modified_at",
            pg_type: PgType::TimestamptzMicros,
            nullable: true,
            select_expr: "(EXTRACT(EPOCH FROM modified_at) * 1000000)::bigint",
        },
        ColumnSpec {
            name: "indexed_at",
            pg_type: PgType::TimestamptzMicros,
            nullable: false,
            select_expr: "(EXTRACT(EPOCH FROM indexed_at) * 1000000)::bigint",
        },
    ],
    key_indices: &[1],
};

/// `pgokf.concept_metadata` projection, keyed by `(concept_id, key)` within
/// the bundle; `value` is exported as its canonical JSON text.
static METADATA_SPEC: TableSpec = TableSpec {
    file_name: "concept_metadata.parquet",
    table: "pgokf.concept_metadata",
    columns: &[
        ColumnSpec {
            name: "bundle_id",
            pg_type: PgType::Bigint,
            nullable: false,
            select_expr: "bundle_id",
        },
        ColumnSpec {
            name: "concept_id",
            pg_type: PgType::Text,
            nullable: false,
            select_expr: "concept_id",
        },
        ColumnSpec {
            name: "key",
            pg_type: PgType::Text,
            nullable: false,
            select_expr: "key",
        },
        ColumnSpec {
            name: "value",
            pg_type: PgType::Jsonb,
            nullable: false,
            select_expr: "value::text",
        },
    ],
    key_indices: &[1, 2],
};

/// `pgokf.links` projection, keyed by `(source_id, ordinal)` within the
/// bundle.
static LINKS_SPEC: TableSpec = TableSpec {
    file_name: "links.parquet",
    table: "pgokf.links",
    columns: &[
        ColumnSpec {
            name: "bundle_id",
            pg_type: PgType::Bigint,
            nullable: false,
            select_expr: "bundle_id",
        },
        ColumnSpec {
            name: "source_id",
            pg_type: PgType::Text,
            nullable: false,
            select_expr: "source_id",
        },
        ColumnSpec {
            name: "target_id",
            pg_type: PgType::Text,
            nullable: true,
            select_expr: "target_id",
        },
        ColumnSpec {
            name: "link_text",
            pg_type: PgType::Text,
            nullable: true,
            select_expr: "link_text",
        },
        ColumnSpec {
            name: "target_path",
            pg_type: PgType::Text,
            nullable: true,
            select_expr: "target_path",
        },
        ColumnSpec {
            name: "link_kind",
            pg_type: PgType::Text,
            nullable: false,
            select_expr: "link_kind",
        },
        ColumnSpec {
            name: "resolved",
            pg_type: PgType::Boolean,
            nullable: false,
            select_expr: "resolved",
        },
        ColumnSpec {
            name: "is_external",
            pg_type: PgType::Boolean,
            nullable: false,
            select_expr: "is_external",
        },
        ColumnSpec {
            name: "ordinal",
            pg_type: PgType::Integer,
            nullable: false,
            select_expr: "ordinal",
        },
    ],
    key_indices: &[1, 8],
};

/// `pgokf.concept_provenance` scalar projection, keyed by `concept_id` within
/// the bundle; `details` is exported as its canonical JSON text. The `verified[]`
/// event list and `sources[]` materials live in their own child tables and are
/// not part of this scalar export.
static PROVENANCE_SPEC: TableSpec = TableSpec {
    file_name: "concept_provenance.parquet",
    table: "pgokf.concept_provenance",
    columns: &[
        ColumnSpec {
            name: "bundle_id",
            pg_type: PgType::Bigint,
            nullable: false,
            select_expr: "bundle_id",
        },
        ColumnSpec {
            name: "concept_id",
            pg_type: PgType::Text,
            nullable: false,
            select_expr: "concept_id",
        },
        ColumnSpec {
            name: "generated_by",
            pg_type: PgType::Text,
            nullable: true,
            select_expr: "generated_by",
        },
        ColumnSpec {
            name: "generated_at",
            pg_type: PgType::TimestamptzMicros,
            nullable: true,
            select_expr: "(EXTRACT(EPOCH FROM generated_at) * 1000000)::bigint",
        },
        ColumnSpec {
            name: "status",
            pg_type: PgType::Text,
            nullable: true,
            select_expr: "status",
        },
        ColumnSpec {
            name: "stale_after",
            pg_type: PgType::TimestamptzMicros,
            nullable: true,
            select_expr: "(EXTRACT(EPOCH FROM stale_after) * 1000000)::bigint",
        },
        ColumnSpec {
            name: "usage_window_from",
            pg_type: PgType::TimestamptzMicros,
            nullable: true,
            select_expr: "(EXTRACT(EPOCH FROM usage_window_from) * 1000000)::bigint",
        },
        ColumnSpec {
            name: "usage_window_to",
            pg_type: PgType::TimestamptzMicros,
            nullable: true,
            select_expr: "(EXTRACT(EPOCH FROM usage_window_to) * 1000000)::bigint",
        },
        ColumnSpec {
            name: "trust_tier",
            pg_type: PgType::Text,
            nullable: true,
            select_expr: "trust_tier",
        },
        ColumnSpec {
            name: "details",
            pg_type: PgType::Jsonb,
            nullable: false,
            select_expr: "details::text",
        },
    ],
    key_indices: &[1],
};

/// Every table exported by one `pgokf.export_parquet` call, in a fixed order.
static EXPORT_SPECS: &[&TableSpec] = &[
    &CONCEPTS_SPEC,
    &METADATA_SPEC,
    &LINKS_SPEC,
    &PROVENANCE_SPEC,
];

/// A resumable keyset bound value, one per ordering column.
///
/// Only the two key-column shapes the catalog uses are modeled: `text` keys
/// (`id`, `concept_id`, `key`, `source_id`) and the `integer` `ordinal`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyBound {
    /// A `text` key column value.
    Text(String),
    /// An `integer` key column value.
    Int(i32),
}

/// Columnar accumulator for one batch of one column, released after the batch
/// is written so peak memory stays bounded to a single batch.
enum ColumnData {
    Int64(Vec<Option<i64>>),
    Int32(Vec<Option<i32>>),
    Utf8(Vec<Option<String>>),
    Bool(Vec<Option<bool>>),
    TimestampMicros(Vec<Option<i64>>),
    TextList(Vec<Option<Vec<String>>>),
}

impl ColumnData {
    /// Create an empty accumulator for a column's `PostgreSQL` type.
    fn empty(pg_type: PgType) -> Self {
        match pg_type {
            PgType::Bigint => Self::Int64(Vec::new()),
            PgType::Integer => Self::Int32(Vec::new()),
            PgType::Text | PgType::Jsonb => Self::Utf8(Vec::new()),
            PgType::Boolean => Self::Bool(Vec::new()),
            PgType::TimestamptzMicros => Self::TimestampMicros(Vec::new()),
            PgType::TextArray => Self::TextList(Vec::new()),
        }
    }

    /// Read the value at SPI position `ordinal` (1-based) from `row` and
    /// append it to this accumulator.
    fn push_from_row(
        &mut self,
        row: &SpiHeapTupleData<'_>,
        ordinal: usize,
    ) -> Result<(), CatalogError> {
        let read = |error| spi_error("failed to read export column", &error);
        match self {
            Self::Int64(values) | Self::TimestampMicros(values) => {
                values.push(row.get::<i64>(ordinal).map_err(read)?);
            }
            Self::Int32(values) => values.push(row.get::<i32>(ordinal).map_err(read)?),
            Self::Utf8(values) => values.push(row.get::<String>(ordinal).map_err(read)?),
            Self::Bool(values) => values.push(row.get::<bool>(ordinal).map_err(read)?),
            Self::TextList(values) => values.push(row.get::<Vec<String>>(ordinal).map_err(read)?),
        }
        Ok(())
    }

    /// Finish the accumulator into an Arrow array matching [`arrow_data_type`].
    fn finish(self) -> ArrayRef {
        match self {
            Self::Int64(values) => Arc::new(Int64Array::from(values)),
            Self::Int32(values) => Arc::new(Int32Array::from(values)),
            Self::Utf8(values) => Arc::new(StringArray::from(values)),
            Self::Bool(values) => Arc::new(BooleanArray::from(values)),
            Self::TimestampMicros(values) => {
                Arc::new(TimestampMicrosecondArray::from(values).with_timezone(TIMESTAMP_TZ))
            }
            Self::TextList(values) => Arc::new(build_string_list(values)),
        }
    }
}

/// One batch of rows read from one table: the per-column accumulators, the
/// last row's keyset bound (for resuming), and the row count.
struct Batch {
    columns: Vec<ColumnData>,
    last_key: Option<Vec<KeyBound>>,
    rows: usize,
}

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

fn composite_error(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build {EXPORT_RESULT_TYPE} composite: {error}"),
        Path::new(""),
    )
}

/// Build a nullable `List<Utf8>` array from per-row optional string vectors.
///
/// A `None` row is a SQL NULL array; a `Some(vec)` row is a present list
/// (possibly empty). The item field name (`item`) and nullability match the
/// [`arrow_data_type`] schema so [`RecordBatch::try_new`] validates.
fn build_string_list(rows: Vec<Option<Vec<String>>>) -> ListArray {
    let mut builder = ListBuilder::new(StringBuilder::new());
    for row in rows {
        match row {
            Some(items) => {
                for item in items {
                    builder.values().append_value(item);
                }
                builder.append(true);
            }
            None => builder.append(false),
        }
    }
    builder.finish()
}

/// Build the Arrow schema for a table from its column specs.
fn build_schema(spec: &TableSpec) -> SchemaRef {
    let fields: Vec<Field> = spec
        .columns
        .iter()
        .map(|column| {
            Field::new(
                column.name,
                arrow_data_type(column.pg_type),
                column.nullable,
            )
        })
        .collect();
    Arc::new(Schema::new(fields))
}

/// Build one keyset-paginated batch query for a table.
///
/// Every row is scoped to the requested bundle via `bundle_id = $1`. When
/// `resume` is set, a row-value comparison `(k1, …) > ($2, …)` over the
/// ordering columns advances past the previous batch's last key; the matching
/// `ORDER BY` makes the pagination a total order over the primary key. The
/// batch size is applied as the SPI row limit by the caller, not embedded in
/// the text.
///
/// The returned SQL contains no caller-supplied text: table, column, and
/// expression fragments are all fixed [`TableSpec`] data, and every value is
/// bound as a parameter.
fn build_batch_query(spec: &TableSpec, resume: bool) -> String {
    let select_list = spec
        .columns
        .iter()
        .map(|column| format!("{} AS {}", column.select_expr, column.name))
        .collect::<Vec<_>>()
        .join(", ");
    let key_names = spec.key_names();
    let keyset_predicate = if resume {
        let columns = key_names.join(", ");
        let placeholders = (0..key_names.len())
            .map(|offset| format!("${}", offset + 2))
            .collect::<Vec<_>>()
            .join(", ");
        format!(" AND ({columns}) > ({placeholders})")
    } else {
        String::new()
    };
    format!(
        "SELECT {select_list} FROM {table} WHERE bundle_id = $1{keyset_predicate} ORDER BY {order_by}",
        table = spec.table,
        order_by = key_names.join(", "),
    )
}

/// Read the keyset bound of one row from its ordering columns.
fn read_key_bounds(
    spec: &TableSpec,
    row: &SpiHeapTupleData<'_>,
) -> Result<Vec<KeyBound>, CatalogError> {
    let mut bounds = Vec::with_capacity(spec.key_indices.len());
    for &index in spec.key_indices {
        let column = &spec.columns[index];
        let ordinal = index + 1;
        let null_message = format!("keyset column {} is unexpectedly NULL", column.name);
        let bound = match column.pg_type {
            PgType::Text => KeyBound::Text(spi_read::required_column(
                row,
                ordinal,
                "failed to read keyset column",
                &null_message,
            )?),
            PgType::Integer => KeyBound::Int(spi_read::required_column(
                row,
                ordinal,
                "failed to read keyset column",
                &null_message,
            )?),
            other => {
                return Err(CatalogError::internal(
                    format!(
                        "column {} of type {other:?} cannot be a keyset key",
                        column.name
                    ),
                    Path::new(""),
                ));
            }
        };
        bounds.push(bound);
    }
    Ok(bounds)
}

/// Read one bounded batch for a table in its own SPI session.
///
/// The session scopes memory: the batch's tuple table is freed when this
/// closure returns, before the next batch is read.
fn read_batch(
    spec: &TableSpec,
    bundle_id: i64,
    cursor: Option<&Vec<KeyBound>>,
) -> Result<Batch, CatalogError> {
    let query = build_batch_query(spec, cursor.is_some());
    Spi::connect(|client| {
        let mut args: Vec<DatumWithOid> = Vec::with_capacity(1 + spec.key_indices.len());
        args.push(bundle_id.into());
        if let Some(bounds) = cursor {
            for bound in bounds {
                match bound {
                    KeyBound::Text(text) => args.push(text.clone().into()),
                    KeyBound::Int(value) => args.push((*value).into()),
                }
            }
        }
        let table = client
            .select(query.as_str(), Some(EXPORT_BATCH_ROWS), &args)
            .map_err(|error| spi_error("failed to read export batch", &error))?;

        let mut columns: Vec<ColumnData> = spec
            .columns
            .iter()
            .map(|column| ColumnData::empty(column.pg_type))
            .collect();
        let mut last_key: Option<Vec<KeyBound>> = None;
        let mut rows: usize = 0;
        for row in table {
            for (index, column) in columns.iter_mut().enumerate() {
                column.push_from_row(&row, index + 1)?;
            }
            last_key = Some(read_key_bounds(spec, &row)?);
            rows += 1;
        }
        Ok(Batch {
            columns,
            last_key,
            rows,
        })
    })
}

/// `open(2)` flag that refuses to traverse a symbolic link at the final path
/// component (`O_NOFOLLOW`). On Linux this is the fixed constant `0x2_0000`;
/// declaring it locally keeps the crate free of a `libc` dependency, and it is
/// applied through the safe [`OpenOptionsExt::custom_flags`] so no `unsafe` is
/// required. Its effect: if the target already exists and is a symlink, the
/// open fails with `ELOOP` instead of following the link to write elsewhere.
const O_NOFOLLOW: i32 = 0x2_0000;

/// Linux `errno` value returned by an `O_NOFOLLOW` open whose final component
/// is a symbolic link (`ELOOP`). Used to translate that specific failure into
/// a caller-facing `22023` refusal rather than an opaque internal error.
const ELOOP: i32 = 40;

/// Create (truncating) a fresh output file **without following a symlink** at
/// the final path component.
///
/// The destination directory is already validated and canonicalized, but a
/// canonical directory does not stop an attacker who can place files inside it
/// from planting a symlink at the exact output file name (for example
/// `concepts.parquet` → `postgresql.auto.conf`). A plain [`File::create`]
/// follows that link and redirects the write — and its `O_TRUNC` — onto an
/// arbitrary file the server process can write. Opening with [`O_NOFOLLOW`]
/// closes that hole: a symlinked target is refused with `ELOOP`, reported as
/// SQLSTATE `22023`, and nothing is written or truncated through it. A missing
/// path is still created normally; a regular file is still truncated as before.
///
/// Exposed `pub(crate)` so the source-export seam ([`crate::catalog::source`])
/// reconstructs bundle files through the same symlink-refusing open, rather
/// than duplicating the security logic.
pub(crate) fn create_export_file(path: &Path) -> Result<File, CatalogError> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            if error.raw_os_error() == Some(ELOOP) {
                CatalogError::invalid_parameter(
                    format!(
                        "refusing to write export file through a symbolic link: {}",
                        path.display()
                    ),
                    Path::new(""),
                )
            } else {
                CatalogError::internal(
                    format!("failed to create export file {}: {error}", path.display()),
                    Path::new(""),
                )
            }
        })
}

/// Open a ZSTD-compressed Parquet writer over an already-created output file.
fn open_parquet_writer(file: File, schema: &SchemaRef) -> Result<ArrowWriter<File>, CatalogError> {
    let properties = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .build();
    ArrowWriter::try_new(file, Arc::clone(schema), Some(properties))
        .map_err(|error| parquet_error("failed to initialize Parquet writer", &error))
}

/// Finish one batch's columns into an Arrow [`RecordBatch`] and write it as a
/// single Parquet row group, flushed before returning.
fn write_row_group(
    writer: &mut ArrowWriter<File>,
    schema: &SchemaRef,
    columns: Vec<ColumnData>,
    table: &str,
) -> Result<(), CatalogError> {
    let arrays: Vec<ArrayRef> = columns.into_iter().map(ColumnData::finish).collect();
    let record = RecordBatch::try_new(Arc::clone(schema), arrays).map_err(|error| {
        CatalogError::internal(
            format!("failed to assemble Arrow record batch for {table}: {error}"),
            Path::new(""),
        )
    })?;
    writer
        .write(&record)
        .map_err(|error| parquet_error("failed to write Parquet row group", &error))?;
    writer
        .flush()
        .map_err(|error| parquet_error("failed to flush Parquet row group", &error))?;
    Ok(())
}

/// Size in bytes of a finalized export file.
fn export_file_len(path: &Path) -> Result<u64, CatalogError> {
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|error| {
            CatalogError::internal(
                format!("failed to stat export file {}: {error}", path.display()),
                Path::new(""),
            )
        })
}

/// Stream one table to a Parquet file, returning `(rows_written, bytes)`.
///
/// Rows are read in keyset batches and written incrementally; each batch is
/// one row group, flushed before the next is read, so neither the reader nor
/// the writer holds more than a single batch.
fn export_table(
    spec: &TableSpec,
    bundle_id: i64,
    dir: &Path,
) -> Result<(usize, u64), CatalogError> {
    let schema = build_schema(spec);
    let path = dir.join(spec.file_name);
    let mut writer = open_parquet_writer(create_export_file(&path)?, &schema)?;

    let mut cursor: Option<Vec<KeyBound>> = None;
    let mut total: usize = 0;
    loop {
        let batch = read_batch(spec, bundle_id, cursor.as_ref())?;
        if batch.rows == 0 {
            break;
        }
        let is_last = batch.rows < EXPORT_BATCH_ROWS_USIZE;
        let rows = batch.rows;
        write_row_group(&mut writer, &schema, batch.columns, spec.table)?;
        total += rows;
        cursor = batch.last_key;
        if is_last {
            break;
        }
    }

    writer
        .close()
        .map_err(|error| parquet_error("failed to finalize Parquet file", &error))?;
    let bytes = export_file_len(&path)?;
    Ok((total, bytes))
}

/// [`EXPORT_BATCH_ROWS`] as a `usize` for the short-batch termination test.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
const EXPORT_BATCH_ROWS_USIZE: usize = EXPORT_BATCH_ROWS as usize;

fn parquet_error(context: &str, error: &parquet::errors::ParquetError) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// A unique, unobtrusive probe file name for the writability check.
fn probe_name() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!(".pgokf-export-probe-{}-{nonce}", std::process::id())
}

/// Confirm the destination directory is writable by the server process.
///
/// Creates and removes a probe file *inside* the validated directory; a
/// failure is reported as `42501` so the caller learns the directory exists
/// but cannot be written.
fn ensure_writable(dir: &Path) -> Result<(), CatalogError> {
    let probe = dir.join(probe_name());
    // The probe is created with `O_NOFOLLOW` for the same reason the export
    // files are (see [`create_export_file`]): a symlink planted at the probe
    // path must never redirect this create/truncate onto another file.
    match OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(O_NOFOLLOW)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(error) => Err(CatalogError::insufficient_privilege(
            format!("dest_dir is not writable: {} ({error})", dir.display()),
            Path::new(""),
        )),
    }
}

/// Validate and canonicalize the destination directory.
///
/// Enforces the security policy documented at the module level: absolute,
/// traversal-free, canonical, contained within `allowed_roots` when
/// configured, an existing directory, and writable.
///
/// Exposed `pub(crate)` so the source-export seam ([`crate::catalog::source`])
/// validates its destination directory through the identical policy instead of
/// duplicating it.
pub(crate) fn validate_dest_dir(dest_dir: &str) -> Result<PathBuf, CatalogError> {
    let requested = Path::new(dest_dir);
    security::validate_path_syntax(requested, Path::new(""))?;

    let roots = config::allowed_roots()?;
    let canonical = if roots.is_empty() {
        std::fs::canonicalize(requested).map_err(|error| {
            CatalogError::invalid_parameter(
                format!(
                    "failed to canonicalize dest_dir {}: {error}",
                    requested.display()
                ),
                Path::new(""),
            )
        })?
    } else {
        security::canonicalize_contained_path(requested, &roots, Path::new(""))?
    };

    if !canonical.is_dir() {
        return Err(CatalogError::invalid_parameter(
            format!("dest_dir is not a directory: {}", canonical.display()),
            Path::new(""),
        ));
    }
    ensure_writable(&canonical)?;
    Ok(canonical)
}

/// Reject an unknown `bundle_id` before any file is written.
fn ensure_bundle_exists(bundle_id: i64) -> Result<(), CatalogError> {
    let exists = Spi::get_one_with_args::<bool>(
        "SELECT EXISTS (SELECT 1 FROM pgokf.bundles WHERE id = $1)",
        &[bundle_id.into()],
    )
    .map_err(|error| spi_error("failed to look up bundle", &error))?
    .unwrap_or(false);
    if exists {
        Ok(())
    } else {
        Err(CatalogError::invalid_parameter(
            format!("bundle {bundle_id} is not registered"),
            Path::new(""),
        ))
    }
}

/// Clamp a row count into the `bigint` range of the result composite.
fn count_to_i64(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// Sum per-file byte counts, clamped into the `bigint` range.
fn total_bytes(sizes: &[u64]) -> i64 {
    let sum: u128 = sizes.iter().map(|&size| u128::from(size)).sum();
    i64::try_from(sum).unwrap_or(i64::MAX)
}

/// The per-file counts and total size returned by one export.
struct ExportSummary {
    bundle_id: i64,
    dest_dir: String,
    concepts_rows: i64,
    metadata_rows: i64,
    links_rows: i64,
    provenance_rows: i64,
    bytes_written: i64,
}

/// Authorize, validate the destination, and export every catalog table for
/// one bundle.
fn export_parquet_impl(bundle_id: i64, dest_dir: &str) -> Result<ExportSummary, CatalogError> {
    security::authorize_current_user(security::Operation::Register, Path::new(""))?;
    // Write-side tenant confinement: a scoped session may only export its own
    // tenant's bundle. Checked before the directory is validated (a filesystem
    // side effect) so a cross-tenant or absent id looks identically unknown.
    security::enforce_bundle_tenant(bundle_id)?;
    ensure_bundle_exists(bundle_id)?;
    let dir = validate_dest_dir(dest_dir)?;

    let mut rows: Vec<usize> = Vec::with_capacity(EXPORT_SPECS.len());
    let mut sizes: Vec<u64> = Vec::with_capacity(EXPORT_SPECS.len());
    for spec in EXPORT_SPECS {
        let (table_rows, table_bytes) = export_table(spec, bundle_id, &dir)?;
        rows.push(table_rows);
        sizes.push(table_bytes);
    }

    Ok(ExportSummary {
        bundle_id,
        dest_dir: dir.to_string_lossy().into_owned(),
        concepts_rows: count_to_i64(rows[0]),
        metadata_rows: count_to_i64(rows[1]),
        links_rows: count_to_i64(rows[2]),
        provenance_rows: count_to_i64(rows[3]),
        bytes_written: total_bytes(&sizes),
    })
}

/// Pack an [`ExportSummary`] into a `pgokf.export_result` heap tuple.
fn export_result_tuple(
    summary: ExportSummary,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple = PgHeapTuple::new_composite_type(EXPORT_RESULT_TYPE).map_err(composite_error)?;
    tuple
        .set_by_name("bundle_id", summary.bundle_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("dest_dir", summary.dest_dir)
        .map_err(composite_error)?;
    tuple
        .set_by_name("concepts_rows", summary.concepts_rows)
        .map_err(composite_error)?;
    tuple
        .set_by_name("metadata_rows", summary.metadata_rows)
        .map_err(composite_error)?;
    tuple
        .set_by_name("links_rows", summary.links_rows)
        .map_err(composite_error)?;
    tuple
        .set_by_name("provenance_rows", summary.provenance_rows)
        .map_err(composite_error)?;
    tuple
        .set_by_name("bytes_written", summary.bytes_written)
        .map_err(composite_error)?;
    Ok(tuple)
}

/// SQL-facing export entry point, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::{extension_sql, pg_extern};

    use super::{export_parquet_impl, export_result_tuple};

    extension_sql!(
        r"
CREATE TYPE pgokf.export_result AS (
    bundle_id       bigint,
    dest_dir        text,
    concepts_rows   bigint,
    metadata_rows   bigint,
    links_rows      bigint,
    provenance_rows bigint,
    bytes_written   bigint
);

COMMENT ON TYPE pgokf.export_result IS
    'Result of pgokf.export_parquet: the resolved destination directory, the per-file row counts, and the total bytes written across the four Parquet files.';
",
        name = "export_result_type",
        requires = ["catalog_tables"]
    );

    /// Export a bundle's catalog projection to Parquet files on the server.
    ///
    /// Writes `concepts.parquet`, `concept_metadata.parquet`,
    /// `links.parquet`, and `concept_provenance.parquet` for `bundle_id` into
    /// the already-existing, writable `dest_dir`, and returns the per-file row
    /// counts plus total bytes. Requires membership in `pgokf_admin`. The
    /// destination is validated exactly like a bundle root: absolute,
    /// traversal-free, canonical, contained within `pgokf.allowed_roots` when
    /// configured, and writable. Raises SQLSTATE `22023` for an unknown
    /// bundle or an invalid/missing directory, and `42501` for a directory
    /// the server process cannot write.
    #[pg_extern(requires = ["export_result_type"])]
    fn export_parquet(
        bundle_id: i64,
        dest_dir: &str,
    ) -> pgrx::composite_type!('static, "pgokf.export_result") {
        let summary =
            export_parquet_impl(bundle_id, dest_dir).unwrap_or_else(|error| error.raise());
        export_result_tuple(summary).unwrap_or_else(|error| error.raise())
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.export_parquet(bigint, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.export_parquet(bigint, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.export_parquet(bigint, text) TO pgokf_admin;
COMMENT ON FUNCTION pgokf.export_parquet(bigint, text) IS
    'Export one bundle''s concepts, concept_metadata, links, and concept_provenance to Parquet files in dest_dir; returns pgokf.export_result. Admin-only; dest_dir must be an existing, writable, canonical directory contained within pgokf.allowed_roots when configured. Raises 22023 (bad bundle/dir) or 42501 (dir not writable).';
",
        name = "export_function_hardening",
        requires = [export_parquet]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrow_data_type_maps_scalar_pg_types() {
        // Arrange & Act & Assert: each scalar PG type maps to its Arrow type.
        assert_eq!(arrow_data_type(PgType::Bigint), DataType::Int64);
        assert_eq!(arrow_data_type(PgType::Integer), DataType::Int32);
        assert_eq!(arrow_data_type(PgType::Text), DataType::Utf8);
        assert_eq!(arrow_data_type(PgType::Boolean), DataType::Boolean);
    }

    #[test]
    fn arrow_data_type_maps_jsonb_to_utf8() {
        // Arrange: jsonb is exported as its JSON text.
        // Act
        let mapped = arrow_data_type(PgType::Jsonb);

        // Assert
        assert_eq!(mapped, DataType::Utf8);
    }

    #[test]
    fn arrow_data_type_maps_timestamptz_to_utc_microseconds() {
        // Arrange & Act
        let mapped = arrow_data_type(PgType::TimestamptzMicros);

        // Assert
        assert_eq!(
            mapped,
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
    }

    #[test]
    fn arrow_data_type_maps_text_array_to_nullable_utf8_list() {
        // Arrange & Act
        let mapped = arrow_data_type(PgType::TextArray);

        // Assert: a List whose nullable item field is Utf8, matching the
        // ListBuilder the exporter uses so RecordBatch validation passes.
        assert_eq!(
            mapped,
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)))
        );
    }

    #[test]
    fn build_schema_projects_every_column_in_order() {
        // Arrange: the concepts table, whose tsvector column is excluded.
        // Act
        let schema = build_schema(&CONCEPTS_SPEC);

        // Assert
        assert_eq!(schema.fields().len(), CONCEPTS_SPEC.columns.len());
        assert_eq!(schema.field(0).name(), "bundle_id");
        assert_eq!(schema.field(0).data_type(), &DataType::Int64);
        assert!(!schema.field(0).is_nullable());
        let tags = schema.field_with_name("tags").expect("tags field present");
        assert_eq!(
            tags.data_type(),
            &DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)))
        );
        assert!(tags.is_nullable());
    }

    #[test]
    fn build_batch_query_without_cursor_omits_the_keyset_predicate() {
        // Arrange: the first batch has no resume cursor.
        // Act
        let query = build_batch_query(&CONCEPTS_SPEC, false);

        // Assert
        assert!(query.contains("FROM pgokf.concepts WHERE bundle_id = $1"));
        assert!(query.trim_end().ends_with("ORDER BY id"));
        assert!(!query.contains(" AND ("));
        assert!(
            query.contains("(EXTRACT(EPOCH FROM modified_at) * 1000000)::bigint AS modified_at")
        );
    }

    #[test]
    fn build_batch_query_single_key_resumes_with_scalar_comparison() {
        // Arrange: a resuming batch over the single-column concepts key.
        // Act
        let query = build_batch_query(&CONCEPTS_SPEC, true);

        // Assert
        assert!(query.contains("WHERE bundle_id = $1 AND (id) > ($2)"));
        assert!(query.trim_end().ends_with("ORDER BY id"));
    }

    #[test]
    fn build_batch_query_composite_key_resumes_with_row_comparison() {
        // Arrange: the two-column links key (source_id, ordinal).
        // Act
        let query = build_batch_query(&LINKS_SPEC, true);

        // Assert: a row-value comparison and matching ORDER BY over both keys.
        assert!(query.contains("AND (source_id, ordinal) > ($2, $3)"));
        assert!(query.trim_end().ends_with("ORDER BY source_id, ordinal"));
    }

    #[test]
    fn build_batch_query_scopes_every_table_to_the_bundle() {
        // Arrange: all exported tables must filter by bundle_id.
        for spec in EXPORT_SPECS {
            // Act
            let query = build_batch_query(spec, false);

            // Assert
            assert!(
                query.contains("WHERE bundle_id = $1"),
                "{} is not bundle-scoped",
                spec.table
            );
        }
    }

    #[test]
    fn key_indices_reference_only_text_or_integer_columns() {
        // Arrange: keyset keys must be readable as KeyBound variants.
        for spec in EXPORT_SPECS {
            for &index in spec.key_indices {
                // Act
                let pg_type = spec.columns[index].pg_type;

                // Assert
                assert!(
                    matches!(pg_type, PgType::Text | PgType::Integer),
                    "{}.{} is not a valid keyset key type",
                    spec.table,
                    spec.columns[index].name
                );
            }
        }
    }

    #[test]
    fn count_to_i64_saturates_at_i64_max() {
        // Arrange
        let out_of_range = usize::MAX;

        // Act
        let converted = count_to_i64(out_of_range);

        // Assert
        assert_eq!(converted, i64::MAX);
    }

    #[test]
    fn total_bytes_sums_file_sizes() {
        // Arrange
        let sizes = [10_u64, 20, 30, 40];

        // Act
        let total = total_bytes(&sizes);

        // Assert
        assert_eq!(total, 100);
    }

    /// Build a two-row `concepts` batch exercising every finished array type:
    /// `Int64`, `Utf8` (present and NULL), the `List<Utf8>` from
    /// [`build_string_list`], and the UTC microsecond timestamp.
    fn sample_concepts_columns() -> Vec<ColumnData> {
        vec![
            ColumnData::Int64(vec![Some(7), Some(7)]),
            ColumnData::Utf8(vec![Some("alpha".to_owned()), Some("beta".to_owned())]),
            ColumnData::Utf8(vec![
                Some("alpha.md".to_owned()),
                Some("beta.md".to_owned()),
            ]),
            ColumnData::Utf8(vec![Some("Reference".to_owned()), None]),
            ColumnData::Utf8(vec![Some("Alpha".to_owned()), Some("Beta".to_owned())]),
            ColumnData::Utf8(vec![None, None]),
            ColumnData::TextList(vec![Some(vec!["x".to_owned(), "y".to_owned()]), None]),
            ColumnData::Utf8(vec![None, None]),
            ColumnData::Utf8(vec![Some(String::new()), Some(String::new())]),
            ColumnData::Utf8(vec![Some("h1".to_owned()), Some("h2".to_owned())]),
            ColumnData::TimestampMicros(vec![Some(1_600_000_000_000_000), None]),
            ColumnData::TimestampMicros(vec![
                Some(1_700_000_000_000_000),
                Some(1_700_000_000_000_001),
            ]),
        ]
    }

    #[test]
    fn create_export_file_refuses_symlink_at_output_path() {
        use std::os::unix::fs::symlink;

        // Arrange: a validated destination directory into which an attacker has
        // planted a symlink at the exact output file name, pointing at a
        // pre-existing "sensitive" file (standing in for postgresql.auto.conf).
        let base = std::env::temp_dir().join(format!(
            "pgokf-export-nofollow-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        let dir = base.join("dest");
        std::fs::create_dir_all(&dir).expect("create dest dir");
        let target = base.join("sensitive.conf");
        std::fs::write(&target, b"original").expect("seed sensitive target");
        let link = dir.join("concepts.parquet");
        symlink(&target, &link).expect("plant symlink at output path");

        // Act: attempt to create the export file at the symlinked path.
        let result = create_export_file(&link);

        // Assert: the create is refused, the symlink target is byte-for-byte
        // untouched (no write and, crucially, no O_TRUNC escaped through it),
        // and the planted link itself is left in place rather than followed.
        assert!(result.is_err(), "a symlinked output path must be refused");
        let after = std::fs::read(&target).expect("target still readable");
        assert_eq!(
            after, b"original",
            "no write or truncation escaped through the symlink"
        );
        let link_meta = std::fs::symlink_metadata(&link).expect("link still present");
        assert!(
            link_meta.file_type().is_symlink(),
            "the planted link must remain a symlink, not be replaced by a file"
        );

        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn create_export_file_creates_a_fresh_regular_file() {
        // Arrange: a clean directory with no file at the output path.
        let base = std::env::temp_dir().join(format!(
            "pgokf-export-fresh-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&base).expect("create dir");
        let path = base.join("concepts.parquet");

        // Act
        let file = create_export_file(&path);

        // Assert: a real, regular file is created at exactly the requested path.
        assert!(file.is_ok(), "a fresh output path must be creatable");
        let meta = std::fs::metadata(&path).expect("created file present");
        assert!(
            meta.file_type().is_file(),
            "the output must be a regular file"
        );

        drop(file);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn parquet_round_trip_preserves_rows_and_column_types() {
        // Arrange: the real concepts schema and a batch built through the same
        // ColumnData::finish path the exporter uses at runtime.
        let schema = build_schema(&CONCEPTS_SPEC);
        let arrays: Vec<ArrayRef> = sample_concepts_columns()
            .into_iter()
            .map(ColumnData::finish)
            .collect();
        let record = RecordBatch::try_new(Arc::clone(&schema), arrays)
            .expect("arrays must match the concepts schema");
        let path = std::env::temp_dir().join(format!(
            "pgokf-export-roundtrip-{}-{}.parquet",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));

        // Act: write with the exporter's writer configuration, then read back
        // with the parquet crate's own reader.
        let file = File::create(&path).expect("create round-trip file");
        let properties = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::default()))
            .build();
        let mut writer = ArrowWriter::try_new(file, Arc::clone(&schema), Some(properties))
            .expect("initialize writer");
        writer.write(&record).expect("write row group");
        writer.flush().expect("flush row group");
        writer.close().expect("finalize file");

        let read_file = File::open(&path).expect("open round-trip file");
        let reader =
            parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(read_file)
                .expect("build parquet reader")
                .build()
                .expect("open record batch reader");
        let mut total_rows = 0;
        let mut read_schema = None;
        for batch in reader {
            let batch = batch.expect("read record batch");
            read_schema = Some(batch.schema());
            total_rows += batch.num_rows();
        }
        let _ = std::fs::remove_file(&path);

        // Assert: both rows survive and the tricky column types round-trip.
        assert_eq!(total_rows, 2);
        let read_schema = read_schema.expect("at least one batch was read");
        assert_eq!(read_schema.fields().len(), CONCEPTS_SPEC.columns.len());
        let tags = read_schema
            .field_with_name("tags")
            .expect("tags field present");
        assert!(
            matches!(tags.data_type(), DataType::List(_)),
            "tags did not round-trip as a List: {:?}",
            tags.data_type()
        );
        let modified_at = read_schema
            .field_with_name("modified_at")
            .expect("modified_at field present");
        assert_eq!(
            modified_at.data_type(),
            &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
    }
}
