// SPDX-License-Identifier: AGPL-3.0-only
//! Checking a built plugin's lockfile against the live catalog
//! (`check_workspace_plugin_freshness`, spec §4.8): a downloaded plugin
//! cannot update itself, so its `okf-workspace.lock` - which pins each
//! bundle's sync hash and, on catalogs that have them, the catalog
//! generation and freshness state, plus the build-time freshness evidence
//! of every content entry - is compared with the catalog as it is now,
//! answering `current`, `stale`, `retired`, or `unknown` with reasons.

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use tokio_postgres::GenericClient;

/// The freshness of a built plugin - or of one of its pinned bundles -
/// against the live catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginStatus {
    /// Everything the lockfile pins still holds, and no lockfile entry
    /// recorded content the catalog already reported as not fresh at build
    /// time.
    Current,
    /// The catalog moved on: a pinned generation or sync hash no longer
    /// matches, or the bundle's effective freshness is not `fresh` - or the
    /// lockfile's own entries record content that was already stale when
    /// the plugin was built. An artifact marked that way does not turn
    /// current when the catalog later reconciles: the comparison is about
    /// the bytes that were shipped, so rebuild the plugin after the catalog
    /// reconciles and the new lockfile's entries record the fresh state.
    Stale,
    /// A pinned bundle is retired, disabled, or gone.
    Retired,
    /// The comparison cannot say (the lockfile pins nothing comparable, or
    /// the catalog predates the freshness surface and the hash matches, or
    /// a pin has no live counterpart to compare against).
    Unknown,
}

impl PluginStatus {
    /// The identifier reported to callers.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Stale => "stale",
            Self::Retired => "retired",
            Self::Unknown => "unknown",
        }
    }
}

/// One pinned bundle's side of the comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BundleCheck {
    pub bundle_id: i64,
    pub name: Option<String>,
    pub status: PluginStatus,
    pub reasons: Vec<String>,
}

/// The outcome of comparing a lockfile with the live catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginCheck {
    /// The plugin name the lockfile records.
    pub name: Option<String>,
    /// The worst status across the pinned bundles: any retired bundle makes
    /// the plugin retired, then stale, then unknown, then current.
    pub status: PluginStatus,
    pub reasons: Vec<String>,
    pub bundles: Vec<BundleCheck>,
}

/// One bundle as the lockfile pins it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PinnedBundle {
    id: i64,
    name: Option<String>,
    sync_hash: Option<String>,
    catalog_generation: Option<i64>,
}

/// One lockfile entry's build-time evidence: what the catalog reported for
/// that file's concept when the plugin was built. Only entries whose
/// lockfile records a freshness state play a part; entries from before the
/// freshness surface carry none.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PinnedEntry {
    bundle_id: i64,
    concept_id: String,
    state: String,
    reasons: Vec<String>,
}

/// The live bundle row of the comparison statement, as plain data so the
/// comparison itself is unit-tested (a `tokio_postgres::Row` cannot be
/// constructed in a test).
#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveBundle {
    sync_hash: Option<String>,
    catalog_generation: Option<i64>,
    enabled: bool,
    retired: bool,
    freshness_state: Option<String>,
    freshness_reasons: Vec<String>,
}

