//! Cross-tenant visibility, exact escaped resource identities and stale build checks.
use super::{
    BTreeMap, DB, Endpoint, Native, PREFIX, bytes, json, key, live, skill_uri, sql_literal,
};
use base64::{Engine, engine::general_purpose::STANDARD};

impl Native {
    #[allow(clippy::too_many_lines)] // Keep each ordered protocol scenario together.
    pub(super) fn tenant_surface(&mut self) -> Result<(), String> {
        let package = std::str::from_utf8(&self.package)
            .map_err(|e| e.to_string())?
            .to_owned();
        let output=self.cluster.sql(DB,&format!("SET pgokf.tenant='contract-b'; SELECT pgokf.register_bundle_content('foreign-skill',ARRAY['skills/deploy/SKILL.md','skills/deploy/assets/payload.bin'],ARRAY[convert_to({},'UTF8'),decode('00ff80','hex')]); SELECT id FROM pgokf.bundles WHERE name='foreign-skill';",sql_literal(&package)))?;
        let foreign_skill_id: i64 = output
            .lines()
            .last()
            .ok_or("foreign skill")?
            .parse()
            .map_err(|_| "foreign skill id")?;
        let uri = skill_uri().replace("/b/1/", &format!("/b/{foreign_skill_id}/"));
        let dsn = self.cluster.dsn(DB, "mcp_contract");
        let mut endpoint = Endpoint::start(
            &self.binary,
            &dsn,
            true,
            Some("contract-b"),
            BTreeMap::new(),
            &self.evidence.join("foreign.json"),
        )?;
        let message = live::modern("skills/get", json!({"uri":uri}));
        let response = endpoint.exchange(&message, "foreign", true)?;
        self.wire.exchange(&message, &response.value)?;
        self.check(
            response.status == 200 && response.value["result"]["skill"]["uri"] == uri,
            "foreign skill positive owner",
        )?;
        let foreign_uris = [
            format!("okf://catalog/bundles/{}", self.foreign_id),
            format!(
                "okf://catalog/bundles/{}/concepts/{}",
                self.foreign_id,
                key("foreign")
            ),
            format!(
                "okf://catalog/bundles/{}/documents/{}",
                self.foreign_id,
                key("foreign.md")
            ),
            uri.clone(),
            uri.replace("SKILL.md", "assets/payload.bin"),
        ];
        for foreign in foreign_uris {
            let message = live::modern("resources/read", json!({"uri":foreign}));
            let response = endpoint.exchange(&message, "foreign", true)?;
            self.wire.exchange(&message, &response.value)?;
            self.check(
                response.status == 200 && response.value["result"]["contents"][0]["uri"] == foreign,
                "foreign resource positive owner",
            )?;
            let missing = if foreign.starts_with("skill://") {
                foreign.replace(&format!("/b/{foreign_skill_id}/"), "/b/999999/")
            } else {
                foreign.replacen(&format!("/{}", self.foreign_id), "/999999", 1)
            };
            for role in live::ROLES {
                let foreign_result = self.error_at(
                    "resources/read",
                    json!({"uri":foreign}),
                    role,
                    false,
                    -32602,
                    400,
                )?;
                let absent = self.error_at(
                    "resources/read",
                    json!({"uri":missing}),
                    role,
                    false,
                    -32602,
                    400,
                )?;
                self.receipts
                    .assert(format!("tenant:{role}"), foreign_result == absent)?;
                self.assertions += 1;
            }
        }
        endpoint.stop()?;
        let missing_skill = uri.replace(&format!("/b/{foreign_skill_id}/"), "/b/999999/");
        let foreign = self.error("skills/get", json!({"uri":uri}))?;
        let missing = self.error("skills/get", json!({"uri":missing_skill}))?;
        self.check(foreign == missing, "skills tenant non-disclosure")?;
        for (name, concept, id) in [
            ("get_concept", "foreign", self.foreign_id),
            ("get_skill", "skills/deploy/SKILL", foreign_skill_id),
        ] {
            for role in live::ROLES {
                let foreign = self.success_at(
                    "tools/call",
                    json!({"name":name,"arguments":{"bundle_id":id,"concept_id":concept}}),
                    role,
                    false,
                )?;
                let missing = self.success_at(
                    "tools/call",
                    json!({"name":name,"arguments":{"bundle_id":999_999,"concept_id":concept}}),
                    role,
                    false,
                )?;
                self.check(
                    foreign == missing && foreign["isError"] == (name == "get_skill"),
                    "tool tenant non-disclosure",
                )?;
            }
        }
        let doc = "---\ntype: Runbook\ntitle: Golden HTTP write\n---\n\n# Golden HTTP write\n\nWritten over HTTP by the golden MCP harness, then deleted again.\n";
        let path = "docs/café % note.md";
        let output=self.cluster.sql(DB,&format!("SET pgokf.tenant='contract-a'; SELECT pgokf.register_bundle_content('unicode-surface',ARRAY[{},'skills/deploy/SKILL.md','skills/deploy/assets/café % note.bin'],ARRAY[convert_to({},'UTF8'),convert_to({},'UTF8'),decode('00ff80','hex')]); SELECT id FROM pgokf.bundles WHERE name='unicode-surface';",sql_literal(path),sql_literal(doc),sql_literal(&package)))?;
        let id: i64 = output
            .lines()
            .last()
            .ok_or("unicode bundle")?
            .parse()
            .map_err(|_| "unicode id")?;
        let uri = format!("okf://catalog/bundles/{id}/documents/{}", key(path));
        let content = self.success("resources/read", json!({"uri":uri}))?["contents"][0].clone();
        self.check(bytes(&content)? == doc.as_bytes(), "unicode document bytes")?;
        let uri = format!(
            "skill://pgokf/b/{id}/c/{}/deploy/assets/caf%C3%A9%20%25%20note.bin",
            key("skills/deploy/SKILL")
        );
        let content = self.success("resources/read", json!({"uri":uri}))?["contents"][0].clone();
        self.check(
            content["uri"] == uri && bytes(&content)? == [0, 255, 128],
            "unicode canonical asset",
        )?;
        for bad in [uri.replace("%C3", "%c3"), uri.replace("%25", "%2525")] {
            self.error("resources/read", json!({"uri":bad}))?;
        }
        self.cluster.sql(
            DB,
            &format!("SET pgokf.tenant='contract-a'; SELECT pgokf.set_bundle_enabled({id},false);"),
        )?;
        let name = "unknown-é";
        let r = self.exchange(
            &live::modern("tools/call", json!({"name":name,"arguments":{}})),
            "reader",
            false,
            &BTreeMap::from([(
                "Mcp-Name".into(),
                Some(format!("=?base64?{}?=", STANDARD.encode(name))),
            )]),
        )?;
        self.check(
            r.status == 400 && r.value["error"]["code"] == -32602,
            "encoded non-ASCII method name",
        )?;
        for value in [None, Some("wrong".into())] {
            let r = self.exchange(
                &live::modern("resources/read", json!({"uri":"okf://catalog/manifest"})),
                "reader",
                false,
                &BTreeMap::from([("Mcp-Name".into(), value)]),
            )?;
            self.check(
                r.status == 400 && r.value["error"]["code"] == -32020,
                "resource header URI binding",
            )?;
        }
        Ok(())
    }
    pub(super) fn freshness_build(&mut self) -> Result<(), String> {
        self.cluster.sql(DB,"SET pgokf.tenant='contract-a'; SELECT pgokf.mark_stale(1,ARRAY['source_advanced'],'surface','surface-r2');")?;
        for role in live::ROLES {
            for (method, field) in [("resources/list", "resources"), ("skills/list", "skills")] {
                let mut result = self.success_at(method, json!({}), role, false)?;
                let mut rows = result[field].as_array().ok_or("rows")?.clone();
                let mut pages = 0;
                while let Some(cursor) = result["nextCursor"].as_str() {
                    pages += 1;
                    self.check(pages < 100, "bounded stale pages")?;
                    result = self.success_at(method, json!({"cursor":cursor}), role, false)?;
                    rows.extend(result[field].as_array().ok_or("rows")?.clone());
                }
                self.check(
                    !rows.is_empty()
                        && rows.iter().all(|r| {
                            !r["uri"]
                                .as_str()
                                .unwrap_or_default()
                                .contains(&format!("/b/{}/", self.foreign_id))
                                && r["uri"] != format!("okf://catalog/bundles/{}", self.foreign_id)
                        }),
                    "stale tenant-visible listing",
                )?;
                let row = rows
                    .iter()
                    .find(|r| {
                        r["uri"]
                            .as_str()
                            .is_some_and(|s| s.contains("/b/1/") || s == "okf://catalog/bundles/1")
                    })
                    .ok_or("stale bundle descriptor")?;
                self.check(
                    row["_meta"][format!("{PREFIX}freshness")]["state"] == "stale",
                    "listing does not hide stale state",
                )?;
            }
        }
        let bundle = self.success("resources/read", json!({"uri":"okf://catalog/bundles/1"}))?;
        self.check(
            bundle["contents"][0]["_meta"][format!("{PREFIX}freshness")]["state"] == "stale",
            "stale bundle metadata",
        )?;
        let listed = self.success_at(
            "tools/call",
            json!({"name":"list_bundles","arguments":{}}),
            "writer",
            false,
        )?;
        self.check(listed["isError"] == false, "stale bundle list tool")?;
        let built=self.success_at("tools/call",json!({"name":"build_workspace_plugin","arguments":{"target":"claude-code","bundle_ids":[1],"limit":10}}),"builder",false)?;
        self.check(built["isError"] == false, "stale build")?;
        let lock = built["structuredContent"]["contents"]
            .as_object()
            .ok_or("contents")?
            .iter()
            .find(|(k, _)| k.ends_with("okf-workspace.lock"))
            .and_then(|(_, v)| v.as_str())
            .ok_or("lock")?;
        self.check(
            lock.contains("catalog_generation") && lock.contains("stale"),
            "build preserves stale evidence",
        )?;
        let checked = self.tool("check_workspace_plugin_freshness", json!({"lock":lock}))?;
        self.check(
            checked["isError"] == false && !checked["structuredContent"].is_null(),
            "freshness checker",
        )?;
        let restored=self.cluster.sql(DB,"SET pgokf.tenant='contract-a'; SELECT pgokf.mark_fresh(1,b.catalog_generation,f.dependency_invalidation_epoch,'surface-r2') FROM pgokf.bundles b JOIN pgokf.bundle_freshness f ON f.bundle_id=b.id WHERE b.id=1;")?;
        self.check(restored.lines().last() == Some("t"), "freshness restore")
    }
}
