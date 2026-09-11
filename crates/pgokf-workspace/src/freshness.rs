// SPDX-License-Identifier: AGPL-3.0-only
//! Checking a built plugin's lockfile against the live catalog
//! (`check_workspace_plugin_freshness`, spec §4.8): a downloaded plugin
//! cannot update itself, so its `okf-workspace.lock` - which pins each
//! bundle's sync hash and, on catalogs that have them, the catalog
//! generation and freshness state - is compared with the catalog as it is
//! now, answering `current`, `stale`, `retired`, or `unknown` with reasons.

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use tokio_postgres::GenericClient;

/// The freshness of a built plugin - or of one of its pinned bundles -
/// against the live catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginStatus {
    /// Everything the lockfile pins still holds.
    Current,
    /// The catalog moved on: a pinned generation or sync hash no longer
    /// matches, or the bundle's effective freshness is not `fresh`.
    Stale,
    /// A pinned bundle is retired, disabled, or gone.
    Retired,
    /// The comparison cannot say (the lockfile pins nothing comparable, or
    /// the catalog predates the freshness surface and the hash matches).
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

/// The plugin name and pinned bundles of an `okf-workspace.lock` document.
///
/// # Errors
///
/// The content is not JSON, or not a lockfile (no `catalog.bundles`).
fn parse_lock(content: &str) -> Result<(Option<String>, Vec<PinnedBundle>)> {
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
    Ok((name, pinned))
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
    let (name, pinned) = parse_lock(lock_content)?;
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

    let mut checks = Vec::with_capacity(pinned.len());
    for pin in &pinned {
        let row = rows
            .iter()
            .find(|row| row.try_get::<_, i64>(0).is_ok_and(|id| id == pin.id));
        checks.push(compare_bundle(pin, row));
    }
    Ok(aggregate(name, checks))
}

/// One bundle's side of the comparison: the lockfile's pins against the
/// live row (or its absence).
fn compare_bundle(pin: &PinnedBundle, row: Option<&tokio_postgres::Row>) -> BundleCheck {
    let mut check = BundleCheck {
        bundle_id: pin.id,
        name: pin.name.clone(),
        status: PluginStatus::Current,
        reasons: Vec::new(),
    };
    let Some(row) = row else {
        check.status = PluginStatus::Retired;
        check
            .reasons
            .push("the bundle is gone from the catalog".to_owned());
        return check;
    };
    let live_hash: Option<String> = row.try_get(1).unwrap_or(None);
    let live_generation: Option<i64> = row.try_get(2).unwrap_or(None);
    let enabled: bool = row.try_get(3).unwrap_or(false);
    let retired: bool = row.try_get(4).unwrap_or(false);
    let state: Option<String> = row.try_get(5).unwrap_or(None);
    let state_reasons: Vec<String> = row.try_get(6).unwrap_or_default();

    if retired {
        check.status = PluginStatus::Retired;
        check.reasons.push("the bundle is retired".to_owned());
        return check;
    }
    if !enabled {
        check.status = PluginStatus::Retired;
        check.reasons.push("the bundle is disabled".to_owned());
        return check;
    }
    let mut drift = Vec::new();
    if let (Some(pinned), Some(live)) = (pin.catalog_generation, live_generation)
        && pinned != live
    {
        drift.push(format!(
            "the catalog generation advanced from {pinned} to {live}"
        ));
    }
    if let (Some(pinned), Some(live)) = (&pin.sync_hash, &live_hash)
        && pinned != live
    {
        drift.push("the bundle's content changed".to_owned());
    }
    match state.as_deref() {
        None | Some("fresh") => {}
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
                if state_reasons.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", state_reasons.join(", "))
                }
            ));
        }
    }
    if !drift.is_empty() {
        check.status = PluginStatus::Stale;
        check.reasons = drift;
        return check;
    }
    if pin.catalog_generation.is_none() && pin.sync_hash.is_none() && state.is_none() {
        check.status = PluginStatus::Unknown;
        check
            .reasons
            .push("the lockfile pins nothing comparable for this bundle".to_owned());
    }
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

    #[test]
    fn parse_lock_reads_the_catalog_bundles() {
        // Arrange
        let lock = r#"{"version":1,"name":"ops","catalog":{"version":"0.3.0","sql_version":"0.3.0","bundles":[{"id":3,"name":"ops","sync_hash":"h","last_synced_at":null,"catalog_generation":12}]}}"#;

        // Act
        let (name, bundles) = parse_lock(lock).expect("a lockfile");

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
        assert!(parse_lock("{}").is_err());
        assert!(parse_lock("not json").is_err());
    }

    #[test]
    fn parse_lock_tolerates_missing_optional_pins() {
        // Arrange: a lockfile from before catalog generations existed.
        let lock = r#"{"catalog":{"bundles":[{"id":1,"name":"a","sync_hash":"h","last_synced_at":null}]}}"#;

        // Act
        let (_, bundles) = parse_lock(lock).expect("a legacy lockfile");

        // Assert
        assert_eq!(bundles[0].catalog_generation, None);
        assert_eq!(bundles[0].sync_hash.as_deref(), Some("h"));
    }

    #[test]
    fn compare_bundle_reports_a_missing_or_retired_bundle_as_retired() {
        // Arrange / Act
        let gone = compare_bundle(&pinned(1), None);

        // Assert
        assert_eq!(gone.status, PluginStatus::Retired);
        assert!(gone.reasons[0].contains("gone"));
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