/// The plugin name, pinned bundles, and per-entry freshness evidence of an
/// `okf-workspace.lock` document.
///
/// # Errors
///
/// The content is not JSON, or not a lockfile (no `catalog.bundles`).
fn parse_lock(content: &str) -> Result<(Option<String>, Vec<PinnedBundle>, Vec<PinnedEntry>)> {
    let lock: serde_json::Value =
        serde_json::from_str(content).context("the lock content is not valid JSON")?;
    let name = lock
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let bundles = lock
        .pointer("/catalog/bundles")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow!("not an okf-workspace.lock: no catalog.bundles list"))?;
    let mut pinned = Vec::with_capacity(bundles.len());
    for entry in bundles {
        let id = entry
            .get("id")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| anyhow!("a lockfile bundle entry has no integer id"))?;
        pinned.push(PinnedBundle {
            id,
            name: entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            sync_hash: entry
                .get("sync_hash")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            catalog_generation: entry
                .get("catalog_generation")
                .and_then(serde_json::Value::as_i64),
        });
    }
    let mut entries = Vec::new();
    for entry in lock
        .pointer("/entries")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(bundle_id), Some(concept_id), Some(state)) = (
            entry.get("bundle_id").and_then(serde_json::Value::as_i64),
            entry.get("concept_id").and_then(serde_json::Value::as_str),
            entry
                .pointer("/freshness/state")
                .and_then(serde_json::Value::as_str),
        ) else {
            continue;
        };
        entries.push(PinnedEntry {
            bundle_id,
            concept_id: concept_id.to_owned(),
            state: state.to_owned(),
            reasons: entry
                .pointer("/freshness/reasons")
                .and_then(serde_json::Value::as_array)
                .map(|reasons| {
                    reasons
                        .iter()
                        .filter_map(|r| r.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        });
    }
    Ok((name, pinned, entries))
}

/// Compare the lockfile of a downloaded plugin with the live catalog,
/// answering `current`, `stale`, `retired`, or `unknown` with reasons. One
/// comparison statement runs after a capability probe, so the live state is
/// read under one snapshot; a catalog without the freshness surface is
/// compared on the pinned sync hashes alone.
///
/// # Errors
///
/// The lock content is not a lockfile, or a catalog query fails.
pub async fn check_plugin_freshness<C: GenericClient>(
    client: &C,
    lock_content: &str,
) -> Result<PluginCheck> {
    let (name, pinned, entries) = parse_lock(lock_content)?;
    if pinned.is_empty() {
        return Ok(PluginCheck {
            name,
            status: PluginStatus::Unknown,
            reasons: vec!["the lockfile pins no bundles".to_owned()],
            bundles: Vec::new(),
        });
    }
    let capabilities = crate::selection::capabilities(client).await?;
    let extended = capabilities.freshness && capabilities.catalog_generation;
    let ids: Vec<i64> = pinned.iter().map(|p| p.id).collect();
    let sql = if extended {
        "SELECT b.id, b.sync_hash, b.catalog_generation, b.enabled, b.retired_at IS NOT NULL,
                ef.state, coalesce(ef.reasons, '{}')
         FROM pgokf.bundles b
         LEFT JOIN pgokf.effective_freshness ef
                ON ef.bundle_id = b.id AND ef.scope_kind = 'bundle'
         WHERE b.id = ANY($1)"
    } else {
        "SELECT b.id, b.sync_hash, NULL::bigint, b.enabled, b.retired_at IS NOT NULL,
                NULL::text, '{}'::text[]
         FROM pgokf.bundles b WHERE b.id = ANY($1)"
    };
    let rows = client
        .query(sql, &[&ids])
        .await
        .context("comparing the lockfile with the catalog")?;
    let live: Vec<(i64, LiveBundle)> = rows
        .iter()
        .map(|row| {
            Ok((
                row.try_get(0)?,
                LiveBundle {
                    sync_hash: row.try_get(1)?,
                    catalog_generation: row.try_get(2)?,
                    enabled: row.try_get(3)?,
                    retired: row.try_get(4)?,
                    freshness_state: row.try_get(5)?,
                    freshness_reasons: row.try_get(6)?,
                },
            ))
        })
        .collect::<Result<_>>()?;

    let mut checks = Vec::with_capacity(pinned.len());
    for pin in &pinned {
        let row = live.iter().find(|(id, _)| *id == pin.id).map(|(_, b)| b);
        let evidence: Vec<&PinnedEntry> =
            entries.iter().filter(|e| e.bundle_id == pin.id).collect();
        checks.push(compare_bundle(pin, row, &evidence));
    }
    Ok(aggregate(name, checks))
}

/// One bundle's side of the comparison: the lockfile's pins and the
/// build-time freshness evidence of its entries against the live row (or
/// its absence).
///
/// `current` needs live confirmation: a matched catalog generation or a
/// live `fresh` state. A matched sync hash alone is not one - on a catalog
/// that predates the freshness surface (or records no state) a matching
/// hash is exactly the case the contract calls `unknown` - and a pin whose
/// live counterpart is absent is a gap, never silent confirmation. The
/// lockfile's own entries are evidence too: content the catalog already
/// reported as not fresh when the plugin was built marks the artifact
/// `stale`, however well the bundle pins match, and only rebuilding after
/// reconciliation clears that (the check compares the bytes that shipped).
#[allow(clippy::too_many_lines)]
fn compare_bundle(
    pin: &PinnedBundle,
    live: Option<&LiveBundle>,
    entries: &[&PinnedEntry],
) -> BundleCheck {
    let mut check = BundleCheck {
        bundle_id: pin.id,
        name: pin.name.clone(),
        status: PluginStatus::Current,
        reasons: Vec::new(),
    };
    let Some(live) = live else {
        check.status = PluginStatus::Retired;
        check
            .reasons
            .push("the bundle is gone from the catalog".to_owned());
        return check;
    };
    if live.retired {
        check.status = PluginStatus::Retired;
        check.reasons.push("the bundle is retired".to_owned());
        return check;
    }
    if !live.enabled {
        check.status = PluginStatus::Retired;
        check.reasons.push("the bundle is disabled".to_owned());
        return check;
    }
    let mut drift = Vec::new();
    let mut gaps = Vec::new();
    let mut confirmed = false;
    match (pin.catalog_generation, live.catalog_generation) {
        (Some(pinned), Some(live)) if pinned != live => drift.push(format!(
            "the catalog generation advanced from {pinned} to {live}"
        )),
        (Some(_), Some(_)) => confirmed = true,
        (Some(_), None) => gaps.push(
            "the catalog records no catalog generation to compare the pin against".to_owned(),
        ),
        (None, _) => {}
    }
    match (&pin.sync_hash, &live.sync_hash) {
        (Some(pinned), Some(live)) if pinned != live => {
            drift.push("the bundle's content changed".to_owned());
        }
        (Some(_), None) => {
            gaps.push("the catalog records no sync hash to compare the pin against".to_owned());
        }
        _ => {}
    }
    match live.freshness_state.as_deref() {
        None => {}
        Some("fresh") => confirmed = true,
        Some("retired") => {
            check.status = PluginStatus::Retired;
            check
                .reasons
                .push("the bundle's freshness is retired".to_owned());
            return check;
        }
        Some(other) => {
            drift.push(format!(
                "the bundle's freshness is {other}{}",
                if live.freshness_reasons.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", live.freshness_reasons.join(", "))
                }
            ));
        }
    }
    // The build-time evidence the lockfile itself records: content that was
    // already not fresh when the plugin shipped. A matched bundle pin does
    // not clear it - the state predates the build - so the artifact stays
    // stale until it is rebuilt from reconciled content.
    for entry in entries {
        if entry.state != "fresh" && entry.state != "unknown" {
            drift.push(format!(
                "concept {} was already {} when the plugin was built{}; rebuild to pick up the \
                 reconciled content",
                entry.concept_id,
                entry.state,
                if entry.reasons.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", entry.reasons.join(", "))
                }
            ));
        }
    }
    if !drift.is_empty() {
        check.status = PluginStatus::Stale;
        check.reasons = drift;
        return check;
    }
    if !confirmed {
        check.status = PluginStatus::Unknown;
        check.reasons = if gaps.is_empty() {
            if pin.catalog_generation.is_none() && pin.sync_hash.is_none() {
                vec!["the lockfile pins nothing comparable for this bundle".to_owned()]
            } else {
                vec![
                    "the pins match, but the catalog offers no live freshness state or catalog \
                     generation to confirm them"
                        .to_owned(),
                ]
            }
        } else {
            gaps
        };
        return check;
    }
    check.reasons = gaps;
    check
}

