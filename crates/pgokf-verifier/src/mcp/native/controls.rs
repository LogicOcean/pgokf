//! Corrupt actual authenticated response branches, with positive union controls.
use super::{BTreeMap, BTreeSet, Native, Value, json, live, skill_uri};
#[derive(Clone)]
enum Edit {
    Replace(String, Value),
    Remove(String),
}
impl Edit {
    fn apply(&self, value: &mut Value) -> Result<(), String> {
        match self {
            Self::Replace(pointer, replacement) => {
                if let Some(slot) = value.pointer_mut(pointer) {
                    *slot = replacement.clone();
                } else {
                    let (parent, key) = pointer.rsplit_once('/').ok_or("mutation pointer")?;
                    value
                        .pointer_mut(parent)
                        .and_then(Value::as_object_mut)
                        .ok_or("mutation parent")?
                        .insert(key.into(), replacement.clone());
                }
            }
            Self::Remove(pointer) => {
                let (parent, key) = pointer.rsplit_once('/').ok_or("mutation pointer")?;
                value
                    .pointer_mut(parent)
                    .and_then(Value::as_object_mut)
                    .ok_or("mutation parent")?
                    .remove(key)
                    .ok_or("mutation field missing")?;
            }
        }
        Ok(())
    }
}
fn replace(path: &str, value: Value) -> Edit {
    Edit::Replace(path.into(), value)
}
fn remove(path: &str) -> Edit {
    Edit::Remove(path.into())
}
impl Native {
    #[allow(clippy::too_many_lines)] // Keep each ordered protocol scenario together.
    pub(super) fn all_wire_controls(&mut self) -> Result<(), String> {
        let concept = json!({"name":"get_concept","arguments":{"bundle_id":1,"concept_id":"services/widget"}});
        let failure = json!({"name":"get_concept","arguments":{"unknown_argument":true}});
        let mut cases: Vec<(String, &str, Value, Edit)> = Vec::new();
        for (field, value) in [
            ("description", json!(271_828)),
            ("name", json!(27)),
            ("inputSchema", json!([])),
            ("outputSchema", json!([])),
            ("annotations", json!({"readOnlyHint":"yes"})),
            ("_meta", json!([])),
            ("icons", json!([{"src":27}])),
        ] {
            cases.push((
                format!("descriptor-{field}"),
                "tools/list",
                json!({}),
                replace(&format!("/result/tools/0/{field}"), value),
            ));
        }
        for field in ["name", "inputSchema"] {
            cases.push((
                format!("descriptor-missing-{field}"),
                "tools/list",
                json!({}),
                remove(&format!("/result/tools/0/{field}")),
            ));
        }
        cases.push((
            "input-schema-root".into(),
            "tools/list",
            json!({}),
            replace("/result/tools/0/inputSchema", json!({"type":"array"})),
        ));
        cases.push((
            "output-schema-invalid".into(),
            "tools/list",
            json!({}),
            replace("/result/tools/0/outputSchema", json!({"type":271_828})),
        ));
        for field in ["jsonrpc", "id", "result"] {
            cases.push((
                format!("envelope-missing-{field}"),
                "tools/list",
                json!({}),
                remove(&format!("/{field}")),
            ));
        }
        for (field, value, label) in [
            ("id", json!([]), "[]"),
            ("id", json!(true), "True"),
            ("id", json!("wrong"), "wrong"),
            ("jsonrpc", json!("1.0"), "1.0"),
        ] {
            cases.push((
                format!("envelope-{field}-{label}"),
                "tools/list",
                json!({}),
                replace(&format!("/{field}"), value),
            ));
        }
        for (label, path, value) in [
            (
                "metadata-key",
                "/result/_meta",
                json!({"invalid/key/name":1}),
            ),
            ("result-meta", "/result/_meta", json!([])),
            (
                "server-info",
                "/result/_meta/io.modelcontextprotocol~1serverInfo",
                json!({"name":27,"version":[]}),
            ),
            ("result-type", "/result/resultType", json!("input_required")),
        ] {
            cases.push((label.into(), "tools/list", json!({}), replace(path, value)));
        }
        for (field, label) in [("resultType", "type"), ("tools", "tools")] {
            cases.push((
                format!("result-missing-{label}"),
                "tools/list",
                json!({}),
                remove(&format!("/result/{field}")),
            ));
        }
        for (label, path, value) in [
            (
                "tool-error-text",
                "/result/content",
                json!([{"type":"text","text":271_828}]),
            ),
            (
                "tool-error-missing-text",
                "/result/content",
                json!([{"type":"text"}]),
            ),
            ("tool-error-flag", "/result/isError", json!("true")),
            ("tool-error-content", "/result/content", json!({})),
        ] {
            cases.push((
                label.into(),
                "tools/call",
                failure.clone(),
                replace(path, value),
            ));
        }
        cases.push((
            "tool-missing-content".into(),
            "tools/call",
            concept.clone(),
            remove("/result/content"),
        ));
        for (name, block) in [
            ("text", json!({"type":"text","text":"valid"})),
            (
                "image",
                json!({"type":"image","data":"AA==","mimeType":"image/png"}),
            ),
            (
                "audio",
                json!({"type":"audio","data":"AA==","mimeType":"audio/wav"}),
            ),
            (
                "resource_link",
                json!({"type":"resource_link","uri":"okf://catalog/manifest","name":"manifest"}),
            ),
            (
                "resource-text",
                json!({"type":"resource","resource":{"uri":"okf://catalog/manifest","text":"valid"}}),
            ),
            (
                "resource-blob",
                json!({"type":"resource","resource":{"uri":"okf://catalog/manifest","blob":"AA=="}}),
            ),
        ] {
            let message = live::modern("tools/call", concept.clone());
            let mut response = self
                .exchange(&message, "reader", false, &BTreeMap::new())?
                .value;
            response["result"]["content"] = json!([block]);
            self.receipts.assert(
                format!("control:wire:valid-{name}"),
                self.wire.exchange(&message, &response).is_ok(),
            )?;
            for field in block.as_object().ok_or("block")?.keys() {
                let mut bad = block.clone();
                bad[field] = json!(271_828);
                cases.push((
                    format!("content-{name}-{field}"),
                    "tools/call",
                    concept.clone(),
                    replace("/result/content", json!([bad])),
                ));
            }
            let mut bad = block.clone();
            bad["_meta"] = json!([]);
            cases.push((
                format!("content-{name}-meta"),
                "tools/call",
                concept.clone(),
                replace("/result/content", json!([bad])),
            ));
            if let Some(resource) = block.get("resource") {
                for field in resource.as_object().ok_or("resource")?.keys() {
                    let mut bad = block.clone();
                    bad["resource"][field] = json!(271_828);
                    cases.push((
                        format!("embedded-{name}-{field}"),
                        "tools/call",
                        concept.clone(),
                        replace("/result/content", json!([bad])),
                    ));
                }
            }
        }
        let missing = json!({"uri":"okf://catalog/bundles/999999"});
        for (field, value) in [("code", json!("bad")), ("message", json!(271_828))] {
            cases.push((
                format!("error-{field}"),
                "resources/read",
                missing.clone(),
                replace(&format!("/error/{field}"), value),
            ));
            cases.push((
                format!("error-missing-{field}"),
                "resources/read",
                missing.clone(),
                remove(&format!("/error/{field}")),
            ));
        }
        for (label, path, value) in [
            ("error-id", "/id", json!("wrong")),
            ("error-id-type", "/id", json!([])),
            ("error-extra-result", "/result", json!({})),
        ] {
            cases.push((
                label.into(),
                "resources/read",
                missing.clone(),
                replace(path, value),
            ));
        }
        for (field, value) in [
            ("text", json!(27)),
            ("uri", json!(27)),
            ("mimeType", json!(27)),
            ("_meta", json!([])),
        ] {
            cases.push((
                format!("resource-{field}"),
                "resources/read",
                json!({"uri":"okf://catalog/manifest"}),
                replace(&format!("/result/contents/0/{field}"), value),
            ));
        }
        for field in ["uri", "text"] {
            cases.push((
                format!("resource-missing-{field}"),
                "resources/read",
                json!({"uri":"okf://catalog/manifest"}),
                remove(&format!("/result/contents/0/{field}")),
            ));
        }
        for (field, value) in [
            ("uri", json!(27)),
            ("name", json!(27)),
            ("_meta", json!([])),
            ("description", json!(27)),
        ] {
            cases.push((
                format!("resource-descriptor-{field}"),
                "resources/list",
                json!({}),
                replace(&format!("/result/resources/0/{field}"), value),
            ));
        }
        for (field, value) in [
            ("uriTemplate", json!(27)),
            ("name", json!(27)),
            ("_meta", json!([])),
        ] {
            cases.push((
                format!("template-{field}"),
                "resources/templates/list",
                json!({}),
                replace(&format!("/result/resourceTemplates/0/{field}"), value),
            ));
        }
        for (field, value) in [
            ("uri", json!(27)),
            ("frontmatter", json!([])),
            ("resources", json!(27)),
            ("_meta", json!([])),
        ] {
            cases.push((
                format!("skill-{field}"),
                "skills/list",
                json!({}),
                replace(&format!("/result/skills/0/{field}"), value),
            ));
        }
        for (label, path) in [
            (
                "skill-frontmatter-name",
                "/result/skills/0/frontmatter/name",
            ),
            ("skill-member-digest", "/result/skills/0/resources/0/digest"),
        ] {
            cases.push((
                label.into(),
                "skills/list",
                json!({}),
                replace(path, json!(27)),
            ));
        }
        self.check(
            cases
                .iter()
                .map(|(l, _, _, _)| l)
                .collect::<BTreeSet<_>>()
                .len()
                == cases.len(),
            "unique wire controls",
        )?;
        for (label, method, params, edit) in cases {
            let message = live::modern(method, params);
            let mut response = self
                .exchange(&message, "reader", false, &BTreeMap::new())?
                .value;
            self.wire.exchange(&message, &response)?;
            edit.apply(&mut response)?;
            self.receipts.assert(
                format!("control:wire:{label}"),
                self.wire.exchange(&message, &response).is_err(),
            )?;
            self.assertions += 1;
        }
        Ok(())
    }
    pub(super) fn cache_controls(&mut self) -> Result<(), String> {
        for (method, params) in [
            ("server/discover", json!({})),
            ("tools/list", json!({})),
            ("resources/list", json!({})),
            ("resources/templates/list", json!({})),
            ("resources/read", json!({"uri":"okf://catalog/manifest"})),
            ("skills/list", json!({})),
            ("skills/get", json!({"uri":skill_uri()})),
        ] {
            let result = self.success(method, params)?;
            let ttl = result["ttlMs"].as_u64().ok_or("ttl")?;
            for (name, field, value) in [
                ("scope", "cacheScope", json!("invalid")),
                (
                    "public-private",
                    "cacheScope",
                    json!(if result["cacheScope"] == "public" {
                        "private"
                    } else {
                        "public"
                    }),
                ),
                ("negative", "ttlMs", json!(-1)),
                ("zero", "ttlMs", json!(0)),
                ("type", "ttlMs", json!("30000")),
                ("boolean", "ttlMs", json!(true)),
                ("ttl", "ttlMs", json!(ttl + 1)),
            ] {
                let mut bad = result.clone();
                bad[field] = value;
                let saved = self.receipts.clone();
                let rejected = self.cache_receipts(method, &bad, ttl).is_err();
                self.receipts = saved;
                self.receipts
                    .assert(format!("control:cache:{method}:{name}"), rejected)?;
                self.assertions += 1;
            }
        }
        Ok(())
    }
}
