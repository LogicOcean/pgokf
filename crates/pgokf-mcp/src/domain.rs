// SPDX-License-Identifier: AGPL-3.0-only
//! Typed successful Tool values; serialization retains legacy JSON exactly.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};
use serde_json::{Number, Value};

use crate::{registry::Handler, service::DomainError};

/// One successful catalog operation, before transport envelopes or resource links.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum ToolValue {
    ConceptSearch(Vec<SearchHit>),
    FindSimilar(Vec<SearchHit>),
    ConceptNeighbors(Vec<LinkNeighbor>),
    TraverseConcepts(Vec<TraversalNode>),
    GetConcept(Vec<ConceptRecord>),
    GetSkill(Box<SkillRecord>),
    ListBundles(Vec<Bundle>),
    CreateContentBundle(CreatedBundle),
    RefreshBundle(RefreshedBundle),
    SetBundleState(BundleState),
    DeleteDocument(DeletedDocument),
    PutDocument(WrittenDocument),
    CheckWorkspacePluginFreshness(PluginCheck),
    ListPluginTargets(Vec<PluginTarget>),
    BuildWorkspacePlugin(Box<BuildOutcome>),
}

impl ToolValue {
    pub fn parse(handler: Handler, value: Value) -> Result<Self, DomainError> {
        let result = match handler {
            Handler::ConceptSearch => serde_json::from_value(value).map(Self::ConceptSearch),
            Handler::FindSimilar => serde_json::from_value(value).map(Self::FindSimilar),
            Handler::ConceptNeighbors => serde_json::from_value(value).map(Self::ConceptNeighbors),
            Handler::TraverseConcepts => serde_json::from_value(value).map(Self::TraverseConcepts),
            Handler::GetConcept => serde_json::from_value(value).map(Self::GetConcept),
            Handler::GetSkill => serde_json::from_value(value).map(Self::GetSkill),
            Handler::ListBundles => serde_json::from_value(value).map(Self::ListBundles),
            Handler::CreateContentBundle => {
                serde_json::from_value(value).map(Self::CreateContentBundle)
            }
            Handler::RefreshBundle => serde_json::from_value(value).map(Self::RefreshBundle),
            Handler::SetBundleState => serde_json::from_value(value).map(Self::SetBundleState),
            Handler::DeleteDocument => serde_json::from_value(value).map(Self::DeleteDocument),
            Handler::PutDocument => serde_json::from_value(value).map(Self::PutDocument),
            Handler::CheckWorkspacePluginFreshness => {
                serde_json::from_value(value).map(Self::CheckWorkspacePluginFreshness)
            }
            Handler::ListPluginTargets => {
                serde_json::from_value(value).map(Self::ListPluginTargets)
            }
            Handler::BuildWorkspacePlugin => {
                serde_json::from_value(value).map(Self::BuildWorkspacePlugin)
            }
        };
        result.map_err(|_| DomainError::internal())
    }

    pub fn into_json(self) -> Value {
        // Every leaf is an already validated JSON scalar, string-keyed map,
        // or record. There are no non-finite floats or non-string map keys.
        serde_json::to_value(self).expect("typed catalog values are JSON representable")
    }
}

/// Required field whose value may be JSON null. Missing is an error.
#[derive(Debug, Serialize)]
#[serde(transparent)]
struct Nullable<T>(Option<T>);