/// The plugin's status is the worst of its bundles': retired, then stale,
/// then unknown, then current.
fn aggregate(name: Option<String>, bundles: Vec<BundleCheck>) -> PluginCheck {
    let status = bundles
        .iter()
        .map(|b| b.status)
        .max_by_key(|s| match s {
            PluginStatus::Current => 0,
            PluginStatus::Unknown => 1,
            PluginStatus::Stale => 2,
            PluginStatus::Retired => 3,
        })
        .unwrap_or(PluginStatus::Unknown);
    let mut reasons = Vec::new();
    for bundle in &bundles {
        for reason in &bundle.reasons {
            reasons.push(format!("bundle {}: {reason}", bundle.bundle_id));
        }
    }
    PluginCheck {
        name,
        status,
        reasons,
        bundles,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pinned(id: i64) -> PinnedBundle {
        PinnedBundle {
            id,
            name: Some(format!("bundle-{id}")),
            sync_hash: Some("abc".to_owned()),
            catalog_generation: Some(7),
        }
    }

    fn live(state: Option<&str>) -> LiveBundle {
        LiveBundle {
            sync_hash: Some("abc".to_owned()),
            catalog_generation: Some(7),
            enabled: true,
            retired: false,
            freshness_state: state.map(str::to_owned),
            freshness_reasons: Vec::new(),
        }
    }

    fn entry(id: &str, state: &str) -> PinnedEntry {
        PinnedEntry {
            bundle_id: 1,
            concept_id: id.to_owned(),
            state: state.to_owned(),
            reasons: vec!["dependency_changed".to_owned()],
        }
    }

    #[test]
    fn parse_lock_reads_the_catalog_bundles() {
        // Arrange
        let lock = r#"{"version":1,"name":"ops","catalog":{"version":"0.3.0","sql_version":"0.3.0","bundles":[{"id":3,"name":"ops","sync_hash":"h","last_synced_at":null,"catalog_generation":12}]}}"#;

        // Act
        let (name, bundles, entries) = parse_lock(lock).expect("a lockfile");

        // Assert
        assert_eq!(name.as_deref(), Some("ops"));
        assert_eq!(
            bundles,
            [PinnedBundle {
                id: 3,
                name: Some("ops".to_owned()),
                sync_hash: Some("h".to_owned()),
                catalog_generation: Some(12),
            }]
        );
        assert!(entries.is_empty(), "no entries block, no evidence");
        assert!(parse_lock("{}").is_err());
        assert!(parse_lock("not json").is_err());
    }

    #[test]
    fn parse_lock_reads_the_entries_build_time_freshness_evidence() {
        // Arrange: an engaged lockfile with one stale and one fresh entry.
        let lock = r#"{"catalog":{"bundles":[{"id":1}]},
            "entries":[
                {"bundle_id":1,"concept_id":"seed","freshness":{"state":"stale","reasons":["dependency_changed"]}},
                {"bundle_id":1,"concept_id":"ok","freshness":{"state":"fresh","reasons":[]}},
                {"bundle_id":1,"concept_id":"legacy"}
            ]}"#;

        // Act
        let (_, _, entries) = parse_lock(lock).expect("a lockfile");

        // Assert: entries without a recorded state carry no evidence.
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].concept_id, "seed");
        assert_eq!(entries[0].state, "stale");
        assert_eq!(entries[0].reasons, ["dependency_changed"]);
        assert_eq!(entries[1].state, "fresh");
    }

    #[test]
    fn parse_lock_tolerates_missing_optional_pins() {
        // Arrange: a lockfile from before catalog generations existed.
        let lock = r#"{"catalog":{"bundles":[{"id":1,"name":"a","sync_hash":"h","last_synced_at":null}]}}"#;

        // Act
        let (_, bundles, _) = parse_lock(lock).expect("a legacy lockfile");

        // Assert
        assert_eq!(bundles[0].catalog_generation, None);
        assert_eq!(bundles[0].sync_hash.as_deref(), Some("h"));
    }

    #[test]
    fn compare_bundle_reports_a_missing_or_retired_bundle_as_retired() {
        // Arrange / Act
        let gone = compare_bundle(&pinned(1), None, &[]);
        let mut retired_row = live(Some("fresh"));
        retired_row.retired = true;
        let retired = compare_bundle(&pinned(1), Some(&retired_row), &[]);

        // Assert
        assert_eq!(gone.status, PluginStatus::Retired);
        assert!(gone.reasons[0].contains("gone"));
        assert_eq!(retired.status, PluginStatus::Retired);
    }

    #[test]
    fn a_bundle_confirmed_by_generation_or_live_fresh_state_is_current() {
        // Arrange / Act / Assert
        assert_eq!(
            compare_bundle(&pinned(1), Some(&live(Some("fresh"))), &[]).status,
            PluginStatus::Current
        );
        // The generation match alone confirms, even with no live state row.
        assert_eq!(
            compare_bundle(&pinned(1), Some(&live(None)), &[]).status,
            PluginStatus::Current
        );
    }

    #[test]
    fn a_build_time_stale_entry_is_stale_even_when_every_pin_matches() {
        // Arrange: the exact W5 reproduction - fresh bundle at the pinned
        // generation, one concept the catalog already reported stale when
        // the plugin was built.
        let evidence = entry("seed", "stale");

        // Act
        let check = compare_bundle(&pinned(1), Some(&live(Some("fresh"))), &[&evidence]);

        // Assert
        assert_eq!(check.status, PluginStatus::Stale);
        assert!(
            check.reasons[0].contains("seed was already stale when the plugin was built"),
            "{:?}",
            check.reasons
        );
        assert!(check.reasons[0].contains("dependency_changed"));
        // A fresh or unknown entry does not mark the artifact.
        let fresh = entry("seed", "fresh");
        let unknown = entry("seed", "unknown");
        assert_eq!(
            compare_bundle(&pinned(1), Some(&live(Some("fresh"))), &[&fresh, &unknown]).status,
            PluginStatus::Current
        );
    }

    #[test]
    fn a_matching_hash_without_live_state_is_unknown_not_current() {
        // Arrange: a legacy lockfile (hash pin only) against a catalog with
        // no freshness surface (or no state row): the hash matches, and
        // nothing live can confirm it - the documented unknown case.
        let mut pin = pinned(1);
        pin.catalog_generation = None;
        let mut legacy = live(None);
        legacy.catalog_generation = None;

        // Act
        let matching = compare_bundle(&pin, Some(&legacy), &[]);
        legacy.sync_hash = Some("different".to_owned());
        let changed = compare_bundle(&pin, Some(&legacy), &[]);

        // Assert: drift is still detected, but a bare hash match cannot
        // certify currency.
        assert_eq!(matching.status, PluginStatus::Unknown);
        assert!(
            matching.reasons[0].contains("no live freshness state"),
            "{:?}",
            matching.reasons
        );
        assert_eq!(changed.status, PluginStatus::Stale);
    }

    #[test]
    fn a_pin_without_a_live_counterpart_is_not_silently_current() {
        // Arrange: the lockfile pins a generation; the live row records
        // none, so the guard that skipped the comparison reported current.
        let mut gap = live(None);
        gap.catalog_generation = None;

        // Act
        let check = compare_bundle(&pinned(1), Some(&gap), &[]);

        // Assert
        assert_eq!(check.status, PluginStatus::Unknown);
        assert!(
            check.reasons[0].contains("no catalog generation to compare"),
            "{:?}",
            check.reasons
        );
    }

    #[test]
    fn aggregate_takes_the_worst_status() {
        // Arrange
        let bundle = |status| BundleCheck {
            bundle_id: 1,
            name: None,
            status,
            reasons: vec!["why".to_owned()],
        };

        // Act / Assert
        assert_eq!(
            aggregate(None, vec![bundle(PluginStatus::Current)]).status,
            PluginStatus::Current
        );
        assert_eq!(
            aggregate(
                None,
                vec![bundle(PluginStatus::Current), bundle(PluginStatus::Unknown)]
            )
            .status,
            PluginStatus::Unknown
        );
        assert_eq!(
            aggregate(
                None,
                vec![bundle(PluginStatus::Unknown), bundle(PluginStatus::Retired)]
            )
            .status,
            PluginStatus::Retired
        );
        let check = aggregate(
            Some("ops".to_owned()),
            vec![bundle(PluginStatus::Stale), bundle(PluginStatus::Unknown)],
        );
        assert_eq!(check.status, PluginStatus::Stale);
        assert_eq!(check.reasons.len(), 2);
    }
}
