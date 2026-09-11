// SPDX-License-Identifier: AGPL-3.0-only
//! What goes into a plugin: the selectors of spec §21.1 resolved through the
//! reader API, so tenant scope, visibility, retirement, and non-disclosure
//! are the database's decisions, never this crate's.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio_postgres::GenericClient;
use tokio_postgres::types::ToSql;

/// The largest selection one build materializes.
pub const MAX_CONCEPTS: usize = 500;
/// The most members one skill package contributes to a build. A package is
/// copied whole, so without this one hostile package makes the row limit
/// meaningless.
pub const MAX_PACKAGE_MEMBERS: usize = 2_000;
/// The most content bytes one build loads, across every concept and every
/// package member. The row limit bounds how many things are loaded, not how
/// big they are; this bounds the build, which is assembled and zipped in
/// memory and is reachable without signing in when the UI has no identity
/// mode.
pub const MAX_CONTENT_BYTES: u64 = 256 * 1024 * 1024;
/// The default when a caller gives no limit.
pub const DEFAULT_LIMIT: usize = 100;

/// What a build does with concepts the catalog reports as not fresh
/// (spec §4.8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StalePolicy {
    /// Keep stale concepts and label them: a banner or an adjacent warning
    /// file per stale concept, a top-level `FRESHNESS.md`, and the states
    /// recorded in the manifest and lockfile. Exact bytes never change.
    #[default]
    Warn,
    /// Drop stale concepts before materialization, refusing the build when
    /// an exact pick, a seed, or a required closure node is stale, when no
    /// fresh concepts remain, or when a requested closure would break.
    Exclude,
}

impl StalePolicy {
    /// The identifier used in manifests and tool arguments.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Exclude => "exclude",
        }
    }

    /// Parse `warn` or `exclude`.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        match id.trim() {
            "warn" => Some(Self::Warn),
            "exclude" => Some(Self::Exclude),
            _ => None,
        }
    }
}

/// Which way a seed closure walks typed relationships.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Source to target (the default).
    #[default]
    Outbound,
    /// Target to source.
    Inbound,
    /// Both ways.
    Both,
}

impl Direction {
    /// The identifier `pgokf.concept_relationship_neighbors` accepts.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Outbound => "outbound",
            Self::Inbound => "inbound",
            Self::Both => "both",
        }
    }

    /// Parse `outbound`, `inbound`, or `both`.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        match id.trim() {
            "outbound" => Some(Self::Outbound),
            "inbound" => Some(Self::Inbound),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}

/// The default hop bound of a seed closure (the extension's own default).
pub const DEFAULT_HOPS: usize = 2;
/// The most hops one closure walks, client-side; the extension caps
/// `max_hops` further through `pgokf.max_graph_hops`.
pub const MAX_HOPS: usize = 8;

/// The selectors of one `include` entry: every field narrows, and an empty
/// selection (no selector at all) is refused rather than exporting a catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    /// Everything visible, bounded by the limit: the scope a selection
    /// starts from when no bundle narrows it, so "all bundles, narrowed by
    /// nothing" is expressible (a selection with no selector at all is
    /// empty and refused).
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub bundle_ids: Vec<i64>,
    /// Concept ids, within the selected bundles (or any bundle).
    #[serde(default)]
    pub concept_ids: Vec<String>,
    /// All-of tag containment.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Any-of concept types.
    #[serde(default)]
    pub types: Vec<String>,
    /// Full-text query (websearch syntax) ranked by `concept_search`.
    #[serde(default)]
    pub query: Option<String>,
    /// Only `human-reviewed` / `machine-confirmed` concepts (spec policy
    /// `trust: verified-only`).
    #[serde(default)]
    pub verified_only: bool,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Specific files picked by identity. Unlike the selectors above, which
    /// narrow the catalog, picks *add*: a picked concept is included whether
    /// or not it matches the other selectors, and picks are never cut by the
    /// limit.
    #[serde(default)]
    pub picks: Vec<ConceptRef>,
    /// What to do with concepts the catalog reports as not fresh.
    #[serde(default)]
    pub stale_policy: StalePolicy,
    /// Closure seeds: catalog concept refs the build starts from and expands
    /// through the typed relationships of `pgokf.current_relationships`.
    #[serde(default)]
    pub seeds: Vec<ConceptRef>,
    /// Namespaced relationship types the closure follows (caller-supplied;
    /// empty follows every type). The builder holds no vocabulary of its own.
    #[serde(default)]
    pub relation_types: Vec<String>,
    /// Which way the closure walks relationships.
    #[serde(default)]
    pub direction: Direction,
    /// The closure's hop bound (default [`DEFAULT_HOPS`], capped at
    /// [`MAX_HOPS`]).
    #[serde(default)]
    pub hops: Option<usize>,
    /// Refuse the build when the closure cannot be completed: an unresolved
    /// relationship target on a traversed edge, or a closure node the stale
    /// policy would drop.
    #[serde(default)]
    pub require_closure: bool,
}

/// One concept named by identity: `(bundle_id, concept_id)`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConceptRef {
    pub bundle_id: i64,
    pub concept_id: String,
}

impl ConceptRef {
    /// Parse the `bundle_id:concept_id` form used in forms and manifests.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (bundle, id) = text.trim().split_once(':')?;
        let bundle_id = bundle.trim().parse::<i64>().ok()?;
        let concept_id = id.trim();
        (bundle_id > 0 && !concept_id.is_empty()).then(|| Self {
            bundle_id,
            concept_id: concept_id.to_owned(),
        })
    }
}

impl ConceptRef {
    /// Split a list of picks (one per line or comma-separated) into the
    /// well-formed picks, duplicates folded in first-seen order, and the
    /// malformed entries verbatim; blank entries are skipped. A form can keep
    /// previewing with the good picks while it reports the bad ones.
    #[must_use]
    pub fn parse_entries(text: &str) -> (Vec<Self>, Vec<String>) {
        let mut picks: Vec<Self> = Vec::new();
        let mut malformed = Vec::new();
        for entry in text
            .split(['\n', ','])
            .map(str::trim)
            .filter(|e| !e.is_empty())
        {
            match Self::parse(entry) {
                Some(pick) if !picks.contains(&pick) => picks.push(pick),
                Some(_) => {}
                None => malformed.push(entry.to_owned()),
            }
        }
        (picks, malformed)
    }

    /// The strict form of [`Self::parse_entries`]: every entry must parse.
    ///
    /// # Errors
    ///
    /// The first entry that is not `bundle_id:concept_id`.
    pub fn parse_list(text: &str) -> Result<Vec<Self>> {
        let (picks, malformed) = Self::parse_entries(text);
        match malformed.first() {
            Some(entry) => Err(anyhow!(
                "picked file {entry:?} is not of the form bundle_id:concept_id"
            )),
            None => Ok(picks),
        }
    }
}

impl std::fmt::Display for ConceptRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.bundle_id, self.concept_id)
    }
}

impl Selection {
    /// `true` when nothing at all is selected: no narrowing selector, no
    /// pick, and no seed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.has_filters() && self.picks.is_empty() && self.seeds.is_empty()
    }

    /// `true` when at least one narrowing selector is set (a pick alone is
    /// not one: it names files without narrowing the catalog).
    #[must_use]
    pub fn has_filters(&self) -> bool {
        self.all
            || !(self.bundle_ids.is_empty()
                && self.concept_ids.is_empty()
                && self.tags.is_empty()
                && self.types.is_empty()
                && self.query.as_deref().is_none_or(|q| q.trim().is_empty()))
    }

    /// The effective row limit, bounded to [`MAX_CONCEPTS`].
    #[must_use]
    pub fn effective_limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_CONCEPTS)
    }

    /// The effective closure hop bound, bounded to [`MAX_HOPS`].
    #[must_use]
    pub fn effective_hops(&self) -> usize {
        self.hops.unwrap_or(DEFAULT_HOPS).clamp(1, MAX_HOPS)
    }

    /// A short human description ("type Runbook, tagged a, b, in bundle 2").
    #[must_use]
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(q) = self.query.as_deref().filter(|q| !q.trim().is_empty()) {
            parts.push(format!("matching \u{201c}{}\u{201d}", q.trim()));
        }
        if !self.types.is_empty() {
            parts.push(format!("of type {}", self.types.join(" or ")));
        }
        if !self.tags.is_empty() {
            parts.push(format!("tagged {}", self.tags.join(", ")));
        }
        if !self.concept_ids.is_empty() {
            parts.push(format!("{} named concept(s)", self.concept_ids.len()));
        }
        if !self.bundle_ids.is_empty() {
            let ids: Vec<String> = self.bundle_ids.iter().map(ToString::to_string).collect();
            parts.push(format!("in bundle(s) {}", ids.join(", ")));
        }
        if self.verified_only {
            parts.push("verified only".to_owned());
        }
        if !self.picks.is_empty() {
            let picked = format!(
                "{} picked file{}",
                self.picks.len(),
                if self.picks.len() == 1 { "" } else { "s" }
            );
            if parts.is_empty() {
                parts.push(picked);
            } else {
                parts.push(format!("plus {picked}"));
            }
        }
        if !self.seeds.is_empty() {
            let mut closure = format!(
                "{} seed{} within {} hop{}",
                self.seeds.len(),
                if self.seeds.len() == 1 { "" } else { "s" },
                self.effective_hops(),
                if self.effective_hops() == 1 { "" } else { "s" }
            );
            if !self.relation_types.is_empty() {
                let _ = write!(closure, " of type {}", self.relation_types.join(", "));
            }
            if self.direction != Direction::Outbound {
                let _ = write!(closure, " ({})", self.direction.id());
            }
            if parts.is_empty() {
                parts.push(format!("the closure of {closure}"));
            } else {
                parts.push(format!("plus the closure of {closure}"));
            }
        }
        if parts.is_empty() {
            if self.all {
                "everything in the catalog".to_owned()
            } else {
                "nothing selected".to_owned()
            }
        } else if self.all && self.bundle_ids.is_empty() {
            format!("everything {}", parts.join(", "))
        } else {
            parts.join(", ")
        }
    }
}

