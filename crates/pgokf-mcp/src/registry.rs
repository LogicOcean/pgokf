// SPDX-License-Identifier: AGPL-3.0-only
//! The single inventory for tool discovery, dispatch and authorization.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn registry_is_complete_and_unique() {
        let tools = all();
        assert_eq!(tools.len(), 15);
        let names: std::collections::HashSet<_> = tools.iter().map(|t| t.name).collect();
        let handlers: std::collections::HashSet<_> = tools.iter().map(|t| t.handler).collect();
        assert_eq!(names.len(), 15);
        assert_eq!(handlers.len(), 15);
        for tool in tools {
            assert_eq!(tool.input_schema["additionalProperties"], false);
            assert!(tool.validate_input(&json!({"unknown":true})).is_err());
            assert!(
                tool.validate_output(&json!({"code":"internal","message":"Unavailable"}))
                    .is_ok()
            );
            assert!(tool.validate_output(&json!({"wrong":true})).is_err());
        }
    }

    #[test]
    fn all_successful_legacy_golden_payloads_validate() {
        let fixtures =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/mcp-golden");
        let mut checked = std::collections::HashSet::new();
        for entry in std::fs::read_dir(fixtures).expect("golden directory") {
            let path = entry.expect("fixture").path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let fixtures: Value =
                serde_json::from_slice(&std::fs::read(&path).expect("read fixture"))
                    .expect("fixture JSON");
            let Some(cases) = fixtures.as_array() else {
                continue;
            };
            for case in cases {
                if case["request"]["method"] != "tools/call"
                    || case["response"]["result"]["isError"] != false
                {
                    continue;
                }
                let name = case["request"]["params"]["name"]
                    .as_str()
                    .expect("tool name");
                let tool = by_name(name).expect("registered legacy tool");
                let text = case["response"]["result"]["content"][0]["text"]
                    .as_str()
                    .expect("legacy text");
                let value: Value = serde_json::from_str(
                    &text.replace("<BYTES>", "0").replace("<SIZE_BYTES>", "0"),
                )
                .expect("legacy JSON data");
                assert!(
                    tool.validate_output(&value).is_ok(),
                    "{}: {}: {:?}",
                    path.display(),
                    name,
                    tool.validate_output(&value)
                );
                checked.insert(name.to_owned());
            }
        }
        assert_eq!(
            checked.len(),
            14,
            "every legacy tool has a real database fixture"
        );
    }

    #[test]
    fn build_refusals_use_the_real_workspace_domain_shapes() {
        use pgokf_workspace::{BuildRefusal, ConceptRef, StaleConcept, UnresolvedEdge};
        let tool = by_name("build_workspace_plugin").unwrap();
        let cases = [
            BuildRefusal::StaleRequired {
                concepts: vec![StaleConcept {
                    bundle_id: 1,
                    concept_id: "stale".to_owned(),
                    role: "pick".to_owned(),
                    state: "stale".to_owned(),
                    reasons: vec!["producer".to_owned()],
                }],
            },
            BuildRefusal::NoFreshConcepts {
                concepts: Vec::new(),
            },
            BuildRefusal::UnresolvedRequired {
                edges: vec![UnresolvedEdge {
                    source_bundle_id: 1,
                    source_concept_id: "seed".to_owned(),
                    relation_type: "docs:references".to_owned(),
                    direction: "outbound".to_owned(),
                    target_bundle_id: None,
                    target_concept_id: None,
                    external_target: Some("opaque".to_owned()),
                }],
            },
            BuildRefusal::SeedNotFound {
                seeds: vec![ConceptRef {
                    bundle_id: 1,
                    concept_id: "missing".to_owned(),
                }],
            },
            BuildRefusal::ClosureNodesMissing { nodes: Vec::new() },
            BuildRefusal::ClosureExceedsLimit { limit: 500 },
        ];
        for case in cases {
            let value = case.to_json();
            assert!(
                tool.validate_output(&value).is_ok(),
                "{:?}",
                tool.validate_output(&value)
            );
            let mut invalid = value;
            invalid["refused"] = json!(false);
            assert!(tool.validate_output(&invalid).is_err());
        }
    }

    #[test]
    fn validation_rejects_deep_values_without_visiting_schema() {
        let tool = by_name("get_skill").unwrap();
        let mut value = json!(null);
        for _ in 0..70 {
            value = json!([value]);
        }
        assert_eq!(
            tool.validate_output(&value).unwrap_err(),
            "JSON value exceeds validation bounds"
        );
    }

    #[test]
    fn structural_selectors_ranges_and_types_fail_closed() {
        let put = by_name("put_document").unwrap();
        for args in [
            json!({"path":"a.md","text":"ok"}),
            json!({"bundle":"a","bundle_id":1,"path":"a.md","text":"ok"}),
        ] {
            assert!(put.validate_input(&args).is_err());
        }
        assert!(
            put.validate_input(&json!({"bundle_id":1,"path":"a.md","text":"ok"}))
                .is_ok()
        );
        let search = by_name("concept_search").unwrap();
        for args in [
            json!({"query":"x","limit":501}),
            json!({"query":"x","limit":0}),
            json!({"query":"x","tags":[false]}),
            json!({"query":"x","status":false}),
        ] {
            assert!(search.validate_input(&args).is_err());
        }
    }

    #[test]
    fn host_arguments_come_from_schema_and_are_not_advertised_remotely() {
        assert_eq!(host_only_arguments(), ["output_dir", "overwrite"]);
        let tool = by_name("build_workspace_plugin").unwrap();
        assert!(
            tool.definition(false)["inputSchema"]["properties"]
                .get("output_dir")
                .is_some()
        );
        assert!(
            tool.definition(true)["inputSchema"]["properties"]
                .get("output_dir")
                .is_none()
        );
    }
}