impl<'de, T: DeserializeOwned> Deserialize<'de> for Nullable<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // deserialize_any distinguishes a missing field from an explicit null;
        // ordinary Option<T> deliberately treats both the same way.
        let value = Value::deserialize(deserializer)?;
        serde_json::from_value(value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// Optional field, retaining a present null when T itself is nullable.
#[derive(Debug, Default)]
enum Presence<T> {
    #[default]
    Missing,
    Present(T),
}
impl<T> Presence<T> {
    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
}
impl<T: Serialize> Serialize for Presence<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Missing => serializer.serialize_none(),
            Self::Present(value) => value.serialize(serializer),
        }
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Presence<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self::Present)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchHit {
    bundle_id: i64,
    concept_id: String,
    path: String,
    title: Nullable<String>,
    #[serde(rename = "type")]
    kind: Nullable<String>,
    headline: Nullable<String>,
    rank: Number,
    #[serde(default, skip_serializing_if = "Presence::is_missing")]
    uri: Presence<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LinkNeighbor {
    source_id: String,
    neighbor_id: String,
    hops: i64,
    path: Vec<String>,
    title: Nullable<String>,
    #[serde(default, skip_serializing_if = "Presence::is_missing")]
    uri: Presence<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConceptIdentity {
    bundle_id: i64,
    concept_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Freshness {
    state: String,
    reasons: Vec<String>,
}

/// Generation-stamped observation accompanying compatibility Tool results.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatalogEvidence {
    bundle_id: i64,
    concept_id: String,
    catalog_generation: i64,
    sync_hash: Nullable<String>,
    freshness: Freshness,
    uri: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Direction {
    Outbound,
    Inbound,
    Both,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TraversalNode {
    bundle_id: i64,
    concept_id: String,
    catalog_generation: i64,
    direction: Direction,
    hops: i64,
    path: Vec<ConceptIdentity>,
    relation_type: Nullable<String>,
    title: Nullable<String>,
    freshness: Freshness,
    uri: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConceptRecord {
    bundle_id: i64,
    concept_id: String,
    path: String,
    #[serde(rename = "type")]
    kind: String,
    title: Nullable<String>,
    description: Nullable<String>,
    tags: Vec<String>,
    resource: Nullable<String>,
    body_text: Nullable<String>,
    modified_at: Nullable<String>,
    #[serde(default, skip_serializing_if = "Presence::is_missing")]
    uri: Presence<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillRecord {
    bundle_id: i64,
    concept_id: String,
    name: String,
    description: String,
    package_root: String,
    file_hash: String,
    package_hash: String,
    visibility: String,
    skill_md: String,
    agent_skill: BTreeMap<String, Value>,
    resources: Vec<SkillMember>,
    #[serde(default, skip_serializing_if = "Presence::is_missing")]
    uri: Presence<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MemberClass {
    Script,
    Reference,
    Asset,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillMember {
    byte_size: i64,
    class: MemberClass,
    concept_id: String,
    file_hash: String,
    path: String,
    sha256: String,
    #[serde(default, skip_serializing_if = "Presence::is_missing")]
    media_type: Presence<Option<String>>,
    #[serde(default, skip_serializing_if = "Presence::is_missing")]
    language: Presence<Option<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bundle {
    id: i64,
    name: String,
    source_type: String,
    enabled: bool,
    retired: bool,
    file_count: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncResult {
    bundle_id: i64,
    path: String,
    added: i64,
    removed: i64,
    total: i64,
    unchanged: i64,
    updated: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreatedBundle {
    created: String,
    sync: SyncResult,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RefreshedBundle {
    bundle_id: i64,
    bundle: String,
    sync: SyncResult,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Enabled,
    Disabled,
    Retired,
    Active,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BundleState {
    bundle_id: i64,
    bundle: String,
    state: State,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeletedDocument {
    bundle_id: i64,
    bundle: String,
    path: String,
    deleted: bool,
    sync: SyncResult,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WrittenDocument {
    bundle_id: i64,
    bundle: String,
    path: String,
    replaced: bool,
    written_by: String,
    generated_by: String,
    verifications_set_aside: i64,
    sync: SyncResult,
    note: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PluginStatus {
    Current,
    Stale,
    Retired,
    Unknown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleCheck {
    bundle_id: i64,
    name: Nullable<String>,
    status: PluginStatus,
    reasons: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PluginCheck {
    name: Nullable<String>,
    status: PluginStatus,
    reasons: Vec<String>,
    bundles: Vec<BundleCheck>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteMcp {
    path: String,
    auto_loaded: bool,
    merge_into: Nullable<String>,
    url_key: String,
    #[serde(rename = "type")]
    kind: Nullable<String>,
    token: String,
    token_env: Nullable<String>,
    documented_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpTarget {
    path: String,
    auto_loaded: bool,
    remote: Nullable<RemoteMcp>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PluginTarget {
    id: String,
    label: String,
    kind: String,
    kind_label: String,
    shape: String,
    root: String,
    documented_at: String,
    verified: String,
    notes: String,
    mcp: Nullable<McpTarget>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildConcept {
    bundle_id: i64,
    concept_id: String,
    title: String,
    #[serde(rename = "type")]
    kind: String,
    trust_tier: String,
    exact_source: bool,
    freshness: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileManifest {
    path: String,
    bytes: u64,
    sha256: String,
    executable: bool,
}

#[derive(Debug, Serialize, Deserialize)]
enum Encoding {
    #[serde(rename = "base64")]
    Base64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EncodedBytes {
    encoding: Encoding,
    data: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum FileContent {
    Text(String),
    Binary(EncodedBytes),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExcludedConcept {
    bundle_id: i64,
    concept_id: String,
    reasons: Vec<String>,
    state: String,
    role: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PluginBuild {
    target: String,
    name: String,
    root: String,
    concept_count: u64,
    size_bytes: u64,
    concepts: Vec<BuildConcept>,
    files: Vec<FileManifest>,
    #[serde(default, skip_serializing_if = "Presence::is_missing")]
    contents: Presence<BTreeMap<String, FileContent>>,
    #[serde(default, skip_serializing_if = "Presence::is_missing")]
    written: Presence<Vec<String>>,
    #[serde(default, skip_serializing_if = "Presence::is_missing")]
    note: Presence<String>,
    #[serde(default, skip_serializing_if = "Presence::is_missing")]
    warnings: Presence<Vec<ExcludedConcept>>,
    #[serde(default, skip_serializing_if = "Presence::is_missing")]
    excluded: Presence<Vec<ExcludedConcept>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UnresolvedEdge {
    source_bundle_id: i64,
    source_concept_id: String,
    relation_type: String,
    direction: String,
    target_bundle_id: Nullable<i64>,
    target_concept_id: Nullable<String>,
    external_target: Nullable<String>,
}

/// Refusal discriminator: false is not a refused build.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct Refused(bool);
impl<'de> Deserialize<'de> for Refused {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if bool::deserialize(deserializer)? {
            Ok(Self(true))
        } else {
            Err(serde::de::Error::custom("a build refusal must be true"))
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum BuildRefusal {
    StaleRequired {
        refused: Refused,
        detail: String,
        concepts: Vec<ExcludedConcept>,
    },
    NoFreshConcepts {
        refused: Refused,
        detail: String,
        concepts: Vec<ExcludedConcept>,
    },
    UnresolvedRequired {
        refused: Refused,
        detail: String,
        edges: Vec<UnresolvedEdge>,
    },
    SeedNotFound {
        refused: Refused,
        detail: String,
        seeds: Vec<ConceptIdentity>,
    },
    ClosureNodesMissing {
        refused: Refused,
        detail: String,
        nodes: Vec<ConceptIdentity>,
    },
    ClosureExceedsLimit {
        refused: Refused,
        detail: String,
        limit: u64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum BuildOutcome {
    Built(Box<PluginBuild>),
    Refused(BuildRefusal),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn all_successful_golden_tool_values_round_trip_without_loss() {
        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/mcp-golden");
        let mut tools = std::collections::BTreeSet::new();
        let mut count = 0;
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            let fixture: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let Some(cases) = fixture.as_array() else {
                continue;
            };
            for case in cases {
                if case["request"]["method"] != "tools/call"
                    || case["response"]["result"]["isError"] != false
                {
                    continue;
                }
                let name = case["request"]["params"]["name"].as_str().unwrap();
                let handler = crate::registry::by_name(name).unwrap().handler;
                let raw = case["response"]["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap();
                let value: Value =
                    serde_json::from_str(&raw.replace("<BYTES>", "0").replace("<SIZE_BYTES>", "0"))
                        .unwrap();
                let typed = ToolValue::parse(handler, value.clone())
                    .unwrap_or_else(|error| panic!("{} {name}: {error}", path.display()));
                assert_eq!(typed.into_json(), value, "{} {name}", path.display());
                count += 1;
                tools.insert(name.to_owned());
            }
        }
        assert_eq!(tools.len(), 14);
        assert!(count >= 14);
    }

    #[test]
    fn traversal_and_nested_freshness_are_typed_and_closed() {
        let value = json!([{"bundle_id":1,"concept_id":"x","hops":2,"catalog_generation":3,"direction":"both",
            "path":[{"bundle_id":2,"concept_id":"y"}],"relation_type":null,
            "title":null,"freshness":{"state":"stale","reasons":["changed"]},"uri":"okf://catalog/bundles/1/concepts/eA"}]);
        let typed = ToolValue::parse(Handler::TraverseConcepts, value.clone()).unwrap();
        assert_eq!(typed.into_json(), value);
        let mut hostile = value;
        hostile[0]["freshness"]["unexpected"] = json!(true);
        assert!(ToolValue::parse(Handler::TraverseConcepts, hostile).is_err());
    }

    #[test]
    fn six_real_workspace_refusals_round_trip() {
        use pgokf_workspace::{BuildRefusal, ConceptRef, StaleConcept, UnresolvedEdge};
        let cases = [
            BuildRefusal::StaleRequired {
                concepts: vec![StaleConcept {
                    bundle_id: 1,
                    concept_id: "stale".into(),
                    role: "pick".into(),
                    state: "stale".into(),
                    reasons: vec!["changed".into()],
                }],
            },
            BuildRefusal::NoFreshConcepts { concepts: vec![] },
            BuildRefusal::UnresolvedRequired {
                edges: vec![UnresolvedEdge {
                    source_bundle_id: 1,
                    source_concept_id: "seed".into(),
                    relation_type: "docs:references".into(),
                    direction: "outbound".into(),
                    target_bundle_id: None,
                    target_concept_id: None,
                    external_target: Some("opaque".into()),
                }],
            },
            BuildRefusal::SeedNotFound {
                seeds: vec![ConceptRef {
                    bundle_id: 1,
                    concept_id: "gone".into(),
                }],
            },
            BuildRefusal::ClosureNodesMissing { nodes: vec![] },
            BuildRefusal::ClosureExceedsLimit { limit: 500 },
        ];
        for case in cases {
            let value = case.to_json();
            let typed = ToolValue::parse(Handler::BuildWorkspacePlugin, value.clone()).unwrap();
            assert_eq!(typed.into_json(), value);
        }
    }

    #[test]
    fn nullable_and_omitted_member_fields_are_distinct() {
        let base = json!({"byte_size":1,"class":"reference","concept_id":"x","file_hash":"h",
            "path":"references/x","sha256":"s"});
        let absent: SkillMember = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(serde_json::to_value(absent).unwrap(), base);
        let mut explicit = base;
        explicit["media_type"] = Value::Null;
        let nullable: SkillMember = serde_json::from_value(explicit.clone()).unwrap();
        assert_eq!(serde_json::to_value(nullable).unwrap(), explicit);
    }

    #[test]
    fn malformed_fixed_records_never_become_successes() {
        for bad in [
            json!({"bundle_id":1,"bundle":"b","state":"unknown"}),
            json!({"bundle_id":1,"bundle":"b","state":"enabled","surprise":1}),
            json!({"bundle_id":"1","bundle":"b","state":"enabled"}),
        ] {
            assert_eq!(
                ToolValue::parse(Handler::SetBundleState, bad)
                    .unwrap_err()
                    .code(),
                "internal"
            );
        }
        let missing_nullable = json!([{"bundle_id":1,"concept_id":"x","path":"x","title":null,
            "type":null,"rank":0.5}]);
        assert!(ToolValue::parse(Handler::ConceptSearch, missing_nullable).is_err());
    }
}