/// The catalog's effective freshness for one concept, read from
/// `pgokf.effective_freshness` inside the build's snapshot: a concept-scope
/// override when one is recorded, otherwise the bundle's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Freshness {
    /// `fresh`, `stale`, `reconciling`, `blocked`, `retired`, or `unknown`
    /// (a catalog without the freshness surface).
    pub state: String,
    pub reasons: Vec<String>,
    /// Which recorded scope set the state: `concept` or `bundle`.
    pub scope: String,
    pub stale_since: Option<String>,
    /// The producer's opaque observed revision.
    pub observed_revision: Option<String>,
    /// The producer's opaque materialized-input revision.
    pub indexed_revision: Option<String>,
    /// The catalog generation the materialization covers.
    pub published_revision: Option<String>,
    /// The bundle's live catalog generation in the build's snapshot.
    pub catalog_generation: Option<i64>,
    pub last_reconciled_at: Option<String>,
}

impl Freshness {
    /// The state of a concept on a catalog without the freshness surface, or
    /// one with no recorded row: nothing is claimed, nothing is warned about.
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            state: "unknown".to_owned(),
            reasons: Vec::new(),
            scope: String::new(),
            stale_since: None,
            observed_revision: None,
            indexed_revision: None,
            published_revision: None,
            catalog_generation: None,
            last_reconciled_at: None,
        }
    }

    /// `true` when the catalog reports the concept as not fresh. `unknown`
    /// is not stale: a catalog without the freshness surface must not turn
    /// every build into a warning.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.state != "fresh" && self.state != "unknown"
    }
}

impl Default for Freshness {
    fn default() -> Self {
        Self::unknown()
    }
}

/// How a concept entered the selection. The narrowing selectors are the
/// default route and set no flag; the flags mark the additive routes, which
/// the stale policy treats as required (a stale one refuses an `exclude`
/// build rather than being dropped).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Origin {
    /// Named by identity in `picks`.
    pub pick: bool,
    /// A closure seed.
    pub seed: bool,
    /// Reached by walking typed relationships from a seed.
    pub closure: bool,
}

impl Origin {
    /// The role a stale concept plays in a refusal or exclusion listing.
    #[must_use]
    pub fn role(&self, require_closure: bool) -> &'static str {
        if self.pick {
            "pick"
        } else if self.seed {
            "seed"
        } else if self.closure && require_closure {
            "required-closure"
        } else if self.closure {
            "closure"
        } else {
            "selected"
        }
    }

    /// `true` when the stale policy must not silently drop this concept.
    #[must_use]
    pub fn required(&self, require_closure: bool) -> bool {
        self.pick || self.seed || (self.closure && require_closure)
    }
}

/// One concept as the builder materializes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConceptRecord {
    pub bundle_id: i64,
    pub bundle_name: String,
    pub concept_id: String,
    pub path: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub concept_type: Option<String>,
    pub tags: Vec<String>,
    pub file_hash: String,
    pub trust_tier: String,
    pub status: String,
    /// `true` when `bytes` are the exact stored source; `false` when the
    /// catalog keeps no source and the document was reconstructed from the
    /// indexed text.
    pub exact: bool,
    /// The document as written to the workspace (empty until sources load).
    /// For a skill this is the exact `SKILL.md`; for a package resource its
    /// exact bytes.
    #[serde(skip)]
    pub bytes: Vec<u8>,
    /// Present when the concept is an Agent Skills package manifest: the
    /// package identity from `pgokf.skills`, and its resources with their
    /// bytes once sources load. The whole package is materialized under one
    /// directory, byte for byte.
    pub package: Option<PackageRecord>,
    /// Present when the concept is a package resource (a script, reference,
    /// or asset) selected on its own; dropped when its package is selected
    /// too, because the package already carries it.
    pub resource: Option<ResourceRecord>,
    /// The catalog's effective freshness for this concept in the build's
    /// snapshot ([`Freshness::unknown`] until resolved, and on a catalog
    /// without the freshness surface).
    #[serde(default)]
    pub freshness: Freshness,
    /// How the concept entered the selection.
    #[serde(default)]
    pub origin: Origin,
}

/// A skill package: what `pgokf.skills` records plus, after loading, every
/// resource the package owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageRecord {
    /// The Agent Skills `name` (the directory the harness expects).
    pub name: String,
    /// Bundle-relative package directory (`""` for a root package).
    pub root: String,
    /// The catalog's package hash over the manifest and every member.
    pub hash: String,
    /// The package's scripts, references, and assets, in path order.
    pub resources: Vec<ResourceFile>,
}

/// One file of a skill package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceFile {
    /// The resource's own concept id (its bundle-relative path).
    pub concept_id: String,
    /// `script`, `reference`, or `asset`.
    pub class: String,
    /// Package-relative path (`scripts/check.sh`).
    pub path: String,
    /// SHA-256 of the exact bytes, as the catalog records it.
    pub sha256: String,
    /// The catalog's BLAKE3 `file_hash` of the resource's own concept.
    pub file_hash: String,
    /// The exact bytes (empty until sources load).
    #[serde(skip)]
    pub bytes: Vec<u8>,
}

impl ResourceFile {
    /// Scripts keep their executable bit in the workspace.
    #[must_use]
    pub fn is_script(&self) -> bool {
        self.class == "script"
    }
}

/// A package resource selected as a concept of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceRecord {
    /// `script`, `reference`, or `asset`.
    pub class: String,
    /// Package-relative path.
    pub source_path: String,
    /// The owning skill's concept id.
    pub package_concept_id: String,
    /// SHA-256 of the exact bytes, as the catalog records it.
    pub sha256: String,
}

/// The catalog identity a lockfile records: versions and the state of each
/// included bundle, so an unchanged catalog reproduces the same tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub version: String,
    pub sql_version: String,
    pub bundles: Vec<BundleState>,
}

/// One bundle's identity in the snapshot. The freshness fields stay `None`
/// (and out of the serialized lockfile) unless the build engaged the
/// freshness surface, so a build with nothing stale is byte-identical to one
/// from before the surface existed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BundleState {
    pub id: i64,
    pub name: String,
    pub sync_hash: Option<String>,
    pub last_synced_at: Option<String>,
    /// The bundle's catalog generation in the build's snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_generation: Option<i64>,
    /// The bundle-scope effective freshness state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness_state: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub freshness_reasons: Vec<String>,
    /// The bundle's embedding contract evidence, as the catalog records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_contract: Option<serde_json::Value>,
}

const TRUSTED_TIERS: [&str; 2] = ["human-reviewed", "machine-confirmed"];

/// The one statement a selection resolves through. The picks join is a
/// union on top of the narrowing selectors (`$11` says whether any is set,
/// so a picks-only selection does not match the whole catalog); the trust
/// filter (`$5`) gates both, which is why it sits outside the union's
/// parentheses.
const RESOLVE_SQL: &str = "SELECT c.bundle_id, coalesce(b.name, regexp_replace(b.path, '^.*/', '')),
                      c.id, c.path, c.title, c.description, c.type, coalesce(c.tags, '{}'),
                      c.file_hash, coalesce(p.trust_tier, 'unverified'), coalesce(p.status, 'stable'),
                      r.ord,
                      sk.agent_skill->>'name', sk.package_root, sk.package_hash,
                      CASE WHEN s.concept_id IS NOT NULL THEN 'script'
                           WHEN d.source_path LIKE 'assets/%' THEN 'asset'
                           WHEN d.concept_id IS NOT NULL THEN 'reference' END,
                      coalesce(s.source_path, d.source_path),
                      coalesce(s.package_concept_id, d.package_concept_id),
                      coalesce(s.executable_sha256, d.content_sha256),
                      (pk.b IS NOT NULL) AS picked
               FROM pgokf.concepts c
               JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
               LEFT JOIN pgokf.concept_provenance p
                      ON p.bundle_id = c.bundle_id AND p.concept_id = c.id
               LEFT JOIN pgokf.skills sk ON sk.bundle_id = c.bundle_id AND sk.concept_id = c.id
               LEFT JOIN pgokf.scripts s ON s.bundle_id = c.bundle_id AND s.concept_id = c.id
               LEFT JOIN pgokf.reference_documents d
                      ON d.bundle_id = c.bundle_id AND d.concept_id = c.id
               LEFT JOIN (
                   SELECT o.b, o.id, min(o.ord) AS ord
                   FROM ROWS FROM (unnest($6::bigint[]), unnest($7::text[])) WITH ORDINALITY AS o(b, id, ord)
                   GROUP BY o.b, o.id
               ) r ON r.b = c.bundle_id AND r.id = c.id
               LEFT JOIN (
                   SELECT DISTINCT q.b, q.id
                   FROM unnest($9::bigint[], $10::text[]) AS q(b, id)
               ) pk ON pk.b = c.bundle_id AND pk.id = c.id
               WHERE (pk.b IS NOT NULL
                      OR ($11
                          AND ($1::bigint[] IS NULL OR c.bundle_id = ANY($1))
                          AND ($2::text[] IS NULL OR c.type = ANY($2))
                          AND ($3::text[] IS NULL OR c.tags @> $3)
                          AND ($4::text[] IS NULL OR c.id = ANY($4))
                          AND ($6::bigint[] IS NULL OR r.ord IS NOT NULL)))
                 AND ($5::text[] IS NULL OR coalesce(p.trust_tier, 'unverified') = ANY($5))
               ORDER BY (pk.b IS NOT NULL) DESC, r.ord NULLS LAST, c.bundle_id, c.id
               LIMIT $8";

