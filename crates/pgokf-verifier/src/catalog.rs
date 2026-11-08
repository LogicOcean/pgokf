// SPDX-License-Identifier: AGPL-3.0-only
//! Typed catalog contracts. A contract must come from a trusted, independently
//! verified fresh build; it must never be synthesized from the candidate.
//! This module validates captured state, not SQL or a running server.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const TIERS: [&str; 4] = [
    "pgokf_reader",
    "pgokf_writer",
    "pgokf_admin",
    "pgokf_dispatcher",
];
pub const SEARCH_INDEX: &str = "pgokf.concepts_bm25_idx";
pub const EMBEDDING_INDEX: &str = "pgokf.concept_embedding_hnsw_idx";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Plain,
    PgTextsearch,
    PgSearch,
}

/// Whether the caller's operation requested rebuilding the optional index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingPolicy {
    NotRequested,
    Rebuilt,
}

/// Resolved names rather than OIDs; positional vectors preserve key order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexKey {
    pub expression: String,
    pub opclass: String,
    pub collation: Option<String>,
    pub options: i16,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Independent PostgreSQL index flags are compared without normalization"
)]
pub struct RuntimeIndex {
    pub name: String,
    pub relation: String,
    pub access_method: String,
    pub keys: Vec<IndexKey>,
    pub included_columns: Vec<String>,
    pub predicate: Option<String>,
    pub valid: bool,
    pub ready: bool,
    pub live: bool,
    pub unique: bool,
    pub primary: bool,
    pub exclusion: bool,
    pub immediate: bool,
    pub clustered: bool,
    pub replica_identity: bool,
    pub nulls_not_distinct: bool,
    pub owner: String,
    pub acl: BTreeSet<String>,
    pub comment: Option<String>,
    pub security_labels: BTreeMap<String, String>,
    pub options: BTreeMap<String, String>,
    pub tablespace: String,
    pub persistence: String,
    pub extension_member: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Independent PostgreSQL role attributes are security-significant"
)]
pub struct Role {
    pub name: String,
    pub superuser: bool,
    pub inherit: bool,
    pub create_role: bool,
    pub create_db: bool,
    pub login: bool,
    pub replication: bool,
    pub bypass_rls: bool,
    pub connection_limit: i32,
    pub valid_until: Option<String>,
    pub config: BTreeMap<String, String>,
    pub database_config: BTreeMap<String, BTreeMap<String, String>>,
    pub comment: Option<String>,
    pub security_labels: BTreeMap<String, String>,
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Membership {
    pub role: String,
    pub member: String,
    pub grantor: String,
    pub admin: bool,
    pub inherit: bool,
    pub set: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogContract {
    pub version: u32,
    pub provider: Provider,
    pub provider_context: crate::provider::Context,
    pub extension_owner: String,
    pub embedding_rows_present: bool,
    /// Explicit fixture/deployment contract: row count alone cannot establish
    /// whether the optional HNSW index was built or is eligible to be built.
    pub embedding_policy: EmbeddingPolicy,
    pub indexes: Vec<RuntimeIndex>,
    /// Every other index in the three product schemas; exact trusted metadata.
    pub relation_indexes: Vec<RuntimeIndex>,
    pub tiers: Vec<Role>,
    pub tier_memberships: Vec<Membership>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogSnapshot {
    pub version: u32,
    pub provider: Provider,
    pub provider_context: crate::provider::Context,
    pub extension_owner: String,
    pub embedding_rows_present: bool,
    pub indexes: Vec<RuntimeIndex>,
    /// Every other index in the three product schemas; exact trusted metadata.
    pub relation_indexes: Vec<RuntimeIndex>,
    pub roles: Vec<Role>,
    pub memberships: Vec<Membership>,
    /// All dependencies, including outside-schema and array row-type dependencies.
    pub rowtype_dependents: BTreeSet<String>,
}

fn keyed<T>(rows: &[T], key: impl Fn(&T) -> &str) -> Result<BTreeMap<&str, &T>, String> {
    let mut result = BTreeMap::new();
    for row in rows {
        let name = key(row);
        if result.insert(name, row).is_some() {
            return Err(format!("duplicate identity: {name}"));
        }
    }
    Ok(result)
}
fn tier(name: &str) -> bool {
    TIERS.contains(&name)
}

/// Exact structured identities, presence, and tier closure are checked before
/// any installation-specific external login memberships are ignored.
///
/// # Errors
/// Rejects incomplete contracts, drifted metadata and unsafe privilege edges.
pub fn validate_contract(
    candidate: &CatalogSnapshot,
    expected: &CatalogContract,
) -> Result<(), String> {
    if expected.version != 1 || candidate.version != 1 {
        return Err("unsupported catalog contract version".into());
    }
    if candidate.provider_context != expected.provider_context
        || candidate.provider != expected.provider
        || candidate.extension_owner != expected.extension_owner
        || candidate.embedding_rows_present != expected.embedding_rows_present
    {
        return Err("catalog context differs from trusted contract".into());
    }
    if !candidate.rowtype_dependents.is_empty() {
        return Err("ROWTYPE_DEPENDENTS must be empty".into());
    }
    if keyed(&candidate.relation_indexes, |r| &r.name)?
        != keyed(&expected.relation_indexes, |r| &r.name)?
    {
        return Err("ordinary relation index identity or presence differs".into());
    }
    if candidate
        .relation_indexes
        .iter()
        .any(|r| [SEARCH_INDEX, EMBEDDING_INDEX].contains(&r.name.as_str()))
    {
        return Err("runtime index misclassified as ordinary relation index".into());
    }
    let indexes = keyed(&candidate.indexes, |r| &r.name)?;
    let pinned = keyed(&expected.indexes, |r| &r.name)?;
    let mut required = BTreeSet::new();
    if expected.provider != Provider::Plain {
        required.insert(SEARCH_INDEX);
    }
    if expected.embedding_policy == EmbeddingPolicy::Rebuilt
        && expected.provider_context.vector_schema.is_some()
        && (1..=2000).contains(&expected.provider_context.embedding_dimensions)
    {
        required.insert(EMBEDDING_INDEX);
    }
    if pinned.keys().copied().collect::<BTreeSet<_>>() != required {
        return Err("trusted contract has an incomplete or unexpected runtime-index set".into());
    }
    if indexes != pinned {
        return Err("runtime index identity or presence differs".into());
    }
    for index in indexes.values() {
        let (relation, am) = match index.name.as_str() {
            SEARCH_INDEX => ("pgokf.concepts", "bm25"),
            EMBEDDING_INDEX => ("pgokf.concept_embedding", "hnsw"),
            _ => return Err("unrecognized runtime index".into()),
        };
        if index.relation != relation
            || index.access_method != am
            || !index.valid
            || !index.ready
            || !index.live
            || index.extension_member
            || index.keys.is_empty()
            || index.owner != expected.extension_owner
        {
            return Err(format!("unsafe runtime index: {}", index.name));
        }
    }
    for index in indexes.values() {
        crate::provider::validate(index, expected.provider, &expected.provider_context)?;
    }
    validate_tiers(candidate, expected)
}

fn validate_tiers(candidate: &CatalogSnapshot, expected: &CatalogContract) -> Result<(), String> {
    let roles = keyed(&candidate.roles, |r| &r.name)?;
    let pinned_roles = keyed(&expected.tiers, |r| &r.name)?;
    if pinned_roles.keys().copied().collect::<BTreeSet<_>>() != TIERS.into_iter().collect() {
        return Err("trusted contract must contain exactly four tier roles".into());
    }
    let comments = tier_comments()?;
    for (name, role) in pinned_roles {
        if role.login
            || role.superuser
            || role.create_role
            || role.create_db
            || role.replication
            || role.bypass_rls
            || !role.inherit
            || role.connection_limit != -1
            || role.valid_until.is_some()
            || !role.config.is_empty()
            || !role.database_config.is_empty()
            || !role.security_labels.is_empty()
            || role.comment.as_ref() != comments.get(name)
        {
            return Err(format!("unsafe tier contract: {name}"));
        }
        if roles.get(name) != Some(&role) {
            return Err(format!("tier role differs: {name}"));
        }
    }
    let membership_set: BTreeSet<_> = candidate.memberships.iter().collect();
    if membership_set.len() != candidate.memberships.len() {
        return Err("duplicate membership".into());
    }
    let pinned_memberships: BTreeSet<_> = expected.tier_memberships.iter().collect();
    if pinned_memberships.len() != expected.tier_memberships.len() {
        return Err("duplicate contract membership".into());
    }
    let canonical_edges = BTreeSet::from([
        ("pgokf_reader", "pgokf_writer"),
        ("pgokf_writer", "pgokf_admin"),
    ]);
    if pinned_memberships
        .iter()
        .map(|r| (r.role.as_str(), r.member.as_str()))
        .collect::<BTreeSet<_>>()
        != canonical_edges
        || pinned_memberships
            .iter()
            .any(|r| r.admin || !r.inherit || !r.set || r.grantor != expected.extension_owner)
    {
        return Err("tier contract hierarchy or grant semantics differ".into());
    }
    let mut actual_tier_edges = BTreeSet::new();
    for edge in &candidate.memberships {
        if tier(&edge.member) {
            // This also refuses broad predefined roles and any external bridge
            // into a tier: no transitive escalation path can enter a tier.
            if !tier(&edge.role) {
                return Err(format!("external membership enters tier: {}", edge.member));
            }
            actual_tier_edges.insert(edge);
        } else if tier(&edge.role) {
            let login = roles
                .get(edge.member.as_str())
                .ok_or_else(|| format!("missing external role metadata: {}", edge.member))?;
            if !login.login
                || edge.admin
                || !edge.inherit
                || !edge.set
                || edge.grantor != expected.extension_owner
            {
                return Err(format!("unsafe external tier grant: {}", edge.member));
            }
            // A login may use tier access, but may not redistribute it through
            // another role, even without ADMIN OPTION on this original grant.
            if candidate.memberships.iter().any(|e| e.role == edge.member) {
                return Err(format!("onward external tier grant: {}", edge.member));
            }
        }
    }
    if actual_tier_edges != pinned_memberships {
        return Err("tier relationships differ".into());
    }
    Ok(())
}

/// Legacy OID-free inventory row encoding. Object identities are unique and
/// unknown definition fields are preserved during comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryRow(pub String, pub String, pub Value);

/// # Errors
/// Rejects malformed, empty or duplicate inventory identities and JSON keys.
pub fn parse_inventory(text: &str) -> Result<Vec<InventoryRow>, String> {
    let mut rows = Vec::new();
    let mut keys = BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: InventoryRow =
            parse_json(line).map_err(|e| format!("inventory line {}: {e}", index + 1))?;
        if !keys.insert((row.0.clone(), row.1.clone())) {
            return Err(format!("duplicate inventory identity: {} {}", row.0, row.1));
        }
        rows.push(row);
    }
    if rows.is_empty() {
        return Err("empty inventory".into());
    }
    Ok(rows)
}
const USERS_FRESH: &[&str] = &[
    "name",
    "role",
    "password_hash",
    "display_name",
    "provider",
    "created_at",
    "updated_at",
];
const USERS_DECLARED: &[&str] = &[
    "name",
    "role",
    "password_hash",
    "created_at",
    "updated_at",
    "display_name",
    "provider",
];
const SESSIONS_FRESH: &[&str] = &[
    "nonce",
    "subject",
    "mode",
    "provider",
    "expires_at",
    "created_at",
];
const SESSIONS_DECLARED: &[&str] = &[
    "nonce",
    "subject",
    "mode",
    "expires_at",
    "created_at",
    "provider",
];

/// Only the two compiled, complete physical-order exceptions are allowed.
/// Runtime-index/role rows are deliberately retained here. Callers comparing
/// different runtime state must first validate typed snapshots and explicitly
/// select member rows; this function cannot silently erase security state.
///
/// # Errors
/// Rejects malformed inventory rows and undeclared column orders.
pub fn canonical_inventory(text: &str, allow_declared_order: bool) -> Result<Vec<String>, String> {
    canonical_inventory_mode(text, allow_declared_order, true)
}
fn canonical_inventory_mode(
    text: &str,
    allow_declared_order: bool,
    check_orders: bool,
) -> Result<Vec<String>, String> {
    let mut rows = parse_inventory(text)?;
    let mut columns: BTreeMap<String, BTreeMap<i64, String>> = BTreeMap::new();
    for row in &rows {
        if row.0 == "column" {
            let (relation, column) = row
                .1
                .rsplit_once('.')
                .ok_or("column lacks relation identity")?;
            let position = row
                .2
                .get(0)
                .and_then(Value::as_i64)
                .filter(|n| *n > 0)
                .ok_or("invalid live column position")?;
            if columns
                .entry(relation.into())
                .or_default()
                .insert(position, column.into())
                .is_some()
            {
                return Err(format!("duplicate column position in {relation}"));
            }
        }
    }
    for (name, fresh, declared) in [
        ("pgokf_web.users", USERS_FRESH, USERS_DECLARED),
        ("pgokf_web.sessions", SESSIONS_FRESH, SESSIONS_DECLARED),
    ] {
        if !check_orders {
            continue;
        }
        let order: Vec<_> = columns
            .get(name)
            .ok_or_else(|| format!("required relation missing: {name}"))?
            .values()
            .map(String::as_str)
            .collect();
        if order != fresh && !(allow_declared_order && order == declared) {
            return Err(format!("undeclared physical column order: {name}"));
        }
    }
    for row in &mut rows {
        if row.0 == "column" {
            let (relation, column) = row
                .1
                .rsplit_once('.')
                .ok_or("column lacks relation identity")?;
            let definition = row
                .2
                .as_array_mut()
                .ok_or("column definition is not an array")?;
            let ordinal = columns[relation]
                .values()
                .position(|c| c == column)
                .ok_or("column missing from order")?
                + 1;
            definition[0] = if allow_declared_order
                && ["pgokf_web.users", "pgokf_web.sessions"].contains(&relation)
            {
                Value::String("declared-order".into())
            } else {
                Value::from(ordinal)
            };
        }
        let acl = match row.0.as_str() {
            "function" => Some(2),
            "relation" => Some(7),
            "column" | "type" => Some(8),
            "schema" | "initial privileges" => Some(1),
            _ => None,
        };
        if let Some(at) = acl {
            let value = row
                .2
                .get_mut(at)
                .ok_or_else(|| format!("{} definition lacks ACL field", row.0))?;
            if let Some(list) = value.as_array_mut() {
                list.sort_by_key(Value::to_string);
            } else if !value.is_null() {
                return Err("ACL must be a list or null".into());
            }
        }
        if row.0 == "default acl" {
            if let Some(list) = row.2.as_array_mut() {
                list.sort_by_key(Value::to_string);
            } else if !row.2.is_null() {
                return Err("default ACL must be a list or null".into());
            }
        }
    }
    let mut output = rows
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    output.sort();
    Ok(output)
}

/// # Errors
/// Rejects invalid inventories or differences after permitted normalization.
pub fn compare_inventory(
    expected: &str,
    actual: &str,
    allow_declared_order: bool,
) -> Result<(), String> {
    if canonical_inventory(expected, allow_declared_order)?
        != canonical_inventory(actual, allow_declared_order)?
    {
        return Err(format!(
            "catalog drift outside exact declared column-order exceptions: {:?}",
            differing_keys(expected, actual)?
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn role(name: &str) -> Role {
        Role {
            name: name.into(),
            superuser: false,
            inherit: true,
            create_role: false,
            create_db: false,
            login: false,
            replication: false,
            bypass_rls: false,
            connection_limit: -1,
            valid_until: None,
            config: BTreeMap::new(),
            database_config: BTreeMap::new(),
            comment: tier_comments().unwrap().get(name).cloned(),
            security_labels: BTreeMap::new(),
        }
    }
    fn index() -> RuntimeIndex {
        RuntimeIndex {
            name: SEARCH_INDEX.into(),
            relation: "pgokf.concepts".into(),
            access_method: "bm25".into(),
            keys: vec![IndexKey {
                expression: crate::provider::TEXT_EXPRESSION.into(),
                opclass: "public.text_bm25_ops".into(),
                collation: Some("\"default\"".into()),
                options: 0,
            }],
            included_columns: vec![],
            predicate: None,
            valid: true,
            ready: true,
            live: true,
            unique: false,
            primary: false,
            exclusion: false,
            immediate: true,
            clustered: false,
            replica_identity: false,
            nulls_not_distinct: false,
            owner: "postgres".into(),
            acl: BTreeSet::new(),
            comment: None,
            security_labels: BTreeMap::new(),
            options: BTreeMap::from([("text_config".into(), "pg_catalog.english".into())]),
            tablespace: "pg_default".into(),
            persistence: "p".into(),
            extension_member: false,
        }
    }
    #[test]
    fn shared_provider_metadata_forgeries_fail() {
        let (snapshot, contract) = fixture();
        for field in [
            "schema",
            "configuration",
            "collation",
            "predicate",
            "options",
            "expression",
            "access_method",
        ] {
            let mut candidate = snapshot.clone();
            let mut expected = contract.clone();
            match field {
                "access_method" => candidate.indexes[0].access_method = "btree".into(),
                "schema" => candidate.indexes[0].keys[0].opclass = "foreign.text_bm25_ops".into(),
                "configuration" => {
                    candidate.indexes[0]
                        .options
                        .insert("text_config".into(), "pg_catalog.simple".into());
                }
                "collation" => candidate.indexes[0].keys[0].collation = Some("C".into()),
                "predicate" => candidate.indexes[0].predicate = Some("true".into()),
                "options" => candidate.indexes[0].keys[0].options = 1,
                _ => candidate.indexes[0].keys[0].expression = "title".into(),
            }
            expected.indexes = candidate.indexes.clone();
            crate::semantic_assert!(
                "shared_provider_metadata_forgeries_fail",
                validate_contract(&candidate, &expected).is_err(),
                "{field}"
            );
        }
    }
    #[test]
    fn shared_search_and_vector_keys_fail() {
        let (mut snapshot, mut contract) = fixture();
        let mut index = snapshot.indexes[0].clone();
        snapshot.provider = Provider::PgSearch;
        snapshot.provider_context.search_opclasses = vec!["public.anyelement_bm25_ops".into(); 5];
        index.keys = ["id", "title", "description", "body_text", "type"]
            .iter()
            .map(|name| IndexKey {
                expression: (*name).into(),
                opclass: "public.anyelement_bm25_ops".into(),
                collation: Some("\"default\"".into()),
                options: 0,
            })
            .collect();
        index.options = BTreeMap::from([("key_field".into(), "id".into())]);
        snapshot.indexes = vec![index];
        contract.provider = snapshot.provider;
        contract.provider_context = snapshot.provider_context.clone();
        contract.indexes = snapshot.indexes.clone();
        validate_contract(&snapshot, &contract).unwrap();
        snapshot.indexes[0].keys[0].opclass = "foreign.anyelement_bm25_ops".into();
        contract.indexes = snapshot.indexes.clone();
        crate::semantic_assert!(
            "shared_search_and_vector_keys_fail",
            validate_contract(&snapshot, &contract).is_err()
        );
        let (mut snapshot, mut contract) = fixture();
        let mut index = snapshot.indexes[0].clone();
        index.name = EMBEDDING_INDEX.into();
        index.relation = "pgokf.concept_embedding".into();
        index.access_method = "hnsw".into();
        index.options.clear();
        index.keys = vec![IndexKey {
            expression: "(embedding::public.vector(2))".into(),
            opclass: "public.vector_cosine_ops".into(),
            collation: None,
            options: 0,
        }];
        snapshot.provider = Provider::Plain;
        snapshot.provider_context.vector_schema = Some("public".into());
        snapshot.provider_context.embedding_dimensions = 2;
        snapshot.indexes = vec![index];
        contract.provider = snapshot.provider;
        contract.provider_context = snapshot.provider_context.clone();
        contract.embedding_policy = EmbeddingPolicy::Rebuilt;
        contract.indexes = snapshot.indexes.clone();
        validate_contract(&snapshot, &contract).unwrap();
        for expression in [
            "(embedding::public.vector(3))",
            "(embedding::foreign.vector(2))",
            "(embedding::public.vector(2)) + 1",
        ] {
            snapshot.indexes[0].keys[0].expression = expression.into();
            contract.indexes = snapshot.indexes.clone();
            crate::semantic_assert!(
                "shared_search_and_vector_keys_fail",
                validate_contract(&snapshot, &contract).is_err()
            );
        }
    }
    #[test]
    fn shared_forged_provider_shape_fails() {
        let (mut snapshot, mut contract) = fixture();
        snapshot.indexes[0].keys[0].expression = "title".into();
        contract.indexes = snapshot.indexes.clone();
        assert!(validate_contract(&snapshot, &contract).is_err());
    }
    #[test]
    fn shared_tier_annotations_fail() {
        let (mut snapshot, mut contract) = fixture();
        snapshot.roles[0].comment = Some("forged".into());
        contract.tiers = snapshot.roles.clone();
        crate::semantic_assert!(
            "shared_tier_annotations_fail",
            validate_contract(&snapshot, &contract).is_err()
        );
        let (mut snapshot, mut contract) = fixture();
        snapshot.roles[0]
            .security_labels
            .insert("foreign".into(), "label".into());
        contract.tiers = snapshot.roles.clone();
        crate::semantic_assert!(
            "shared_tier_annotations_fail",
            validate_contract(&snapshot, &contract).is_err()
        );
    }
    #[test]
    fn shared_database_config_fails() {
        let (mut snapshot, mut contract) = fixture();
        snapshot.roles[0].database_config.insert(
            "db".into(),
            BTreeMap::from([("row_security".into(), "off".into())]),
        );
        contract.tiers = snapshot.roles.clone();
        crate::semantic_assert!(
            "shared_database_config_fails",
            validate_contract(&snapshot, &contract).is_err()
        );
    }
    #[test]
    fn unexpected_ordinary_index_is_rejected() {
        let (mut snapshot, contract) = fixture();
        let mut extra = index();
        extra.name = "pgokf.third_runtime_idx".into();
        snapshot.relation_indexes.push(extra);
        assert!(validate_contract(&snapshot, &contract).is_err());
    }
    #[test]
    fn ordinary_index_metadata_stays_visible() {
        let (mut snapshot, mut contract) = fixture();
        let mut ordinary = index();
        ordinary.name = "pgokf.static_index".into();
        snapshot.relation_indexes.push(ordinary.clone());
        contract.relation_indexes.push(ordinary);
        crate::semantic_assert!(
            "ordinary_index_metadata_stays_visible",
            validate_contract(&snapshot, &contract).is_ok()
        );
        snapshot.relation_indexes[0].comment = Some("forged".into());
        crate::semantic_assert!(
            "ordinary_index_metadata_stays_visible",
            validate_contract(&snapshot, &contract).is_err()
        );
    }
    #[test]
    fn runtime_index_cannot_be_reclassified() {
        let (mut snapshot, mut contract) = fixture();
        snapshot.relation_indexes.push(index());
        contract.relation_indexes.push(index());
        assert!(validate_contract(&snapshot, &contract).is_err());
    }
    fn fixture() -> (CatalogSnapshot, CatalogContract) {
        let memberships = vec![
            Membership {
                role: "pgokf_reader".into(),
                member: "pgokf_writer".into(),
                grantor: "postgres".into(),
                admin: false,
                inherit: true,
                set: true,
            },
            Membership {
                role: "pgokf_writer".into(),
                member: "pgokf_admin".into(),
                grantor: "postgres".into(),
                admin: false,
                inherit: true,
                set: true,
            },
        ];
        let contract = CatalogContract {
            version: 1,
            provider: Provider::PgTextsearch,
            provider_context: crate::provider::Context {
                provider_schema: Some("public".into()),
                search_opclasses: vec![],
                text_config: "pg_catalog.english".into(),
                vector_schema: None,
                embedding_dimensions: 1536,
            },
            extension_owner: "postgres".into(),
            embedding_rows_present: false,
            embedding_policy: EmbeddingPolicy::NotRequested,
            indexes: vec![index()],
            relation_indexes: vec![],
            tiers: TIERS.iter().map(|n| role(n)).collect(),
            tier_memberships: memberships,
        };
        let snapshot = CatalogSnapshot {
            version: contract.version,
            provider: contract.provider,
            provider_context: contract.provider_context.clone(),
            extension_owner: contract.extension_owner.clone(),
            embedding_rows_present: false,
            indexes: contract.indexes.clone(),
            relation_indexes: contract.relation_indexes.clone(),
            roles: contract.tiers.clone(),
            memberships: contract.tier_memberships.clone(),
            rowtype_dependents: BTreeSet::new(),
        };
        (snapshot, contract)
    }
    #[test]
    fn trusted_contract_accepts() {
        let (s, c) = fixture();
        assert_eq!(validate_contract(&s, &c), Ok(()));
    }
    #[test]
    fn required_runtime_index_cannot_disappear() {
        let (mut s, c) = fixture();
        s.indexes.clear();
        crate::semantic_assert!(
            "required_runtime_index_cannot_disappear",
            validate_contract(&s, &c).is_err()
        );
    }
    #[test]
    fn third_runtime_index_cannot_appear() {
        let (mut s, c) = fixture();
        let mut extra = index();
        extra.name = "pgokf.third_idx".into();
        s.indexes.push(extra);
        assert!(validate_contract(&s, &c).is_err());
    }
    #[test]
    fn unsafe_tier_even_in_reference_fails() {
        let (mut s, mut c) = fixture();
        s.roles[0].bypass_rls = true;
        c.tiers[0].bypass_rls = true;
        crate::semantic_assert!(
            "unsafe_tier_even_in_reference_fails",
            validate_contract(&s, &c).is_err()
        );
    }
    #[test]
    fn legitimate_login_noise_passes() {
        let (mut s, c) = fixture();
        let mut login = role("application_user");
        login.login = true;
        s.roles.push(login);
        s.memberships.push(Membership {
            role: "pgokf_reader".into(),
            member: "application_user".into(),
            grantor: "postgres".into(),
            admin: false,
            inherit: true,
            set: true,
        });
        assert_eq!(validate_contract(&s, &c), Ok(()));
    }
    #[test]
    fn broad_predefined_membership_fails() {
        let (mut s, c) = fixture();
        s.memberships.push(Membership {
            role: "pg_read_all_data".into(),
            member: "pgokf_reader".into(),
            grantor: "postgres".into(),
            admin: false,
            inherit: true,
            set: true,
        });
        crate::semantic_assert!(
            "broad_predefined_membership_fails",
            validate_contract(&s, &c).is_err()
        );
    }
    #[test]
    fn onward_login_grant_fails() {
        let (mut s, c) = fixture();
        let mut login = role("application_user");
        login.login = true;
        s.roles.push(login);
        s.memberships.push(Membership {
            role: "pgokf_reader".into(),
            member: "application_user".into(),
            grantor: "postgres".into(),
            admin: false,
            inherit: true,
            set: true,
        });
        s.memberships.push(Membership {
            role: "application_user".into(),
            member: "bridge".into(),
            grantor: "postgres".into(),
            admin: false,
            inherit: true,
            set: true,
        });
        crate::semantic_assert!(
            "onward_login_grant_fails",
            validate_contract(&s, &c).is_err()
        );
    }
    #[test]
    fn rowtype_dependency_fails() {
        let (mut s, c) = fixture();
        s.rowtype_dependents.insert("public.foreign_view".into());
        crate::semantic_assert!(
            "rowtype_dependency_fails",
            validate_contract(&s, &c).is_err()
        );
    }
    #[test]
    fn plain_requires_no_search_index() {
        let (mut s, mut c) = fixture();
        s.provider = Provider::Plain;
        c.provider = Provider::Plain;
        assert!(validate_contract(&s, &c).is_err());
        s.indexes.clear();
        c.indexes.clear();
        assert_eq!(validate_contract(&s, &c), Ok(()));
    }
    #[test]
    fn embedding_presence_uses_explicit_contract_not_row_count() {
        let (mut snapshot, mut contract) = fixture();
        snapshot.embedding_rows_present = true;
        contract.embedding_rows_present = true;
        assert_eq!(validate_contract(&snapshot, &contract), Ok(()));
        snapshot.embedding_rows_present = false;
        contract.embedding_rows_present = false;
        contract.embedding_policy = EmbeddingPolicy::Rebuilt;
        let mut embedding = index();
        embedding.name = EMBEDDING_INDEX.into();
        embedding.relation = "pgokf.concept_embedding".into();
        embedding.access_method = "hnsw".into();
        embedding.keys = vec![IndexKey {
            expression: "(embedding::public.vector(3))".into(),
            opclass: "public.vector_cosine_ops".into(),
            collation: None,
            options: 0,
        }];
        snapshot.provider_context.vector_schema = Some("public".into());
        snapshot.provider_context.embedding_dimensions = 3;
        contract.provider_context = snapshot.provider_context.clone();
        crate::semantic_assert!(
            "embedding_presence_uses_explicit_contract_not_row_count",
            validate_contract(&snapshot, &contract).is_err(),
            "required HNSW absent from both"
        );
        embedding.options.clear();
        contract.indexes.push(embedding.clone());
        snapshot.indexes.push(embedding);
        assert_eq!(validate_contract(&snapshot, &contract), Ok(()));
        snapshot.indexes.pop();
        crate::semantic_assert!(
            "embedding_presence_uses_explicit_contract_not_row_count",
            validate_contract(&snapshot, &contract).is_err()
        );
        contract.indexes.pop();
        crate::semantic_assert!(
            "embedding_presence_uses_explicit_contract_not_row_count",
            validate_contract(&snapshot, &contract).is_err(),
            "shared missing required index"
        );
        snapshot.provider_context.embedding_dimensions = 2001;
        contract.provider_context = snapshot.provider_context.clone();
        crate::semantic_assert!(
            "embedding_presence_uses_explicit_contract_not_row_count",
            validate_contract(&snapshot, &contract).is_ok(),
            "ineligible HNSW must remain absent"
        );
    }
    /// Semantic input mutations, not removal/substitution of implementation
    /// checks. Each independent struct field must influence the verdict.
    #[test]
    fn every_index_field_is_load_bearing() {
        let (snapshot, contract) = fixture();
        let original = serde_json::to_value(&snapshot.indexes[0]).unwrap();
        let fields = original.as_object().unwrap();
        for (name, value) in fields {
            let mut mutated = original.clone();
            mutated[name] = match value {
                Value::Bool(v) => Value::Bool(!v),
                Value::String(v) => Value::String(format!("{v}_forged")),
                Value::Array(v) => {
                    let mut a = v.clone();
                    if a.is_empty() {
                        a.push(Value::String("forged".into()));
                    } else {
                        a.clear();
                    }
                    Value::Array(a)
                }
                Value::Object(_) if name == "database_config" => {
                    serde_json::json!({"db":{"forged":"forged"}})
                }
                Value::Object(_) => serde_json::json!({"forged":"forged"}),
                Value::Null => Value::String("forged".into()),
                Value::Number(_) => panic!("uncovered index field {name}"),
            };
            let mut s = snapshot.clone();
            match serde_json::from_value(mutated) {
                Ok(index) => {
                    s.indexes[0] = index;
                    assert!(validate_contract(&s, &contract).is_err(), "field {name}");
                }
                Err(e) => panic!("mutation {name} did not compile into semantic input: {e}"),
            }
        }
    }
    #[test]
    fn shared_index_fields_fail() {
        let (snapshot, contract) = fixture();
        let original = serde_json::to_value(&snapshot.indexes[0]).unwrap();
        let fields = original.as_object().unwrap();
        for (name, value) in fields {
            let mut mutated = original.clone();
            mutated[name] = match value {
                Value::Bool(v) => Value::Bool(!v),
                Value::String(v) => Value::String(format!("{v}_forged")),
                Value::Array(v) => {
                    let mut a = v.clone();
                    if a.is_empty() {
                        a.push(Value::String("forged".into()));
                    } else {
                        a.clear();
                    }
                    Value::Array(a)
                }
                Value::Object(_) if name == "database_config" => {
                    serde_json::json!({"db":{"forged":"forged"}})
                }
                Value::Object(_) => serde_json::json!({"forged":"forged"}),
                Value::Null => Value::String("forged".into()),
                Value::Number(_) => panic!("uncovered index field {name}"),
            };
            let mut s = snapshot.clone();
            match serde_json::from_value(mutated) {
                Ok(index) => {
                    s.indexes[0] = index;
                    let mut contract = contract.clone();
                    contract.indexes = s.indexes.clone();
                    crate::semantic_assert!(
                        "shared_index_fields_fail",
                        validate_contract(&s, &contract).is_err(),
                        "field {name}"
                    );
                }
                Err(e) => panic!("mutation {name} did not compile into semantic input: {e}"),
            }
        }
    }
    #[test]
    fn index_key_metadata_is_load_bearing() {
        let (snapshot, contract) = fixture();
        for field in ["expression", "opclass", "collation", "options"] {
            let mut value = serde_json::to_value(&snapshot).unwrap();
            value["indexes"][0]["keys"][0][field] = if field == "options" {
                Value::from(1)
            } else {
                Value::String("forged".into())
            };
            let s = serde_json::from_value(value).unwrap();
            assert!(validate_contract(&s, &contract).is_err(), "{field}");
        }
    }
    #[test]
    fn every_tier_field_is_load_bearing() {
        let (snapshot, contract) = fixture();
        let original = serde_json::to_value(&snapshot.roles[0]).unwrap();
        for (name, value) in original.as_object().unwrap() {
            let mut mutated = original.clone();
            mutated[name] = match value {
                Value::Bool(v) => Value::Bool(!v),
                Value::String(v) => Value::String(format!("{v}_forged")),
                Value::Number(_) => Value::from(1),
                Value::Object(_) if name == "database_config" => {
                    serde_json::json!({"db":{"forged":"forged"}})
                }
                Value::Object(_) => serde_json::json!({"forged":"forged"}),
                Value::Null => Value::String("forged".into()),
                Value::Array(_) => panic!("uncovered role field {name}"),
            };
            let mut s = snapshot.clone();
            s.roles[0] = serde_json::from_value(mutated).unwrap();
            crate::semantic_assert!(
                "every_tier_field_is_load_bearing",
                validate_contract(&s, &contract).is_err(),
                "{name}"
            );
        }
    }
    #[test]
    fn shared_tier_fields_fail() {
        let (snapshot, contract) = fixture();
        let original = serde_json::to_value(&snapshot.roles[0]).unwrap();
        for (name, value) in original.as_object().unwrap() {
            let mut mutated = original.clone();
            mutated[name] = match value {
                Value::Bool(v) => Value::Bool(!v),
                Value::String(v) => Value::String(format!("{v}_forged")),
                Value::Number(_) => Value::from(1),
                Value::Object(_) if name == "database_config" => {
                    serde_json::json!({"db":{"forged":"forged"}})
                }
                Value::Object(_) => serde_json::json!({"forged":"forged"}),
                Value::Null => Value::String("forged".into()),
                Value::Array(_) => panic!("uncovered role field {name}"),
            };
            let mut s = snapshot.clone();
            s.roles[0] = serde_json::from_value(mutated).unwrap();
            let mut contract = contract.clone();
            contract.tiers = s.roles.clone();
            crate::semantic_assert!(
                "shared_tier_fields_fail",
                validate_contract(&s, &contract).is_err(),
                "{name}"
            );
        }
    }
    #[test]
    fn every_membership_field_is_load_bearing() {
        let (snapshot, contract) = fixture();
        let original = serde_json::to_value(&snapshot.memberships[0]).unwrap();
        for (name, value) in original.as_object().unwrap() {
            let mut mutated = original.clone();
            mutated[name] = match value {
                Value::Bool(v) => Value::Bool(!v),
                Value::String(v) => Value::String(format!("{v}_forged")),
                _ => panic!("uncovered membership field {name}"),
            };
            let mut s = snapshot.clone();
            s.memberships[0] = serde_json::from_value(mutated).unwrap();
            assert!(validate_contract(&s, &contract).is_err(), "{name}");
        }
    }
    #[test]
    fn shared_membership_fields_fail() {
        let (snapshot, contract) = fixture();
        for field in ["admin", "inherit", "set", "grantor"] {
            let mut s = snapshot.clone();
            match field {
                "admin" => s.memberships[0].admin = true,
                "inherit" => s.memberships[0].inherit = false,
                "set" => s.memberships[0].set = false,
                _ => s.memberships[0].grantor = "foreign".into(),
            }
            let mut c = contract.clone();
            c.tier_memberships = s.memberships.clone();
            crate::semantic_assert!(
                "shared_membership_fields_fail",
                validate_contract(&s, &c).is_err(),
                "{field}"
            );
        }
    }
    #[test]
    fn external_membership_semantics() {
        let (mut s, c) = fixture();
        let mut login = role("app");
        login.login = true;
        s.roles.push(login);
        s.memberships.push(Membership {
            role: "pgokf_reader".into(),
            member: "app".into(),
            grantor: "postgres".into(),
            admin: false,
            inherit: true,
            set: true,
        });
        validate_contract(&s, &c).unwrap();
        for field in ["login", "admin", "inherit", "set", "grantor"] {
            let mut bad = s.clone();
            match field {
                "login" => bad.roles.last_mut().unwrap().login = false,
                "admin" => bad.memberships.last_mut().unwrap().admin = true,
                "inherit" => bad.memberships.last_mut().unwrap().inherit = false,
                "set" => bad.memberships.last_mut().unwrap().set = false,
                _ => bad.memberships.last_mut().unwrap().grantor = "foreign".into(),
            }
            crate::semantic_assert!(
                "external_membership_semantics",
                validate_contract(&bad, &c).is_err(),
                "{field}"
            );
        }
    }
    #[test]
    fn catalog_context_is_bound() {
        let (s, c) = fixture();
        for field in ["version", "provider", "owner", "embedding_rows", "context"] {
            let mut bad = s.clone();
            match field {
                "version" => bad.version = 2,
                "provider" => bad.provider = Provider::PgSearch,
                "owner" => bad.extension_owner = "foreign".into(),
                "embedding_rows" => bad.embedding_rows_present = true,
                _ => bad.provider_context.text_config = "foreign".into(),
            }
            crate::semantic_assert!(
                "catalog_context_is_bound",
                validate_contract(&bad, &c).is_err(),
                "{field}"
            );
        }
    }
    fn inventory(declared: bool) -> String {
        let mut lines = Vec::new();
        for (name, columns) in [
            (
                "pgokf_web.users",
                if declared {
                    USERS_DECLARED
                } else {
                    USERS_FRESH
                },
            ),
            (
                "pgokf_web.sessions",
                if declared {
                    SESSIONS_DECLARED
                } else {
                    SESSIONS_FRESH
                },
            ),
        ] {
            for (i, column) in columns.iter().enumerate() {
                lines.push(
                    serde_json::json!([
                        "column",
                        format!("{name}.{column}"),
                        [
                            i + 1,
                            "text",
                            false,
                            "",
                            "",
                            "x",
                            "",
                            "default",
                            null,
                            null,
                            null
                        ]
                    ])
                    .to_string(),
                );
            }
        }
        lines.join("\n")
    }
    #[test]
    fn only_complete_declared_orders_normalize() {
        let fresh = inventory(false);
        let declared = inventory(true);
        assert!(compare_inventory(&fresh, &declared, false).is_err());
        assert_eq!(compare_inventory(&fresh, &declared, true), Ok(()));
    }
    #[test]
    fn third_order_rejected() {
        let fresh = inventory(false);
        let bad = fresh.replace("[1,\"text\"", "[99,\"text\"");
        crate::semantic_assert!(
            "third_order_rejected",
            compare_inventory(&fresh, &bad, true).is_err()
        );
    }
    #[test]
    fn metadata_on_exception_stays_visible() {
        let fresh = inventory(false);
        let bad = inventory(true).replace("\"text\"", "\"varchar\"");
        crate::semantic_assert!(
            "metadata_on_exception_stays_visible",
            compare_inventory(&fresh, &bad, true).is_err()
        );
        for field in [8, 9, 10] {
            let mut rows = parse_inventory(&inventory(true)).unwrap();
            rows[0].2[field] = if field == 8 {
                serde_json::json!(["pgokf_reader=r/postgres"])
            } else {
                Value::String("forged metadata".into())
            };
            let mutated = rows
                .iter()
                .map(|row| serde_json::to_string(row).unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            crate::semantic_assert!(
                "metadata_on_exception_stays_visible",
                compare_inventory(&fresh, &mutated, true).is_err(),
                "field {field}"
            );
        }
    }
    #[test]
    fn duplicate_identity_rejected() {
        let fresh = inventory(false);
        assert!(parse_inventory(&format!("{fresh}\n{fresh}")).is_err());
    }
}

/// Parse any contract through this boundary, rejecting duplicate object keys at
/// every depth before serde converts objects into typed maps or structs.
///
/// # Errors
/// Rejects malformed JSON, duplicate keys and type mismatches.
pub fn parse_json<T: serde::de::DeserializeOwned>(input: &str) -> Result<T, String> {
    crate::json::from_slice(input.as_bytes())
}

#[cfg(test)]
mod duplicate_json_tests {
    use super::*;
    #[test]
    fn nested_duplicate_keys_fail_closed() {
        assert!(
            parse_json::<Value>(
                r#"{"index":{"options":{"text_config":"english","text_config":"simple"}}}"#
            )
            .is_err()
        );
    }
    #[test]
    fn trailing_json_rejected() {
        assert!(parse_json::<Value>("{} {}").is_err());
    }
    #[test]
    fn valid_json_retains_types() {
        assert_eq!(
            parse_json::<Value>(r#"[1,-1,0.5,null,true,"x",{}]"#).unwrap(),
            serde_json::json!([1, -1, 0.5, null, true, "x", {}])
        );
    }
}

/// Changed identities after normalizing dropped attribute slots and ACL order.
/// # Errors
/// Rejects malformed or duplicate rows.
pub fn differing_keys(left: &str, right: &str) -> Result<BTreeSet<(String, String)>, String> {
    let keyed = |text: &str| -> Result<BTreeMap<(String, String), Value>, String> {
        canonical_inventory_mode(text, false, false)?
            .iter()
            .map(|s| {
                let r: InventoryRow = parse_json(s)?;
                Ok(((r.0, r.1), r.2))
            })
            .collect()
    };
    let a = keyed(left)?;
    let b = keyed(right)?;
    Ok(a.keys()
        .chain(b.keys())
        .filter(|k| a.get(*k) != b.get(*k))
        .cloned()
        .collect())
}

/// Public pre/post acceptance envelope; inventory is mandatory so relation
/// ownership, RLS, grants, routines, defaults and extension membership cannot be
/// omitted by a caller validating only runtime objects.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceContract {
    pub format_version: u32,
    pub catalog: CatalogContract,
    pub member_inventory: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceCapture {
    pub catalog: CatalogSnapshot,
    pub member_inventory: String,
}
/// # Errors
/// Rejects partial envelopes, unsafe ownership, runtime metadata and member drift.
pub fn validate_acceptance(
    expected: &AcceptanceContract,
    actual: &AcceptanceCapture,
    allow_declared_order: bool,
) -> Result<(), String> {
    if expected.format_version != 1 {
        return Err("unsupported acceptance envelope".into());
    }
    validate_contract(&actual.catalog, &expected.catalog)?;
    for text in [&expected.member_inventory, &actual.member_inventory] {
        let rows = parse_inventory(text)?;
        let tables: BTreeSet<_> = rows
            .iter()
            .filter(|r| r.0 == "relation" && r.2.get(0).and_then(Value::as_str) == Some("r"))
            .map(|r| r.1.as_str())
            .collect();
        for name in [
            "pgokf.concepts",
            "pgokf.bundles",
            "pgokf_web.users",
            "pgokf_web.sessions",
        ] {
            if !tables.contains(name) {
                return Err(format!("required relation missing: {name}"));
            }
        }
        validate_relation_grants(&rows, &expected.catalog.extension_owner)?;
        validate_tenant_policies(&rows)?;
        validate_relation_owners(&rows, &expected.catalog.extension_owner)?;
        for row in &rows {
            if row.0 == "relation"
                && row.1 == "pgokf.concepts"
                && row.2.get(2) != Some(&Value::Bool(true))
            {
                return Err("concept tenant RLS disabled".into());
            }
        }
    }
    compare_inventory(
        &member_inventory(&expected.member_inventory, &expected.catalog.indexes)?,
        &member_inventory(&actual.member_inventory, &actual.catalog.indexes)?,
        allow_declared_order,
    )
}
fn member_inventory(text: &str, indexes: &[RuntimeIndex]) -> Result<String, String> {
    let runtime: BTreeSet<_> = indexes.iter().map(|i| i.name.as_str()).collect();
    let rows = parse_inventory(text)?;
    let mut output = vec![];
    for row in rows {
        if ["role", "role membership"].contains(&row.0.as_str()) {
            continue;
        }
        if ["relation", "index state"].contains(&row.0.as_str()) && runtime.contains(row.1.as_str())
        {
            continue;
        }
        if row.0 == "column"
            && row
                .1
                .rsplit_once('.')
                .is_some_and(|(relation, _)| runtime.contains(relation))
        {
            continue;
        }
        output.push(serde_json::to_string(&row).map_err(|e| e.to_string())?);
    }
    Ok(output.join("\n"))
}

fn validate_relation_owners(rows: &[InventoryRow], owner: &str) -> Result<(), String> {
    for row in rows {
        if row.0 == "relation"
            && [Some("r"), Some("p"), Some("v"), Some("m"), Some("S")]
                .contains(&row.2.get(0).and_then(Value::as_str))
            && row.2.get(6).and_then(Value::as_str) != Some(owner)
        {
            return Err(format!("unsafe relation ownership: {}", row.1));
        }
    }
    Ok(())
}
#[cfg(test)]
mod owner_tests {
    use super::*;
    #[test]
    fn independent_relation_owner() {
        let mut row = InventoryRow(
            "relation".into(),
            "pgokf.concepts".into(),
            serde_json::json!(["r", "p", true, false, false, "", "postgres"]),
        );
        validate_relation_owners(std::slice::from_ref(&row), "postgres").unwrap();
        row.2[6] = Value::String("pgokf_reader".into());
        crate::semantic_assert!(
            "independent_relation_owner",
            validate_relation_owners(&[row], "postgres").is_err()
        );
    }
}

fn validate_relation_grants(rows: &[InventoryRow], owner: &str) -> Result<(), String> {
    for row in rows.iter().filter(|row| row.0 == "column") {
        let acl = row.2.get(8).ok_or("column ACL metadata missing")?;
        if !acl.is_null() && acl.as_array().is_none_or(|entries| !entries.is_empty()) {
            return Err(format!("unsafe column grant: {}", row.1));
        }
    }

    let readers = BTreeSet::from([
        "pgokf.bundles",
        "pgokf.concepts",
        "pgokf.concept_metadata",
        "pgokf.bundle_log",
        "pgokf.concept_embedding",
        "pgokf.links",
        "pgokf.skills",
        "pgokf.scripts",
        "pgokf.reference_documents",
        "pgokf.concept_history",
        "pgokf.concept_provenance",
        "pgokf.concept_verification",
        "pgokf.concept_provenance_source",
        "pgokf.current_relationships",
        "pgokf.effective_freshness",
        "pgokf.concept_source",
    ]);
    let writers = BTreeSet::from([
        "pgokf_web.users",
        "pgokf_web.sessions",
        "pgokf_web.identity_providers",
        "pgokf_web.mcp_tokens",
    ]);
    let mut observed = BTreeSet::new();
    for row in rows.iter().filter(|r| r.0 == "relation grant") {
        let fields = row
            .2
            .as_array()
            .filter(|a| a.len() == 5)
            .ok_or("malformed relation grant")?;
        let relation = fields[0].as_str().ok_or("grant relation missing")?;
        let grantor = fields[1].as_str().ok_or("grantor missing")?;
        let grantee = fields[2].as_str().ok_or("grantee missing")?;
        let privilege = fields[3].as_str().ok_or("privilege missing")?;
        if grantee == owner && grantor == owner {
            continue;
        }
        if grantor != owner || fields[4] != Value::Bool(false) {
            return Err("relation grantor or grant option is unsafe".into());
        }
        let safe = grantee == "pgokf_reader" && readers.contains(relation) && privilege == "SELECT"
            || grantee == "pgokf_writer"
                && writers.contains(relation)
                && ["SELECT", "INSERT", "DELETE", "UPDATE"].contains(&privilege)
                && !(relation == "pgokf_web.mcp_tokens" && privilege == "UPDATE");
        if !safe {
            return Err(format!(
                "unsafe relation grant: {relation} {grantee} {privilege}"
            ));
        }
        observed.insert((relation, grantee, privilege));
    }
    for relation in readers {
        if !observed.contains(&(relation, "pgokf_reader", "SELECT")) {
            return Err(format!("required reader grant missing: {relation}"));
        }
    }
    for relation in writers {
        for privilege in ["SELECT", "INSERT", "UPDATE", "DELETE"] {
            if relation == "pgokf_web.mcp_tokens" && privilege == "UPDATE" {
                continue;
            }
            if !observed.contains(&(relation, "pgokf_writer", privilege)) {
                return Err(format!(
                    "required writer grant missing: {relation} {privilege}"
                ));
            }
        }
    }
    Ok(())
}

fn validate_tenant_policies(rows: &[InventoryRow]) -> Result<(), String> {
    const POLICY: &str = "((((current_setting('pgokf.tenant'::text, true) IS NULL) OR (current_setting('pgokf.tenant'::text, true) = ''::text)) AND (NOT ( SELECT pgokf.tenant_required() AS tenant_required))) OR (tenant_id = current_setting('pgokf.tenant'::text, true)))";
    let canonical = |s: &str| -> Result<Vec<(crate::sql::Kind, String)>, String> {
        Ok(crate::sql::lex(s)?
            .into_iter()
            .map(|t| {
                (
                    t.kind,
                    if t.kind == crate::sql::Kind::Word {
                        t.text.to_ascii_lowercase()
                    } else {
                        t.text
                    },
                )
            })
            .collect())
    };
    let shape = canonical(POLICY)?;
    for relation in ["pgokf.bundles", "pgokf.concepts", "pgokf.concept_metadata"] {
        let table = rows
            .iter()
            .find(|r| r.0 == "relation" && r.1 == relation)
            .ok_or("tenant relation missing")?;
        if table.2.get(2) != Some(&Value::Bool(true)) || table.2.get(3) != Some(&Value::Bool(false))
        {
            return Err(format!("unsafe tenant RLS flags: {relation}"));
        }
        let policies: Vec<_> = rows
            .iter()
            .filter(|r| {
                r.0 == "policy"
                    && r.1
                        .rsplit_once('.')
                        .is_some_and(|(table, _)| table == relation)
            })
            .collect();
        if policies.len() != 1 {
            return Err(format!("tenant policy count differs: {relation}"));
        }
        let policy = policies[0];
        let name = relation
            .split('.')
            .next_back()
            .ok_or("relation lacks name")?;
        if policy.1 != format!("{relation}.{name}_tenant_isolation")
            || policy.2.get(0) != Some(&Value::String("*".into()))
            || policy.2.get(1) != Some(&Value::Bool(true))
            || policy.2.get(2) != Some(&serde_json::json!(["public"]))
        {
            return Err(format!("unsafe tenant policy scope: {relation}"));
        }
        for at in [3, 4] {
            if canonical(
                policy
                    .2
                    .get(at)
                    .and_then(Value::as_str)
                    .ok_or("tenant policy expression missing")?,
            )? != shape
            {
                return Err(format!("unsafe tenant policy expression: {relation}"));
            }
        }
    }
    Ok(())
}

/// # Errors
/// Only the reviewed column orders, runtime identities and dynamic statements
/// may be declared; explanatory prose does not affect the contract.
pub fn check_exceptions(text: &str) -> Result<(), String> {
    crate::positional::check_exceptions(text.as_bytes())
        .map_err(|error| format!("catalog exceptions changed: {error}"))
}

fn tier_comments() -> Result<BTreeMap<String, String>, String> {
    let tokens = crate::sql::lex(include_str!("../../extension/sql/bootstrap.sql"))?;
    let mut comments = BTreeMap::new();
    for part in tokens.windows(6) {
        if part[0].text.eq_ignore_ascii_case("comment")
            && part[1].text.eq_ignore_ascii_case("on")
            && part[2].text.eq_ignore_ascii_case("role")
            && part[4].text.eq_ignore_ascii_case("is")
            && TIERS.contains(&part[3].text.as_str())
        {
            let value = part[5]
                .text
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
                .ok_or("tier comment must be a literal")?
                .replace("''", "'");
            if comments.insert(part[3].text.clone(), value).is_some() {
                return Err("duplicate shipped tier comment".into());
            }
        }
    }
    if comments.len() != TIERS.len() {
        return Err("shipped tier comment missing".into());
    }
    Ok(comments)
}