use crate::{schemas, tokens::Role};
use serde_json::{Value, json};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Availability {
    Reader,
    Build,
    Writer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum Handler {
    ConceptSearch,
    FindSimilar,
    ConceptNeighbors,
    GetConcept,
    GetSkill,
    ListPluginTargets,
    BuildWorkspacePlugin,
    CheckWorkspacePluginFreshness,
    ListBundles,
    PutDocument,
    DeleteDocument,
    CreateContentBundle,
    RefreshBundle,
    SetBundleState,
    TraverseConcepts,
}

pub(crate) struct ToolSpec {
    pub(crate) name: &'static str,
    pub(crate) title: &'static str,
    pub(crate) description: String,
    pub(crate) minimum_role: Role,
    pub(crate) availability: Availability,
    pub(crate) handler: Handler,
    pub(crate) input_schema: Value,
    pub(crate) output_schema: Value,
    input_validator: jsonschema::Validator,
    output_validator: jsonschema::Validator,
}

impl ToolSpec {
    pub(crate) fn definition(&self, remote: bool) -> Value {
        let mut input = self.input_schema.clone();
        if remote && let Some(properties) = input["properties"].as_object_mut() {
            properties.retain(|_, schema| schema[schemas::HOST_ONLY] != true);
        }
        let read_only = !matches!(
            self.availability,
            Availability::Writer | Availability::Build
        );
        let destructive = matches!(
            self.handler,
            Handler::PutDocument
                | Handler::DeleteDocument
                | Handler::SetBundleState
                | Handler::RefreshBundle
                | Handler::BuildWorkspacePlugin
        );
        json!({"name": self.name, "title": self.title, "description": self.description,
            "inputSchema": input, "outputSchema": self.output_schema,
            "annotations": {"readOnlyHint":read_only, "destructiveHint":destructive,
                "idempotentHint":read_only, "openWorldHint":false}})
    }

    pub(crate) fn validate_input(&self, value: &Value) -> Result<(), String> {
        // Both transports cap wire bytes; local programmatic callers get the
        // same bounded-depth and bounded-node contract before validation.
        bounded(value, 0, &mut 200_000)?;
        self.input_validator.validate(value).map_err(|error| {
            format!(
                "invalid arguments at {}: {}",
                error.instance_path(),
                error.schema_path()
            )
        })
    }

    pub(crate) fn validate_output(&self, value: &Value) -> Result<(), String> {
        bounded(value, 0, &mut 1_000_000)?;
        self.output_validator.validate(value).map_err(|error| {
            format!(
                "invalid output at {}: {}",
                error.instance_path(),
                error.schema_path()
            )
        })
    }
}

fn bounded(value: &Value, depth: usize, remaining: &mut usize) -> Result<(), String> {
    if depth > 64 || *remaining == 0 {
        return Err("JSON value exceeds validation bounds".to_owned());
    }
    *remaining -= 1;
    match value {
        Value::Array(values) => {
            for value in values {
                bounded(value, depth + 1, remaining)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                bounded(value, depth + 1, remaining)?;
            }
        }
        _ => (),
    }
    Ok(())
}

use Availability::{Build, Reader, Writer};
const INVENTORY: [(&str, &str, Role, Availability, Handler); 15] = [
    (
        "concept_search",
        "Search concepts",
        Role::Reader,
        Reader,
        Handler::ConceptSearch,
    ),
    (
        "find_similar",
        "Find similar concepts",
        Role::Reader,
        Reader,
        Handler::FindSimilar,
    ),
    (
        "concept_neighbors",
        "Follow concept links",
        Role::Reader,
        Reader,
        Handler::ConceptNeighbors,
    ),
    (
        "get_concept",
        "Read concept metadata",
        Role::Reader,
        Reader,
        Handler::GetConcept,
    ),
    (
        "get_skill",
        "Read stored skill",
        Role::Reader,
        Reader,
        Handler::GetSkill,
    ),
    (
        "list_plugin_targets",
        "List plugin targets",
        Role::Builder,
        Reader,
        Handler::ListPluginTargets,
    ),
    (
        "build_workspace_plugin",
        "Build workspace plugin",
        Role::Builder,
        Build,
        Handler::BuildWorkspacePlugin,
    ),
    (
        "check_workspace_plugin_freshness",
        "Check plugin freshness",
        Role::Reader,
        Reader,
        Handler::CheckWorkspacePluginFreshness,
    ),
    (
        "list_bundles",
        "List bundles",
        Role::Writer,
        Reader,
        Handler::ListBundles,
    ),
    (
        "put_document",
        "Write document",
        Role::Writer,
        Writer,
        Handler::PutDocument,
    ),
    (
        "delete_document",
        "Delete document",
        Role::Writer,
        Writer,
        Handler::DeleteDocument,
    ),
    (
        "create_content_bundle",
        "Create content bundle",
        Role::Admin,
        Writer,
        Handler::CreateContentBundle,
    ),
    (
        "refresh_bundle",
        "Refresh bundle",
        Role::Admin,
        Writer,
        Handler::RefreshBundle,
    ),
    (
        "set_bundle_state",
        "Set bundle state",
        Role::Admin,
        Writer,
        Handler::SetBundleState,
    ),
    (
        "traverse_concepts",
        "Traverse concept graph",
        Role::Reader,
        Reader,
        Handler::TraverseConcepts,
    ),
];

pub(crate) fn all() -> &'static [ToolSpec] {
    static TOOLS: OnceLock<Vec<ToolSpec>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        let mut definitions = schemas::definitions();
        INVENTORY
            .into_iter()
            .map(|(name, title, minimum_role, availability, handler)| {
                let definition = definitions.remove(name).expect("every tool has one schema");
                let input_schema = definition["inputSchema"].clone();
                let output_schema = schemas::output(handler);
                let input_validator = compile(&input_schema);
                let output_validator = compile(&output_schema);
                ToolSpec {
                    name,
                    title,
                    minimum_role,
                    availability,
                    handler,
                    input_schema,
                    output_schema,
                    input_validator,
                    output_validator,
                    description: format!(
                        "{} Unknown arguments and wrong-typed optionals are refused.",
                        definition["description"].as_str().expect("description")
                    ),
                }
            })
            .collect()
    })
}

fn compile(schema: &Value) -> jsonschema::Validator {
    // Only checked-in, reference-free schemas enter this evaluator. The
    // dependency disables both HTTP and file resolvers at compile time.
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(schema)
        .expect("valid embedded JSON Schema")
}

pub(crate) fn by_name(name: &str) -> Option<&'static ToolSpec> {
    all().iter().find(|spec| spec.name == name)
}

pub(crate) fn host_only_arguments() -> &'static [String] {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names = std::collections::BTreeSet::new();
        for spec in all() {
            if let Some(properties) = spec.input_schema["properties"].as_object() {
                for (name, schema) in properties {
                    if schema[schemas::HOST_ONLY] == true {
                        names.insert(name.clone());
                    }
                }
            }
        }
        names.into_iter().collect()
    })
}

pub(crate) fn echo(text: &str) -> String {
    let mut shown: String = text.chars().take(64).collect();
    if text.chars().nth(64).is_some() {
        shown.push('…');
    }
    format!("{shown:?}")
}