/// Resolve a selection to concept records without their content (a preview,
/// or the first step of a build). Ordered by bundle then concept id; a query
/// selection is ordered by rank instead.
///
/// # Errors
///
/// An empty selection, or a catalog query failure.
pub async fn resolve<C: GenericClient>(
    client: &C,
    selection: &Selection,
) -> Result<Vec<ConceptRecord>> {
    if selection.is_empty() {
        return Err(anyhow!(
            "the selection is empty: give a query, a bundle, a type, a tag, or concept ids"
        ));
    }
    // Picks are added on top of the narrowed set and are never cut by the
    // limit: they sort first, and the limit is raised to hold every pick
    // (still bounded by MAX_CONCEPTS, beyond which a build is refused).
    let mut distinct_picks = selection.picks.clone();
    distinct_picks.sort();
    distinct_picks.dedup();
    if distinct_picks.len() > MAX_CONCEPTS {
        return Err(anyhow!(
            "{} files picked; a plugin holds at most {MAX_CONCEPTS} concepts",
            distinct_picks.len()
        ));
    }
    let limit =
        i64::try_from(selection.effective_limit().max(distinct_picks.len())).unwrap_or(i64::MAX);
    let has_filters = selection.has_filters();
    let pick_bundles: Vec<i64> = distinct_picks.iter().map(|p| p.bundle_id).collect();
    let pick_ids: Vec<String> = distinct_picks
        .iter()
        .map(|p| p.concept_id.clone())
        .collect();
    let bundle_ids = (!selection.bundle_ids.is_empty()).then(|| selection.bundle_ids.clone());
    let types = (!selection.types.is_empty()).then(|| selection.types.clone());
    let tags = (!selection.tags.is_empty()).then(|| selection.tags.clone());
    let concept_ids = (!selection.concept_ids.is_empty()).then(|| selection.concept_ids.clone());
    let trusted = selection
        .verified_only
        .then(|| TRUSTED_TIERS.map(str::to_owned).to_vec());

    let (ranked_bundles, ranked_ids) = match ranked_ids(client, selection).await? {
        Some((b, c)) => (Some(b), Some(c)),
        None => (None, None),
    };

    let sql = RESOLVE_SQL;
    let params: [&(dyn ToSql + Sync); 11] = [
        &bundle_ids,
        &types,
        &tags,
        &concept_ids,
        &trusted,
        &ranked_bundles,
        &ranked_ids,
        &limit,
        &pick_bundles,
        &pick_ids,
        &has_filters,
    ];
    let rows = client
        .query(sql, &params)
        .await
        .context("resolving the selection")?;
    rows.iter().map(record_from_row).collect()
}

// ---------------------------------------------------------------------------
// Catalog capabilities, freshness, seed closure, and the stale policy
// ---------------------------------------------------------------------------

/// The catalog capabilities a build consults, probed once per build from
/// `pgokf.capabilities()`. A catalog that predates the capability function
/// reports none of them, and the build behaves exactly as it always has.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Capabilities {
    /// `pgokf.effective_freshness`.
    pub freshness: bool,
    /// `pgokf.bundles.catalog_generation`.
    pub catalog_generation: bool,
    /// `pgokf.current_relationships` and `pgokf.concept_relationship_neighbors`.
    pub typed_relationships: bool,
}

/// Probe the catalog's capability declaration; SQLSTATE 42883 means the
/// catalog predates it.
pub(crate) async fn capabilities<C: GenericClient>(client: &C) -> Result<Capabilities> {
    let row = match client.query_opt("SELECT pgokf.capabilities()", &[]).await {
        Ok(row) => row,
        Err(error)
            if error
                .as_db_error()
                .is_some_and(|e| e.code().code() == "42883") =>
        {
            return Ok(Capabilities::default());
        }
        Err(error) => return Err(error).context("reading the catalog capabilities"),
    };
    let declared: Option<serde_json::Value> = row.and_then(|r| r.try_get(0).ok()).flatten();
    let has = |name: &str| {
        declared
            .as_ref()
            .and_then(|v| v.get(name))
            .and_then(serde_json::Value::as_i64)
            .is_some_and(|version| version >= 1)
    };
    Ok(Capabilities {
        freshness: has("effective_freshness"),
        catalog_generation: has("catalog_generation"),
        typed_relationships: has("typed_relationships"),
    })
}

/// One stale concept named in a refusal or an exclusion, with the role that
/// made it matter and the catalog's reasons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StaleConcept {
    pub bundle_id: i64,
    pub concept_id: String,
    /// `pick`, `seed`, `required-closure`, `closure`, or `selected`.
    pub role: String,
    pub state: String,
    pub reasons: Vec<String>,
}

impl StaleConcept {
    pub(crate) fn of(record: &ConceptRecord, require_closure: bool) -> Self {
        Self {
            bundle_id: record.bundle_id,
            concept_id: record.concept_id.clone(),
            role: record.origin.role(require_closure).to_owned(),
            state: record.freshness.state.clone(),
            reasons: record.freshness.reasons.clone(),
        }
    }
}

/// A typed relationship row whose target did not resolve: reported as
/// metadata when the closure is optional, refused when it is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnresolvedEdge {
    pub source_bundle_id: i64,
    pub source_concept_id: String,
    pub relation_type: String,
    pub direction: String,
    pub target_bundle_id: Option<i64>,
    pub target_concept_id: Option<String>,
    pub external_target: Option<String>,
}

/// One node a closure reached beyond its seeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClosureNode {
    pub bundle_id: i64,
    pub concept_id: String,
    /// Shortest hop count from a seed.
    pub hops: usize,
    /// The node this one was reached from.
    pub via: Option<ConceptRef>,
    /// The relation type of the reaching edge.
    pub relation_type: Option<String>,
}

/// What a seed closure did, recorded in the lockfile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClosureReport {
    pub seeds: Vec<ConceptRef>,
    pub direction: Direction,
    pub relation_types: Vec<String>,
    pub hops: usize,
    pub required: bool,
    /// The reached nodes (seeds excluded), in deterministic order.
    pub nodes: Vec<ClosureNode>,
    /// Unresolved edges the traversal could not follow. Present only when
    /// the closure is not required; a required closure refuses instead.
    pub unresolved: Vec<UnresolvedEdge>,
}

/// A build the stale policy or the closure rules refuse, carrying the
/// enumerations the refusal must give (spec §4.8). The MCP tool renders it
/// as structured JSON; everywhere else its message is the enumeration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum BuildRefusal {
    /// An exact pick, a seed, or a required closure node is stale.
    StaleRequired { concepts: Vec<StaleConcept> },
    /// Excluding the stale concepts left nothing to build.
    NoFreshConcepts { concepts: Vec<StaleConcept> },
    /// A required closure crosses a relationship whose target never resolved.
    UnresolvedRequired { edges: Vec<UnresolvedEdge> },
    /// A seed names no visible concept.
    SeedNotFound { seeds: Vec<ConceptRef> },
    /// Seeds plus their closure would push the build past the concept ceiling.
    ClosureExceedsLimit { limit: usize },
}

impl BuildRefusal {
    /// The structured form the MCP tool returns: `{"refused": true, ...}`.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        value["refused"] = serde_json::Value::Bool(true);
        value["detail"] = serde_json::Value::String(self.to_string());
        value
    }
}

impl std::fmt::Display for BuildRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleRequired { concepts } => {
                writeln!(
                    f,
                    "the stale policy refuses this build: {} required concept{} not fresh",
                    concepts.len(),
                    if concepts.len() == 1 { " is" } else { "s are" }
                )?;
                for c in concepts {
                    writeln!(
                        f,
                        "  {}:{} ({}, state {}, reasons: {})",
                        c.bundle_id,
                        c.concept_id,
                        c.role,
                        c.state,
                        c.reasons.join(", ")
                    )?;
                }
                write!(f, "rebuild with stale_policy warn to keep them, labelled")?;
            }
            Self::NoFreshConcepts { concepts } => {
                writeln!(
                    f,
                    "the stale policy refuses this build: excluding the stale concepts leaves nothing"
                )?;
                for c in concepts {
                    writeln!(
                        f,
                        "  {}:{} (state {}, reasons: {})",
                        c.bundle_id,
                        c.concept_id,
                        c.state,
                        c.reasons.join(", ")
                    )?;
                }
                write!(f, "rebuild with stale_policy warn to keep them, labelled")?;
            }
            Self::UnresolvedRequired { edges } => {
                writeln!(
                    f,
                    "the required closure cannot be completed: {} relationship{} an unresolved target",
                    edges.len(),
                    if edges.len() == 1 { " has" } else { "s have" }
                )?;
                for e in edges {
                    write!(
                        f,
                        "  {}:{} -[{}]-> ",
                        e.source_bundle_id, e.source_concept_id, e.relation_type
                    )?;
                    match (
                        &e.target_bundle_id,
                        &e.target_concept_id,
                        &e.external_target,
                    ) {
                        (Some(b), Some(c), _) => writeln!(f, "{b}:{c} (not in the catalog)")?,
                        (_, _, Some(x)) => writeln!(f, "external {x:?}")?,
                        _ => writeln!(f, "(target unknown)")?,
                    }
                }
            }
            Self::SeedNotFound { seeds } => {
                write!(f, "no visible concept answers to the seed")?;
                let list: Vec<String> = seeds.iter().map(ToString::to_string).collect();
                write!(f, "{}", if seeds.len() == 1 { " " } else { "s " })?;
                write!(f, "{}", list.join(", "))?;
            }
            Self::ClosureExceedsLimit { limit } => {
                write!(
                    f,
                    "the seeds and their closure select more than {limit} concepts; narrow the selection, the relationship types, or the hop bound"
                )?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for BuildRefusal {}

/// Everything the resolution stage learned beyond the records themselves:
/// what the closure did, what the stale policy excluded, and whether the
/// catalog has the freshness surface at all.
#[derive(Debug, Clone, Default)]
pub struct BuildReport {
    /// `false` on a catalog without `pgokf.effective_freshness` (every
    /// record then keeps [`Freshness::unknown`], and nothing is warned or
    /// excluded).
    pub freshness_available: bool,
    pub closure: Option<ClosureReport>,
    /// Concepts the `exclude` policy dropped, in selection order.
    pub excluded: Vec<StaleConcept>,
    /// The probed catalog capabilities, for the snapshot's extended form.
    pub(crate) capabilities: Capabilities,
}

/// Resolve a selection completely, inside the caller's snapshot: the plain
/// selectors, then the seed closure, then each record's effective freshness,
/// then the stale policy. This is the build's (and the preview's) one
/// resolution path; [`resolve`] remains the plain record-only form.
///
/// # Errors
///
/// An empty selection, a catalog failure, or a [`BuildRefusal`].
pub(crate) async fn resolve_selected<C: GenericClient>(
    client: &C,
    selection: &Selection,
) -> Result<(Vec<ConceptRecord>, BuildReport)> {
    let capabilities = capabilities(client).await?;
    let mut records = resolve(client, selection).await?;
    let closure = if selection.seeds.is_empty() {
        None
    } else {
        if !capabilities.typed_relationships {
            return Err(anyhow!(
                "seeds and closure need the catalog's typed_relationships capability \
                 (pgokf 0.3.0); this catalog does not offer it"
            ));
        }
        Some(apply_closure(client, selection, &mut records).await?)
    };
    let freshness_available = capabilities.freshness;
    if freshness_available {
        apply_freshness(client, &mut records).await?;
    }
    let excluded = apply_stale_policy(selection, &mut records)?;
    Ok((
        records,
        BuildReport {
            freshness_available,
            closure,
            excluded,
            capabilities,
        },
    ))
}

/// `true` when the freshness/closure machinery had any say in this build:
/// the lockfile and manifest record the policy only then, so a build with no
/// new selectors and nothing stale stays byte-identical to one from before
/// the surface existed.
pub(crate) fn engaged(selection: &Selection, records: &[ConceptRecord]) -> bool {
    !selection.seeds.is_empty()
        || selection.stale_policy == StalePolicy::Exclude
        || records.iter().any(|r| r.freshness.is_stale())
}

/// Fill every record's freshness from `pgokf.effective_freshness`: a
/// concept-scope override when one is recorded, otherwise the bundle row.
async fn apply_freshness<C: GenericClient>(
    client: &C,
    records: &mut [ConceptRecord],
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let bundle_ids: Vec<i64> = {
        let mut ids: Vec<i64> = records.iter().map(|r| r.bundle_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let rows = client
        .query(
            "SELECT bundle_id, scope_kind, scope_key, state, coalesce(reasons, '{}'),
                    to_char(stale_since AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
                    observed_revision, indexed_revision, published_revision, catalog_generation,
                    to_char(last_reconciled_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')
             FROM pgokf.effective_freshness
             WHERE bundle_id = ANY($1) AND scope_kind IN ('bundle', 'concept')",
            &[&bundle_ids],
        )
        .await
        .context("reading the effective freshness")?;
    let mut bundle_rows: std::collections::BTreeMap<i64, &tokio_postgres::Row> =
        std::collections::BTreeMap::new();
    let mut concept_rows: std::collections::BTreeMap<(i64, String), &tokio_postgres::Row> =
        std::collections::BTreeMap::new();
    for row in &rows {
        let scope_kind: String = row.try_get(1)?;
        if scope_kind == "bundle" {
            bundle_rows.insert(row.try_get(0)?, row);
        } else if let Some(key) = row.try_get::<_, Option<String>>(2)? {
            concept_rows.insert((row.try_get(0)?, key), row);
        }
    }
    for record in records.iter_mut() {
        let row = concept_rows
            .get(&(record.bundle_id, record.concept_id.clone()))
            .or_else(|| bundle_rows.get(&record.bundle_id));
        let Some(row) = row else { continue };
        record.freshness = Freshness {
            state: row.try_get(3)?,
            reasons: row.try_get(4)?,
            scope: row.try_get(1)?,
            stale_since: row.try_get(5)?,
            observed_revision: row.try_get(6)?,
            indexed_revision: row.try_get(7)?,
            published_revision: row.try_get(8)?,
            catalog_generation: row.try_get(9)?,
            last_reconciled_at: row.try_get(10)?,
        };
    }
    Ok(())
}

/// One node a single seed's traversal returned.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NeighborHit {
    bundle_id: i64,
    concept_id: String,
    hops: usize,
    /// The node this one was reached from (`None` at hop 0).
    via: Option<ConceptRef>,
    relation_type: Option<String>,
}

/// Merge one seed's hits into the closure's node map, keeping the shortest
/// hop count (and its reaching edge) when several seeds reach a node.
/// Deterministic: ties settle by first seed in sorted order, and duplicate
/// hops keep the existing entry, so cycles cannot rewrite a settled node.
fn merge_closure_nodes(
    nodes: &mut std::collections::BTreeMap<(i64, String), NeighborHit>,
    hits: Vec<NeighborHit>,
) {
    for hit in hits {
        let key = (hit.bundle_id, hit.concept_id.clone());
        match nodes.get(&key) {
            Some(existing) if existing.hops <= hit.hops => {}
            _ => {
                nodes.insert(key, hit);
            }
        }
    }
}

/// The most unresolved edges one closure report names.
const MAX_UNRESOLVED_REPORTED: usize = 100;

/// Expand the seeds through `pgokf.concept_relationship_neighbors` — one
/// call per seed inside the build's snapshot, merged into one visited set
/// keyed `(bundle_id, concept_id)` — and add every reached concept to the
/// records. The extension's traversal is already cycle-safe breadth-first
/// with type/direction filters over the active relationship generation, so
/// the builder holds no graph logic of its own.
///
/// # Errors
///
/// A [`BuildRefusal`] when a seed names no visible concept, when the closure
/// would push the build past [`MAX_CONCEPTS`], or when a required closure
/// crosses an unresolved target; a catalog failure otherwise.
#[allow(clippy::too_many_lines)]
async fn apply_closure<C: GenericClient>(
    client: &C,
    selection: &Selection,
    records: &mut Vec<ConceptRecord>,
) -> Result<ClosureReport> {
    let mut seeds = selection.seeds.clone();
    seeds.sort();
    seeds.dedup();
    let hops = selection.effective_hops();
    let hop_bound = i32::try_from(hops).unwrap_or(i32::MAX);
    let max_results = i32::try_from(MAX_CONCEPTS).unwrap_or(i32::MAX);
    let relation_types =
        (!selection.relation_types.is_empty()).then(|| selection.relation_types.clone());
    let direction = selection.direction.id();

    let mut nodes: std::collections::BTreeMap<(i64, String), NeighborHit> =
        std::collections::BTreeMap::new();
    for seed in &seeds {
        let rows = client
            .query(
                "SELECT bundle_id, concept_id, hops, relation_type,
                        path_bundle_ids, path_concept_ids
                 FROM pgokf.concept_relationship_neighbors($1, $2, $3, $4, $5, $6)",
                &[
                    &seed.bundle_id,
                    &seed.concept_id,
                    &hop_bound,
                    &direction,
                    &relation_types,
                    &max_results,
                ],
            )
            .await
            .context("walking the seed closure")?;
        let hits = rows
            .iter()
            .map(|row| {
                let node_hops = usize::try_from(row.try_get::<_, i32>(2)?).unwrap_or(0);
                let path_bundles: Vec<i64> = row.try_get(4)?;
                let path_concepts: Vec<String> = row.try_get(5)?;
                let via = node_hops.checked_sub(1).and_then(|previous| {
                    Some(ConceptRef {
                        bundle_id: *path_bundles.get(previous)?,
                        concept_id: path_concepts.get(previous)?.clone(),
                    })
                });
                Ok(NeighborHit {
                    bundle_id: row.try_get(0)?,
                    concept_id: row.try_get(1)?,
                    hops: node_hops,
                    via,
                    relation_type: row.try_get(3)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        merge_closure_nodes(&mut nodes, hits);
    }

    // Every seed and every reached node must be a visible, selectable
    // concept; fetch the missing ones through the picks branch of the one
    // resolution statement (picks add without narrowing and are never cut by
    // the limit).
    let mut wanted: Vec<ConceptRef> = seeds.clone();
    wanted.extend(nodes.keys().map(|(b, c)| ConceptRef {
        bundle_id: *b,
        concept_id: c.clone(),
    }));
    wanted.sort();
    wanted.dedup();
    let known: BTreeSet<(i64, &str)> = records
        .iter()
        .map(|r| (r.bundle_id, r.concept_id.as_str()))
        .collect();
    let missing: Vec<ConceptRef> = wanted
        .iter()
        .filter(|r| !known.contains(&(r.bundle_id, r.concept_id.as_str())))
        .cloned()
        .collect();
    if records.len() + missing.len() > MAX_CONCEPTS {
        return Err(BuildRefusal::ClosureExceedsLimit {
            limit: MAX_CONCEPTS,
        }
        .into());
    }
    if !missing.is_empty() {
        let mut fetched = resolve(
            client,
            &Selection {
                picks: missing,
                ..Selection::default()
            },
        )
        .await?;
        records.append(&mut fetched);
    }
    // Mark origins; a seed nothing visible answers to is a caller error in
    // either policy.
    let mut absent_seeds = Vec::new();
    for seed in &seeds {
        match records
            .iter_mut()
            .find(|r| r.bundle_id == seed.bundle_id && r.concept_id == seed.concept_id)
        {
            Some(record) => record.origin.seed = true,
            None => absent_seeds.push(seed.clone()),
        }
    }
    if !absent_seeds.is_empty() {
        return Err(BuildRefusal::SeedNotFound {
            seeds: absent_seeds,
        }
        .into());
    }
    for record in records.iter_mut() {
        if nodes.contains_key(&(record.bundle_id, record.concept_id.clone())) {
            record.origin.closure = true;
        }
    }

    // Unresolved edges out of (or, for an inbound walk, into) the nodes the
    // traversal could still expand: metadata when the closure is optional, a
    // refusal when it is required.
    let unresolved = unresolved_edges(
        client,
        &nodes,
        &seeds,
        hops,
        direction,
        relation_types.as_deref(),
    )
    .await?;
    if selection.require_closure && !unresolved.is_empty() {
        return Err(BuildRefusal::UnresolvedRequired { edges: unresolved }.into());
    }

    let mut report_nodes: Vec<ClosureNode> = nodes
        .into_values()
        .map(|hit| ClosureNode {
            bundle_id: hit.bundle_id,
            concept_id: hit.concept_id,
            hops: hit.hops,
            via: hit.via,
            relation_type: hit.relation_type,
        })
        .collect();
    report_nodes.sort_by(|a, b| {
        a.hops
            .cmp(&b.hops)
            .then_with(|| a.bundle_id.cmp(&b.bundle_id))
            .then_with(|| a.concept_id.cmp(&b.concept_id))
    });
    Ok(ClosureReport {
        seeds,
        direction: selection.direction,
        relation_types: selection.relation_types.clone(),
        hops,
        required: selection.require_closure,
        nodes: report_nodes,
        unresolved: if selection.require_closure {
            Vec::new()
        } else {
            unresolved
        },
    })
}

/// The unresolved relationship rows among the nodes a closure could still
/// expand (every seed, and every reached node below the hop bound), matching
/// the walk's type and direction filters.
async fn unresolved_edges<C: GenericClient>(
    client: &C,
    nodes: &std::collections::BTreeMap<(i64, String), NeighborHit>,
    seeds: &[ConceptRef],
    hops: usize,
    direction: &str,
    relation_types: Option<&[String]>,
) -> Result<Vec<UnresolvedEdge>> {
    let mut frontier: Vec<ConceptRef> = seeds.to_vec();
    frontier.extend(
        nodes
            .values()
            .filter(|hit| hit.hops < hops)
            .map(|hit| ConceptRef {
                bundle_id: hit.bundle_id,
                concept_id: hit.concept_id.clone(),
            }),
    );
    frontier.sort();
    frontier.dedup();
    if frontier.is_empty() {
        return Ok(Vec::new());
    }
    let bundles: Vec<i64> = frontier.iter().map(|r| r.bundle_id).collect();
    let ids: Vec<String> = frontier.iter().map(|r| r.concept_id.clone()).collect();
    let types = relation_types.map(<[String]>::to_vec);
    let rows = client
        .query(
            "SELECT r.source_bundle_id, r.source_concept_id, r.relation_type, r.direction,
                    r.target_bundle_id, r.target_concept_id, r.external_target
             FROM pgokf.current_relationships r
             WHERE r.unresolved
               AND ($3::text[] IS NULL OR r.relation_type = ANY($3))
               AND ((($4 = 'outbound' OR $4 = 'both')
                     AND (r.source_bundle_id, r.source_concept_id) IN
                         (SELECT * FROM unnest($1::bigint[], $2::text[])))
                    OR (($4 = 'inbound' OR $4 = 'both')
                     AND r.target_bundle_id IS NOT NULL
                     AND (r.target_bundle_id, r.target_concept_id) IN
                         (SELECT * FROM unnest($1::bigint[], $2::text[]))))
             ORDER BY r.source_bundle_id, r.source_concept_id, r.relation_type
             LIMIT $5",
            &[
                &bundles,
                &ids,
                &types,
                &direction,
                &i64::try_from(MAX_UNRESOLVED_REPORTED).unwrap_or(i64::MAX),
            ],
        )
        .await
        .context("checking the closure for unresolved targets")?;
    rows.iter()
        .map(|row| {
            Ok(UnresolvedEdge {
                source_bundle_id: row.try_get(0)?,
                source_concept_id: row.try_get(1)?,
                relation_type: row.try_get(2)?,
                direction: row.try_get(3)?,
                target_bundle_id: row.try_get(4)?,
                target_concept_id: row.try_get(5)?,
                external_target: row.try_get(6)?,
            })
        })
        .collect()
}

/// Apply the stale policy after freshness is known. `warn` keeps every
/// record (materialization labels them); `exclude` drops the stale optional
/// ones and refuses - enumerating ids and reasons - when an exact pick, a
/// seed, or a required closure node is stale, or when nothing fresh remains
/// (which is also how exclusion breaking a requested closure surfaces: a
/// required closure node is never dropped).
fn apply_stale_policy(
    selection: &Selection,
    records: &mut Vec<ConceptRecord>,
) -> Result<Vec<StaleConcept>> {
    if selection.stale_policy != StalePolicy::Exclude {
        return Ok(Vec::new());
    }
    let required = selection.require_closure;
    let blocking: Vec<StaleConcept> = records
        .iter()
        .filter(|r| r.freshness.is_stale() && r.origin.required(required))
        .map(|r| StaleConcept::of(r, required))
        .collect();
    if !blocking.is_empty() {
        return Err(BuildRefusal::StaleRequired { concepts: blocking }.into());
    }
    let excluded: Vec<StaleConcept> = records
        .iter()
        .filter(|r| r.freshness.is_stale())
        .map(|r| StaleConcept::of(r, required))
        .collect();
    records.retain(|r| !r.freshness.is_stale());
    if !excluded.is_empty() && records.is_empty() {
        return Err(BuildRefusal::NoFreshConcepts { concepts: excluded }.into());
    }
    Ok(excluded)
}

/// One resolved row as a [`ConceptRecord`] (without content).
fn record_from_row(r: &tokio_postgres::Row) -> Result<ConceptRecord> {
    let picked: bool = r.try_get(19)?;
    Ok(ConceptRecord {
        bundle_id: r.try_get(0)?,
        bundle_name: r.try_get(1)?,
        concept_id: r.try_get(2)?,
        path: r.try_get(3)?,
        title: r.try_get(4)?,
        description: r.try_get(5)?,
        concept_type: r.try_get(6)?,
        tags: r.try_get(7)?,
        file_hash: r.try_get(8)?,
        trust_tier: r.try_get(9)?,
        status: r.try_get(10)?,
        exact: false,
        bytes: Vec::new(),
        package: package_from_row(r)?,
        resource: resource_from_row(r)?,
        freshness: Freshness::unknown(),
        origin: Origin {
            pick: picked,
            seed: false,
            closure: false,
        },
    })
}

/// The `pgokf.skills` columns of a resolved row, when the concept is a
/// manifest.
fn package_from_row(r: &tokio_postgres::Row) -> Result<Option<PackageRecord>> {
    let name: Option<String> = r.try_get(12)?;
    let root: Option<String> = r.try_get(13)?;
    let hash: Option<String> = r.try_get(14)?;
    Ok(match (root, hash) {
        (Some(root), Some(hash)) => Some(PackageRecord {
            // A manifest always has a name; fall back to the directory only
            // if the frontmatter is somehow unnamed.
            name: name.unwrap_or_else(|| root.rsplit('/').next().unwrap_or("skill").to_owned()),
            root,
            hash,
            resources: Vec::new(),
        }),
        _ => None,
    })
}

/// The resource columns of a resolved row, when the concept is a package
/// script, reference, or asset.
fn resource_from_row(r: &tokio_postgres::Row) -> Result<Option<ResourceRecord>> {
    let class: Option<String> = r.try_get(15)?;
    let source_path: Option<String> = r.try_get(16)?;
    let package_concept_id: Option<String> = r.try_get(17)?;
    let sha256: Option<String> = r.try_get(18)?;
    Ok(match (class, source_path, package_concept_id, sha256) {
        (Some(class), Some(source_path), Some(package_concept_id), Some(sha256)) => {
            Some(ResourceRecord {
                class,
                source_path,
                package_concept_id,
                sha256,
            })
        }
        _ => None,
    })
}

/// For a query selection, the `(bundle_id, concept_id)` pairs
/// `concept_search` ranks, in rank order, restricted to the selected
/// bundles; `None` when the selection has no query text.
async fn ranked_ids<C: GenericClient>(
    client: &C,
    selection: &Selection,
) -> Result<Option<(Vec<i64>, Vec<String>)>> {
    let Some(q) = selection
        .query
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
    else {
        return Ok(None);
    };
    let one_bundle = match selection.bundle_ids.as_slice() {
        [only] => Some(*only),
        _ => None,
    };
    let first_type = match selection.types.as_slice() {
        [only] => Some(only.clone()),
        _ => None,
    };
    let tags = (!selection.tags.is_empty()).then(|| selection.tags.clone());
    // The other selectors (several types, verified-only, ids, bundles) are
    // applied after ranking, so rank the full window and let the outer
    // query's LIMIT cut it; otherwise a page could come back short.
    let search_limit = i32::try_from(MAX_CONCEPTS).unwrap_or(i32::MAX);
    let rows = client
        .query(
            "SELECT bundle_id, concept_id
             FROM pgokf.concept_search($1, $2, $3, $4, $5, NULL, NULL, NULL)",
            &[&q, &one_bundle, &search_limit, &first_type, &tags],
        )
        .await
        .context("ranking the selection")?;
    let mut ids: (Vec<i64>, Vec<String>) = (Vec::new(), Vec::new());
    for row in rows {
        let b: i64 = row.try_get(0)?;
        let c: String = row.try_get(1)?;
        if selection.bundle_ids.is_empty() || selection.bundle_ids.contains(&b) {
            ids.0.push(b);
            ids.1.push(c);
        }
    }
    Ok(Some(ids))
}

/// Load each record's content, always through the audited readers (an
/// export is exactly what that log is for): a skill package through
/// `get_skill()` (the exact `SKILL.md`) plus `get_script()` /
/// `get_reference()` for each resource it owns; a resource selected on its
/// own through the same two; any other concept through
/// `get_concept_source()`, or a document reconstructed from the indexed
/// fields when the catalog keeps no source. A concept that disappeared (or
/// was hidden) between resolving and loading is dropped from the list, and
/// so is a resource whose package is in the selection, since the package
/// carries it.
///
/// # Errors
///
/// A catalog failure other than "no stored source".
pub async fn load_sources<C: GenericClient>(
    client: &C,
    records: &mut Vec<ConceptRecord>,
) -> Result<()> {
    drop_packaged_resources(records);
    let mut vanished: Vec<bool> = vec![false; records.len()];
    // A running total, checked as each record is loaded, so the ceiling
    // bounds the work and the memory - not just the finished output. Without
    // it a huge selection was read whole (and audit-logged, one row per
    // member) before being refused.
    let mut loaded: u64 = 0;
    for (index, record) in records.iter_mut().enumerate() {
        vanished[index] = !load_one(client, record).await?;
        loaded = loaded.saturating_add(record_bytes(record));
        if loaded > MAX_CONTENT_BYTES {
            return Err(anyhow!(
                "the selection loads more than {MAX_CONTENT_BYTES} bytes; a build takes at \
                 most that. Narrow the selection."
            ));
        }
    }
    let mut index = 0;
    records.retain(|r| {
        // A document that loaded nothing vanished; a resource may legitimately
        // be empty, so only the explicit flag drops it.
        let keep = !vanished[index] && (r.resource.is_some() || !r.bytes.is_empty());
        index += 1;
        keep
    });
    Ok(())
}

/// Drop every resource whose package is in the selection: the package
/// carries it, byte for byte, so writing it again would duplicate the file.
/// Applied to previews and builds alike so both show the same tree.
pub fn drop_packaged_resources(records: &mut Vec<ConceptRecord>) {
    let owned: BTreeSet<(i64, String)> = records
        .iter()
        .filter(|r| r.package.is_some())
        .map(|r| (r.bundle_id, r.concept_id.clone()))
        .collect();
    records.retain(|r| {
        r.resource
            .as_ref()
            .is_none_or(|res| !owned.contains(&(r.bundle_id, res.package_concept_id.clone())))
    });
}

/// The bytes a reader returned must hash to what the listing promised;
/// otherwise the catalog changed between the two reads and the tree would
/// mix versions.
fn verify_sha256(bytes: &[u8], expected: &str, concept_id: &str) -> Result<()> {
    let actual = sha256_hex(bytes);
    if !expected.is_empty() && actual != expected {
        return Err(anyhow!(
            "resource {concept_id} changed while the plugin was being built (expected \
             sha256 {expected}, read {actual}); build again"
        ));
    }
    Ok(())
}

/// Lowercase hex SHA-256 of a buffer.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Load one record's bytes: a skill package, a resource selected on its own,
/// or any other concept from its stored source (or reconstructed from the
/// indexed fields when the catalog keeps none). Returns `false` when the
/// concept or resource vanished between resolving and loading, so the caller
/// drops it.
async fn load_one<C: GenericClient>(client: &C, record: &mut ConceptRecord) -> Result<bool> {
    if record.package.is_some() {
        return load_package(client, record).await;
    }
    if let Some(resource) = record.resource.clone() {
        return match resource_bytes(
            client,
            record.bundle_id,
            &record.concept_id,
            &resource.class,
        )
        .await?
        {
            Some(bytes) => {
                verify_sha256(&bytes, &resource.sha256, &record.concept_id)?;
                record.bytes = bytes;
                record.exact = true;
                Ok(true)
            }
            None => Ok(false),
        };
    }
    let source = client
        .query_opt(
            "SELECT pgokf.get_concept_source($1, $2)",
            &[&record.bundle_id, &record.concept_id],
        )
        .await;
    match source {
        Ok(Some(row)) => {
            record.bytes = row.try_get::<_, Option<Vec<u8>>>(0)?.unwrap_or_default();
            record.exact = !record.bytes.is_empty();
        }
        // invalid_parameter_value: no source stored for this concept.
        Err(error)
            if error
                .as_db_error()
                .is_some_and(|e| e.code().code() == "22023") =>
        {
            record.exact = false;
        }
        Ok(None) => record.exact = false,
        Err(error) => return Err(error).context("reading a concept's stored source"),
    }
    if !record.exact {
        let body: Option<String> = client
            .query_opt(
                "SELECT body_text FROM pgokf.concepts WHERE bundle_id = $1 AND id = $2",
                &[&record.bundle_id, &record.concept_id],
            )
            .await
            .context("reading a concept's indexed text")?
            .map(|row| row.try_get(0))
            .transpose()?;
        record.bytes = body
            .map(|b| reconstruct(record, &b).into_bytes())
            .unwrap_or_default();
    }
    Ok(true)
}

/// The bytes one loaded record contributes to the build: its own content
/// plus, for a package, every resource it carries.
fn record_bytes(record: &ConceptRecord) -> u64 {
    let members: u64 = record.package.as_ref().map_or(0, |p| {
        p.resources
            .iter()
            .map(|f| u64::try_from(f.bytes.len()).unwrap_or(u64::MAX))
            .fold(0_u64, u64::saturating_add)
    });
    u64::try_from(record.bytes.len())
        .unwrap_or(u64::MAX)
        .saturating_add(members)
}

/// Fill a skill record with its exact manifest and every resource's bytes.
/// Returns `false` when the package vanished (or was hidden) between
/// resolving and loading, so the caller drops the record. A listed member
/// that cannot be read, or whose bytes no longer hash as listed, is an error:
/// the catalog changed under the build and the tree would mix versions.
async fn load_package<C: GenericClient>(client: &C, record: &mut ConceptRecord) -> Result<bool> {
    let row = match client
        .query_opt(
            "SELECT s.skill_md, s.package_root, s.package_hash, s.agent_skill->>'name', s.resources
             FROM pgokf.get_skill($1, $2) AS s",
            &[&record.bundle_id, &record.concept_id],
        )
        .await
    {
        Ok(row) => row,
        Err(error) if is_invalid_parameter(&error) => None,
        Err(error) => return Err(error).context("reading a skill package"),
    };
    let Some(row) = row else {
        record.bytes.clear();
        return Ok(false);
    };
    let skill_md: Vec<u8> = row.try_get(0)?;
    let root: String = row.try_get(1)?;
    let hash: String = row.try_get(2)?;
    let name: Option<String> = row.try_get(3)?;
    let listing: serde_json::Value = row.try_get(4)?;
    let members = listing.as_array().map_or(0, Vec::len);
    if members > MAX_PACKAGE_MEMBERS {
        return Err(anyhow!(
            "package {} has {members} members; a build takes at most \
             {MAX_PACKAGE_MEMBERS}. Narrow the selection.",
            record.concept_id
        ));
    }
    let mut resources = Vec::new();
    for entry in listing.as_array().into_iter().flatten() {
        let concept_id = entry["concept_id"].as_str().unwrap_or_default().to_owned();
        let class = entry["class"].as_str().unwrap_or("reference").to_owned();
        let path = entry["path"].as_str().unwrap_or_default().to_owned();
        let sha256 = entry["sha256"].as_str().unwrap_or_default().to_owned();
        let file_hash = entry["file_hash"].as_str().unwrap_or_default().to_owned();
        if concept_id.is_empty() || path.is_empty() {
            continue;
        }
        let Some(bytes) = resource_bytes(client, record.bundle_id, &concept_id, &class).await?
        else {
            return Err(anyhow!(
                "package {} lists {concept_id} but it could not be read (the catalog changed \
                 while the plugin was being built); build again",
                record.concept_id
            ));
        };
        verify_sha256(&bytes, &sha256, &concept_id)?;
        resources.push(ResourceFile {
            concept_id,
            class,
            path,
            sha256,
            file_hash,
            bytes,
        });
    }
    let package = record.package.get_or_insert_with(|| PackageRecord {
        name: String::new(),
        root: String::new(),
        hash: String::new(),
        resources: Vec::new(),
    });
    if let Some(name) = name {
        package.name = name;
    }
    package.root = root;
    package.hash = hash;
    package.resources = resources;
    record.bytes = skill_md;
    record.exact = true;
    Ok(true)
}

/// The exact bytes of one package resource through the audited reader for
/// its class; `None` when it vanished or is hidden.
async fn resource_bytes<C: GenericClient>(
    client: &C,
    bundle_id: i64,
    concept_id: &str,
    class: &str,
) -> Result<Option<Vec<u8>>> {
    let sql = if class == "script" {
        "SELECT (pgokf.get_script($1, $2)).exact_bytes"
    } else {
        "SELECT (pgokf.get_reference($1, $2)).exact_bytes"
    };
    match client.query_opt(sql, &[&bundle_id, &concept_id]).await {
        Ok(Some(row)) => Ok(row.try_get::<_, Option<Vec<u8>>>(0)?),
        Ok(None) => Ok(None),
        Err(error) if is_invalid_parameter(&error) => Ok(None),
        Err(error) => Err(error).context("reading a package resource"),
    }
}

/// SQLSTATE 22023: the catalog's "no such (visible) concept".
fn is_invalid_parameter(error: &tokio_postgres::Error) -> bool {
    error
        .as_db_error()
        .is_some_and(|e| e.code().code() == "22023")
}

/// A document for a concept whose source the catalog does not keep: the
/// modeled frontmatter fields and the indexed text, marked as such.
fn reconstruct(record: &ConceptRecord, body: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("---\n");
    if let Some(t) = &record.concept_type {
        let _ = writeln!(out, "type: {}", yaml_string(t));
    }
    if let Some(t) = &record.title {
        let _ = writeln!(out, "title: {}", yaml_string(t));
    }
    if let Some(d) = &record.description {
        let _ = writeln!(out, "description: {}", yaml_string(d));
    }
    if !record.tags.is_empty() {
        out.push_str("tags:\n");
        for tag in &record.tags {
            let _ = writeln!(out, "  - {}", yaml_string(tag));
        }
    }
    out.push_str("---\n\n");
    out.push_str("<!-- Reconstructed from the catalog's indexed text: the bundle was ingested without store_source, so this is not the original file. -->\n\n");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// A YAML scalar in the JSON-compatible double-quoted form, which every
/// YAML parser accepts and which needs no escaping rules of its own.
#[must_use]
pub fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// The catalog snapshot the lockfile records.
///
/// # Errors
///
/// A catalog query failure.
pub async fn snapshot<C: GenericClient>(client: &C, bundle_ids: &[i64]) -> Result<Snapshot> {
    snapshot_extended(client, bundle_ids, false).await
}

/// The catalog snapshot, extended with each bundle's catalog generation and
/// effective freshness when `extended` (a build that engaged the freshness
/// surface on a catalog that has it). The plain form is byte-identical to
/// what the lockfile has always recorded.
///
/// # Errors
///
/// A catalog query failure.
pub(crate) async fn snapshot_extended<C: GenericClient>(
    client: &C,
    bundle_ids: &[i64],
    extended: bool,
) -> Result<Snapshot> {
    let versions = client
        .query_one(
            "SELECT pgokf.version(),
                    coalesce((SELECT extversion FROM pg_catalog.pg_extension WHERE extname = 'pgokf'), '')",
            &[],
        )
        .await
        .context("reading the catalog version")?;
    let ids: Vec<i64> = bundle_ids.to_vec();
    let sql = if extended {
        "SELECT b.id, coalesce(b.name, regexp_replace(b.path, '^.*/', '')), b.sync_hash,
                to_char(b.last_synced_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
                b.catalog_generation, ef.state, coalesce(ef.reasons, '{}'), ef.embedding_contract
         FROM pgokf.bundles b
         LEFT JOIN pgokf.effective_freshness ef
                ON ef.bundle_id = b.id AND ef.scope_kind = 'bundle'
         WHERE b.id = ANY($1) ORDER BY b.id"
    } else {
        "SELECT b.id, coalesce(b.name, regexp_replace(b.path, '^.*/', '')), b.sync_hash,
                to_char(b.last_synced_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')
         FROM pgokf.bundles b WHERE b.id = ANY($1) ORDER BY b.id"
    };
    let rows = client
        .query(sql, &[&ids])
        .await
        .context("reading the bundle states")?;
    let bundles = rows
        .iter()
        .map(|r| {
            Ok(BundleState {
                id: r.try_get(0)?,
                name: r.try_get(1)?,
                sync_hash: r.try_get(2)?,
                last_synced_at: r.try_get(3)?,
                catalog_generation: if extended { r.try_get(4)? } else { None },
                freshness_state: if extended { r.try_get(5)? } else { None },
                freshness_reasons: if extended { r.try_get(6)? } else { Vec::new() },
                embedding_contract: if extended { r.try_get(7)? } else { None },
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Snapshot {
        version: versions.try_get(0)?,
        sql_version: versions.try_get(1)?,
        bundles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> ConceptRecord {
        ConceptRecord {
            bundle_id: 1,
            bundle_name: "sample".to_owned(),
            concept_id: "runbooks/a".to_owned(),
            path: "runbooks/a.md".to_owned(),
            title: Some("A \"quoted\" title".to_owned()),
            description: None,
            concept_type: Some("Runbook".to_owned()),
            tags: vec!["x".to_owned()],
            file_hash: "abc".to_owned(),
            trust_tier: "unverified".to_owned(),
            status: "stable".to_owned(),
            exact: false,
            bytes: Vec::new(),
            package: None,
            resource: None,
            freshness: Freshness::unknown(),
            origin: Origin::default(),
        }
    }

    #[test]
    fn drop_packaged_resources_keeps_only_resources_of_absent_packages() {
        // Arrange: a package, one of its scripts, and a script of a package
        // that is not selected (and one in another bundle with the same id).
        let mut package = record();
        package.concept_id = "skills/deploy/SKILL".to_owned();
        package.package = Some(PackageRecord {
            name: "deploy".to_owned(),
            root: "skills/deploy".to_owned(),
            hash: "p".repeat(64),
            resources: Vec::new(),
        });
        let mut owned = record();
        owned.concept_id = "skills/deploy/scripts/a.sh".to_owned();
        owned.resource = Some(ResourceRecord {
            class: "script".to_owned(),
            source_path: "scripts/a.sh".to_owned(),
            package_concept_id: "skills/deploy/SKILL".to_owned(),
            sha256: String::new(),
        });
        let mut other_bundle = owned.clone();
        other_bundle.bundle_id = 2;
        let mut orphan = owned.clone();
        orphan.concept_id = "skills/other/scripts/b.sh".to_owned();
        orphan.resource.as_mut().unwrap().package_concept_id = "skills/other/SKILL".to_owned();
        let mut records = vec![package, owned, other_bundle, orphan];

        // Act
        drop_packaged_resources(&mut records);

        // Assert
        let ids: Vec<(i64, &str)> = records
            .iter()
            .map(|r| (r.bundle_id, r.concept_id.as_str()))
            .collect();
        assert_eq!(
            ids,
            [
                (1, "skills/deploy/SKILL"),
                (2, "skills/deploy/scripts/a.sh"),
                (1, "skills/other/scripts/b.sh")
            ]
        );
    }

    #[test]
    fn verify_sha256_accepts_a_match_or_an_unknown_digest_and_refuses_drift() {
        // Arrange
        let bytes = b"echo ok\n";
        let digest = sha256_hex(bytes);

        // Act / Assert
        assert!(verify_sha256(bytes, &digest, "x").is_ok());
        assert!(verify_sha256(bytes, "", "x").is_ok());
        let error = verify_sha256(bytes, &"0".repeat(64), "skills/deploy/scripts/a.sh")
            .expect_err("a different digest is refused");
        assert!(
            error
                .to_string()
                .contains("changed while the plugin was being built")
        );
    }

    #[test]
    fn concept_refs_parse_the_bundle_colon_id_form() {
        // Arrange / Act / Assert
        assert_eq!(
            ConceptRef::parse(" 3:skills/deploy/SKILL "),
            Some(ConceptRef {
                bundle_id: 3,
                concept_id: "skills/deploy/SKILL".to_owned()
            })
        );
        assert_eq!(
            ConceptRef::parse("2:a:b").map(|r| r.concept_id),
            Some("a:b".to_owned())
        );
        assert_eq!(ConceptRef::parse("x:id"), None);
        assert_eq!(ConceptRef::parse("0:id"), None);
        assert_eq!(ConceptRef::parse("3:"), None);
        assert_eq!(ConceptRef::parse("no-colon"), None);
        assert_eq!(ConceptRef::parse("3:x").unwrap().to_string(), "3:x");
    }

    #[test]
    fn picks_make_a_selection_non_empty_without_filtering() {
        // Arrange
        let picked = Selection {
            picks: vec![ConceptRef {
                bundle_id: 1,
                concept_id: "a".to_owned(),
            }],
            ..Selection::default()
        };
        let mixed = Selection {
            tags: vec!["ops".to_owned()],
            picks: picked.picks.clone(),
            ..Selection::default()
        };

        // Act / Assert
        assert!(!picked.is_empty());
        assert!(!picked.has_filters());
        assert_eq!(picked.describe(), "1 picked file");
        assert!(mixed.has_filters());
        assert_eq!(mixed.describe(), "tagged ops, plus 1 picked file");
    }

    #[test]
    fn everything_is_a_scope_of_its_own() {
        // Arrange
        let everything = Selection {
            all: true,
            ..Selection::default()
        };
        let narrowed = Selection {
            all: true,
            types: vec!["Runbook".to_owned()],
            ..Selection::default()
        };
        let in_bundle = Selection {
            all: true,
            bundle_ids: vec![2],
            ..Selection::default()
        };

        // Act / Assert
        assert!(everything.has_filters() && !everything.is_empty());
        assert_eq!(everything.describe(), "everything in the catalog");
        assert_eq!(narrowed.describe(), "everything of type Runbook");
        assert_eq!(in_bundle.describe(), "in bundle(s) 2");
        assert_eq!(Selection::default().describe(), "nothing selected");
    }

    #[test]
    fn the_resolve_statement_gates_picks_by_trust_and_guards_filters() {
        // The union of picks and filters is parenthesized so the trust
        // filter applies to both, and the filters are guarded by $11.
        assert!(RESOLVE_SQL.contains("WHERE (pk.b IS NOT NULL"));
        assert!(RESOLVE_SQL.contains("OR ($11\n"));
        let union_end = RESOLVE_SQL
            .find("r.ord IS NOT NULL)))")
            .expect("union closes");
        let trust = RESOLVE_SQL
            .find("AND ($5::text[] IS NULL")
            .expect("trust filter");
        assert!(
            trust > union_end,
            "the trust filter follows the closed union"
        );
        assert!(RESOLVE_SQL.contains("ORDER BY (pk.b IS NOT NULL) DESC"));
    }

    #[test]
    fn parse_list_folds_duplicates_and_names_the_bad_entry() {
        // Arrange / Act
        let picks = ConceptRef::parse_list("1:a\n2:b, 1:a\n\n").expect("parses");
        let error = ConceptRef::parse_list("1:a\nnope").expect_err("bad entry");
        let (kept, malformed) = ConceptRef::parse_entries("1:a\nnope\n0:z\n2:b");

        // Assert
        assert_eq!(
            picks.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["1:a", "2:b"]
        );
        assert!(error.to_string().contains("\"nope\""));
        assert_eq!(kept.len(), 2, "the good entries survive a bad one");
        assert_eq!(malformed, ["nope", "0:z"]);
    }

    #[test]
    fn an_empty_selection_is_recognised_and_described() {
        // Arrange
        let empty = Selection::default();
        let tagged = Selection {
            tags: vec!["ops".to_owned()],
            bundle_ids: vec![2],
            verified_only: true,
            ..Selection::default()
        };

        // Act & Assert
        assert!(empty.is_empty());
        assert_eq!(empty.describe(), "nothing selected");
        assert!(!tagged.is_empty());
        assert_eq!(
            tagged.describe(),
            "tagged ops, in bundle(s) 2, verified only"
        );
        assert_eq!(
            Selection {
                limit: Some(9_999),
                ..Selection::default()
            }
            .effective_limit(),
            MAX_CONCEPTS
        );
    }

    #[test]
    fn reconstruct_writes_valid_frontmatter_and_marks_the_document() {
        // Arrange
        let record = record();

        // Act
        let text = reconstruct(&record, "Body text");

        // Assert
        assert!(text.starts_with(
            "---\ntype: \"Runbook\"\ntitle: \"A \\\"quoted\\\" title\"\ntags:\n  - \"x\"\n---\n\n"
        ));
        assert!(text.contains("Reconstructed from the catalog's indexed text"));
        assert!(text.ends_with("Body text\n"));
    }

    fn stale_record(id: &str) -> ConceptRecord {
        let mut record = record();
        record.concept_id = id.to_owned();
        record.freshness = Freshness {
            state: "stale".to_owned(),
            reasons: vec!["upstream_changed".to_owned()],
            scope: "bundle".to_owned(),
            stale_since: None,
            observed_revision: Some("r2".to_owned()),
            indexed_revision: Some("r1".to_owned()),
            published_revision: Some("41".to_owned()),
            catalog_generation: Some(42),
            last_reconciled_at: None,
        };
        record
    }

    #[test]
    fn stale_policy_and_direction_parse_their_ids() {
        // Arrange / Act / Assert
        assert_eq!(StalePolicy::parse("warn"), Some(StalePolicy::Warn));
        assert_eq!(StalePolicy::parse("exclude"), Some(StalePolicy::Exclude));
        assert_eq!(StalePolicy::parse("drop"), None);
        assert_eq!(StalePolicy::Warn.id(), "warn");
        assert_eq!(StalePolicy::default(), StalePolicy::Warn);
        assert_eq!(Direction::parse("outbound"), Some(Direction::Outbound));
        assert_eq!(Direction::parse("inbound"), Some(Direction::Inbound));
        assert_eq!(Direction::parse("both"), Some(Direction::Both));
        assert_eq!(Direction::parse("sideways"), None);
        assert_eq!(Direction::default().id(), "outbound");
        assert_eq!(Direction::Both.id(), "both");
    }

    #[test]
    fn warn_keeps_every_stale_record_and_excludes_nothing() {
        // Arrange
        let mut records = vec![stale_record("a"), record()];
        let selection = Selection::default();

        // Act
        let excluded = apply_stale_policy(&selection, &mut records).expect("warn never refuses");

        // Assert
        assert_eq!(records.len(), 2);
        assert!(excluded.is_empty());
    }

    #[test]
    fn exclude_drops_stale_optional_records_and_reports_them() {
        // Arrange: one stale selected record beside a fresh one.
        let mut records = vec![stale_record("a"), record()];
        let selection = Selection {
            stale_policy: StalePolicy::Exclude,
            ..Selection::default()
        };

        // Act
        let excluded = apply_stale_policy(&selection, &mut records).expect("nothing required");

        // Assert
        assert_eq!(records.len(), 1);
        assert_eq!(excluded.len(), 1);
        assert_eq!(excluded[0].concept_id, "a");
        assert_eq!(excluded[0].role, "selected");
        assert_eq!(excluded[0].reasons, ["upstream_changed"]);
    }

    #[test]
    fn exclude_refuses_a_stale_pick_seed_or_required_closure_node() {
        // Arrange
        let mut picks = stale_record("picked");
        picks.origin.pick = true;
        let mut seed = stale_record("seed");
        seed.origin.seed = true;
        let mut closure = stale_record("via-closure");
        closure.origin.closure = true;
        let selection = Selection {
            stale_policy: StalePolicy::Exclude,
            require_closure: true,
            ..Selection::default()
        };

        // Act
        let error = apply_stale_policy(&selection, &mut vec![picks, seed, closure, record()])
            .expect_err("required stale concepts refuse the build");

        // Assert
        let refusal = error
            .downcast_ref::<BuildRefusal>()
            .expect("a structured refusal");
        let BuildRefusal::StaleRequired { concepts } = refusal else {
            panic!("expected StaleRequired, got {refusal:?}");
        };
        let roles: Vec<&str> = concepts.iter().map(|c| c.role.as_str()).collect();
        assert_eq!(roles, ["pick", "seed", "required-closure"]);
        for concept in concepts {
            assert_eq!(concept.state, "stale");
            assert_eq!(concept.reasons, ["upstream_changed"]);
        }
        // The message enumerates ids and reasons.
        let text = refusal.to_string();
        assert!(
            text.contains("1:picked (pick, state stale, reasons: upstream_changed)"),
            "{text}"
        );
        assert_eq!(refusal.to_json()["refused"], serde_json::Value::Bool(true));
    }

    #[test]
    fn exclude_without_required_closure_drops_a_stale_closure_node() {
        // Arrange
        let mut closure = stale_record("via-closure");
        closure.origin.closure = true;
        let selection = Selection {
            stale_policy: StalePolicy::Exclude,
            ..Selection::default()
        };

        // Act
        let excluded =
            apply_stale_policy(&selection, &mut vec![closure, record()]).expect("optional closure");

        // Assert
        assert_eq!(excluded[0].role, "closure");
    }

    #[test]
    fn exclude_refuses_when_nothing_fresh_remains() {
        // Arrange
        let selection = Selection {
            stale_policy: StalePolicy::Exclude,
            ..Selection::default()
        };

        // Act
        let error = apply_stale_policy(&selection, &mut vec![stale_record("a"), stale_record("b")])
            .expect_err("an empty build is refused");

        // Assert
        let refusal = error.downcast_ref::<BuildRefusal>().expect("structured");
        let BuildRefusal::NoFreshConcepts { concepts } = refusal else {
            panic!("expected NoFreshConcepts, got {refusal:?}");
        };
        assert_eq!(concepts.len(), 2);
        assert!(refusal.to_string().contains("1:a"));
    }

    #[test]
    fn closure_nodes_merge_by_shortest_hops_and_cycles_cannot_rewrite() {
        // Arrange: two seeds reach the same node; the later (longer) path
        // must not replace the shorter one, and a cycle back to a settled
        // node changes nothing.
        let mut nodes = std::collections::BTreeMap::new();
        let hit = |concept: &str, hops: usize, via: Option<ConceptRef>| NeighborHit {
            bundle_id: 1,
            concept_id: concept.to_owned(),
            hops,
            via,
            relation_type: Some("ns:rel".to_owned()),
        };

        // Act
        merge_closure_nodes(&mut nodes, vec![hit("a", 2, None)]);
        merge_closure_nodes(
            &mut nodes,
            vec![
                hit("a", 3, None),
                hit(
                    "b",
                    1,
                    Some(ConceptRef {
                        bundle_id: 1,
                        concept_id: "seed".to_owned(),
                    }),
                ),
            ],
        );
        merge_closure_nodes(&mut nodes, vec![hit("b", 4, None)]);

        // Assert
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[&(1, "a".to_owned())].hops, 2);
        let b = &nodes[&(1, "b".to_owned())];
        assert_eq!(b.hops, 1);
        assert_eq!(
            b.via,
            Some(ConceptRef {
                bundle_id: 1,
                concept_id: "seed".to_owned()
            })
        );
    }

    #[test]
    fn seeds_make_a_selection_non_empty_and_described() {
        // Arrange
        let seeded = Selection {
            seeds: vec![ConceptRef {
                bundle_id: 1,
                concept_id: "a".to_owned(),
            }],
            relation_types: vec!["ns:depends".to_owned()],
            ..Selection::default()
        };

        // Act / Assert
        assert!(!seeded.is_empty());
        assert!(!seeded.has_filters(), "seeds add without narrowing");
        assert_eq!(
            seeded.describe(),
            "the closure of 1 seed within 2 hops of type ns:depends"
        );
        assert!(engaged(&seeded, &[]));
        assert!(!engaged(&Selection::default(), &[record()]));
        assert!(engaged(&Selection::default(), &[stale_record("a")]));
        assert!(
            engaged(
                &Selection {
                    stale_policy: StalePolicy::Exclude,
                    ..Selection::default()
                },
                &[record()]
            ),
            "an explicit exclude policy is recorded even when nothing is stale"
        );
        assert_eq!(seeded.effective_hops(), DEFAULT_HOPS);
        assert_eq!(
            Selection {
                hops: Some(99),
                ..Selection::default()
            }
            .effective_hops(),
            MAX_HOPS
        );
    }

    #[test]
    fn unknown_freshness_is_not_stale() {
        // Arrange / Act / Assert
        assert!(!Freshness::unknown().is_stale());
        assert!(
            !Freshness {
                state: "fresh".to_owned(),
                ..Freshness::unknown()
            }
            .is_stale()
        );
        assert!(
            Freshness {
                state: "reconciling".to_owned(),
                ..Freshness::unknown()
            }
            .is_stale()
        );
    }
}
