//! Independently named normative predicates; never derive execution from ledger floors.
use super::semantic_rules::{Rule, check as rule};
use super::{BTreeMap, BTreeSet, Native, Value, bytes, json, key, live, skill_uri};
impl Native {
    fn semantic(
        &mut self,
        scenario: &str,
        check: &str,
        operation: &str,
        predicate: bool,
    ) -> Result<(), String> {
        self.receipts.assert(
            format!("semantic:{scenario}:{check}:{operation}"),
            predicate,
        )?;
        self.assertions += 1;
        Ok(())
    }
    pub(super) fn cache_receipts(
        &mut self,
        method: &str,
        value: &Value,
        ttl: u64,
    ) -> Result<(), String> {
        if self.mutation != Some(super::Mutation::CacheTtl)
            && method == "server/discover"
            && !rule(Rule::CacheTtl, value, &json!(ttl))
        {
            return Err("cache TTL: server/discover".into());
        }
        let scope = if method == "server/discover" {
            "public"
        } else {
            "private"
        };
        if self.mutation != Some(super::Mutation::CacheScope) {
            self.semantic(
                "caching",
                "sep-2549-cache-scope-valid",
                method,
                rule(Rule::CacheScope, value, &json!(scope)),
            )?;
        }
        if self.mutation != Some(super::Mutation::CachePositive) {
            self.semantic(
                "caching",
                "sep-2549-ttl-non-negative",
                method,
                rule(Rule::CachePositive, value, &Value::Null),
            )?;
        }
        let pair = match method {
            "tools/list" => Some(("caching", "sep-2549-tools-list-caching-hints")),
            "resources/list" => Some(("caching", "sep-2549-resources-list-caching-hints")),
            "resources/templates/list" => {
                Some(("caching", "sep-2549-resources-templates-list-caching-hints"))
            }
            "resources/read" => Some(("caching", "sep-2549-resources-read-caching-hints")),
            "skills/list" => Some((
                "sep-2640-skills-enumeration",
                "sep-2640-skills-list-cache-attributes",
            )),
            "skills/get" => Some((
                "sep-2640-skills-enumeration",
                "sep-2640-skills-get-cache-attributes",
            )),
            _ => None,
        };
        if let Some((scenario, check)) = pair {
            for (family, predicate) in [
                ("scope", rule(Rule::CacheScope, value, &json!(scope))),
                ("positive", rule(Rule::CachePositive, value, &Value::Null)),
                ("ttl", rule(Rule::CacheTtl, value, &json!(ttl))),
            ] {
                let skipped = matches!(
                    (self.mutation, family),
                    (Some(super::Mutation::CacheScope), "scope")
                        | (Some(super::Mutation::CachePositive), "positive")
                        | (Some(super::Mutation::CacheTtl), "ttl")
                );
                if !skipped {
                    self.semantic(scenario, check, &format!("{method}:{family}"), predicate)?;
                }
            }
        }
        Ok(())
    }
    pub(super) fn requirement_tools(&mut self) -> Result<(), String> {
        let result = self.success_at("tools/list", json!({}), "admin", false)?;
        let tools = result["tools"].as_array().ok_or("tools")?;
        self.semantic(
            "tools-list",
            "tools-list",
            "tools/list",
            rule(Rule::ToolList, &result, &Value::Null),
        )?;
        for tool in tools {
            let name = tool["name"].as_str().ok_or("tool name")?;
            self.semantic(
                "tools-list",
                "tools-name-format",
                name,
                rule(Rule::ToolName, &json!(name), &Value::Null),
            )?;
            self.semantic(
                "json-schema-2020-12",
                "json-schema-2020-12-tool-found",
                name,
                rule(Rule::SchemaDraft, tool, &Value::Null),
            )?;
        }
        let result = self.tool(
            "get_concept",
            json!({"bundle_id":1,"concept_id":"services/widget"}),
        )?;
        self.semantic(
            "tools-call-simple-text",
            "tools-call-simple-text",
            "get_concept",
            rule(Rule::ToolText, &result, &Value::Null),
        )?;
        let result = self.tool(
            "get_concept",
            json!({"bundle_id":1,"concept_id":"services/widget","unadvertised_argument":true}),
        )?;
        self.semantic(
            "tools-call-error",
            "tools-call-error",
            "get_concept",
            rule(Rule::ToolError, &result, &Value::Null),
        )
    }
    pub(super) fn requirement_resources(&mut self) -> Result<(), String> {
        let (resources, _) = self.pages("resources/list", "resources")?;
        self.semantic(
            "resources-list",
            "resources-list",
            "resources/list",
            rule(Rule::ResourceList, &json!(resources), &Value::Null),
        )?;
        let uri = format!(
            "okf://catalog/bundles/1/documents/{}",
            key("services/widget.md")
        );
        let text = self.success("resources/read", json!({"uri":uri}))?;
        self.semantic(
            "resources-read-text",
            "resources-read-text",
            "document",rule(Rule::ResourceText, &text, &json!({"uri":uri,"text":std::fs::read_to_string(self.root.join("tests/mcp-golden/bundle/services/widget.md")).map_err(|e|e.to_string())?})),
        )?;
        let (skills, _) = self.pages("skills/list", "skills")?;
        let binary_uri = skills
            .iter()
            .filter_map(|s| s["resources"].as_array())
            .flatten()
            .find(|r| {
                r["uri"]
                    .as_str()
                    .is_some_and(|s| s.ends_with("/assets/payload.bin"))
            })
            .ok_or("binary member")?["uri"]
            .clone();
        let binary = self.success("resources/read", json!({"uri":binary_uri}))?;
        self.semantic(
            "resources-read-binary",
            "resources-read-binary",
            "member",
            rule(Rule::ResourceBinary, &binary, &Value::Null),
        )?;
        let templates = self.success("resources/templates/list", json!({}))?;
        for template in templates["resourceTemplates"]
            .as_array()
            .ok_or("templates")?
        {
            let pattern = template["uriTemplate"].as_str().ok_or("template")?;
            let expansion = pattern
                .replace("{bundle_id}", "1")
                .replace("{concept_key}", &key("skills/deploy/SKILL"))
                .replace("{path_key}", &key("services/widget.md"))
                .replace("{skill_name}", "deploy")
                .replace("{+member_path}", "scripts/check.sh");
            let contents = self.success("resources/read", json!({"uri":expansion}))?;
            self.semantic(
                "resources-templates-read",
                "resources-templates-read",
                pattern,
                rule(Rule::ResourceTemplate, &contents, &json!(expansion)),
            )?;
            self.receipts.assert(
                format!("template:{pattern}"),
                contents["contents"][0]["uri"] == expansion,
            )?;
        }
        let response = self.exchange(
            &live::modern(
                "resources/read",
                json!({"uri":"okf://catalog/bundles/999999"}),
            ),
            "reader",
            false,
            &BTreeMap::new(),
        )?;
        self.semantic(
            "sep-2164-resource-not-found",
            "sep-2164-no-empty-contents",
            "resources/read",
            rule(
                Rule::ResourceError,
                &json!({"status":response.status,"response":response.value}),
                &Value::Null,
            ),
        )?;
        self.semantic(
            "sep-2164-resource-not-found",
            "sep-2164-error-code",
            "resources/read",
            rule(Rule::ErrorCode, &response.value, &json!(-32602)),
        )
    }
    fn skill_check(
        &mut self,
        suffix: &str,
        operation: &str,
        predicate: bool,
    ) -> Result<(), String> {
        self.semantic(
            "sep-2640-skills-enumeration",
            &format!("sep-2640-{suffix}"),
            operation,
            predicate,
        )
    }
    #[allow(clippy::too_many_lines)] // Keep each ordered protocol scenario together.
    pub(super) fn requirement_skills(&mut self) -> Result<(), String> {
        let caps = self.success("server/discover", json!({}))?["capabilities"].clone();
        self.skill_check(
            "capability-empty-object",
            "server/discover",
            rule(Rule::SkillCapability, &caps, &Value::Null),
        )?;
        self.skill_check(
            "capability-declaration-inline",
            "server/discover",
            rule(Rule::SkillDeclared, &caps, &Value::Null),
        )?;
        self.skill_check(
            "capability-requires-resources",
            "server/discover",
            rule(Rule::ResourcesDeclared, &caps, &Value::Null),
        )?;
        let (skills, cursors) = self.pages("skills/list", "skills")?;
        self.skill_check(
            "skills-list-implemented",
            "skills/list",
            rule(Rule::SkillCount, &json!(skills), &Value::Null),
        )?;
        self.skill_check(
            "skills-list-pagination",
            "skills/list",
            rule(
                Rule::SkillPagination,
                &json!({"skills":skills,"cursors":cursors}),
                &Value::Null,
            ),
        )?;
        for skill in skills {
            let uri = skill["uri"].as_str().ok_or("skill URI")?;
            let fetched = self.success("skills/get", json!({"uri":uri}))?;
            let members = skill["resources"].as_array().ok_or("members")?;
            let front = &skill["frontmatter"];
            let name = front["name"].as_str().ok_or("skill name")?;
            let base = uri.rsplit_once('/').ok_or("skill base")?.0;
            let expected = if uri.contains("/b/1/") {
                vec!["SKILL.md", "scripts/check.sh", "references/guide.md"]
            } else {
                vec!["SKILL.md", "assets/payload.bin"]
            }
            .iter()
            .map(|p| format!("{base}/{p}"))
            .collect::<BTreeSet<_>>();
            for (suffix, op, predicate) in [
                (
                    "capability-commits-to-methods",
                    "skills/get",
                    rule(
                        Rule::SkillCommit,
                        &json!({"fetched":fetched,"skill":skill}),
                        &Value::Null,
                    ),
                ),
                (
                    "skills-get-implemented",
                    "skills/get",
                    rule(Rule::SkillGetUri, &fetched, &json!(uri)),
                ),
                (
                    "skills-get-entry-shape",
                    "skills/get",
                    rule(Rule::SkillShape, &fetched, &Value::Null),
                ),
                (
                    "skills-get-no-cursor",
                    "skills/get",
                    rule(Rule::SkillNoCursor, &fetched, &Value::Null),
                ),
                (
                    "skills-list-entry-atomic",
                    "skills/list",
                    rule(
                        Rule::SkillAtomic,
                        &json!({"fetched":fetched,"skill":skill}),
                        &Value::Null,
                    ),
                ),
                (
                    "entry-uri-required",
                    "skills/list",
                    rule(Rule::UriRequired, &json!(uri), &Value::Null),
                ),
                (
                    "entry-frontmatter-required",
                    "skills/list",
                    rule(Rule::Frontmatter, front, &Value::Null),
                ),
                (
                    "entry-uri-matches-frontmatter-name",
                    "skills/list",
                    rule(
                        Rule::NameMatches,
                        &json!({"base":base,"name":name}),
                        &Value::Null,
                    ),
                ),
                (
                    "skill-uri-scheme",
                    "skills/list",
                    rule(Rule::SkillScheme, &json!(uri), &Value::Null),
                ),
                (
                    "entry-resources-required",
                    "skills/list",
                    rule(Rule::MembersRequired, &json!(members), &Value::Null),
                ),
                (
                    "resources-complete",
                    "skills/list",
                    rule(Rule::MembersComplete, &json!(members), &json!(expected)),
                ),
                (
                    "resources-uri-within-skill",
                    "skills/list",
                    rule(Rule::MembersWithin, &json!(members), &json!(base)),
                ),
                (
                    "resources-digest-format",
                    "skills/list",
                    rule(Rule::DigestFormat, &json!(members), &Value::Null),
                ),
                (
                    "resources-size-required",
                    "skills/list",
                    rule(Rule::SizeRequired, &json!(members), &Value::Null),
                ),
                (
                    "limit-resources-per-skill",
                    "skills/list",
                    rule(Rule::CountLimit, &json!(members), &Value::Null),
                ),
                (
                    "limit-total-size",
                    "skills/list",
                    rule(Rule::SizeLimit, &json!(members), &Value::Null),
                ),
                (
                    "metadata-reserved-prefix",
                    "skills/list",
                    rule(Rule::ReservedMetadata, front, &Value::Null),
                ),
                (
                    "name-naming-rules",
                    "skills/list",
                    rule(Rule::NameRules, &json!(name), &Value::Null),
                ),
                (
                    "authority-reg-name",
                    "skills/list",
                    rule(Rule::Authority, &json!(uri), &Value::Null),
                ),
                (
                    "skillmd-required",
                    "skills/list",
                    rule(
                        Rule::SkillMdRequired,
                        &json!({"uri":uri,"members":members}),
                        &Value::Null,
                    ),
                ),
            ] {
                self.skill_check(suffix, op, predicate)?;
            }
            let content =
                self.success("resources/read", json!({"uri":uri}))?["contents"][0].clone();
            let raw = String::from_utf8(bytes(&content)?).map_err(|e| e.to_string())?;
            let yaml = raw.split("---").nth(1).ok_or("frontmatter delimiters")?;
            let parsed = super::frontmatter(yaml)?;
            self.skill_check(
                "skillmd-frontmatter",
                "resources/read",
                rule(
                    Rule::FrontmatterParsed,
                    &json!({"raw":raw,"parsed":parsed}),
                    &Value::Null,
                ),
            )?;
            self.skill_check(
                "entry-frontmatter-identical",
                "resources/read",
                rule(Rule::FrontmatterEqual, &parsed, front),
            )?;
            self.semantic(
                "sep-2640-skills-manifest",
                "sep-2640-skillmd-mimetype",
                "resources/read",
                rule(Rule::Mime, &content, &Value::Null),
            )?;
            self.semantic(
                "sep-2640-skills-manifest",
                "sep-2640-final-segment-equals-name",
                "resources/read",
                rule(
                    Rule::NameMatches,
                    &json!({"base":base,"name":name}),
                    &Value::Null,
                ),
            )?;
            self.semantic(
                "sep-2640-skills-manifest",
                "sep-2640-meta-prefix",
                "resources/read",
                rule(Rule::MetaPrefix, &content, &Value::Null),
            )?;
        }
        let response = self.exchange(
            &live::modern(
                "skills/get",
                json!({"uri":"skill://pgokf/b/999999/c/YQ/deploy/SKILL.md"}),
            ),
            "reader",
            false,
            &BTreeMap::new(),
        )?;
        self.skill_check(
            "skills-get-unknown-uri-invalid-params",
            "skills/get",
            rule(
                Rule::HttpError,
                &json!({"status":response.status,"response":response.value}),
                &json!({"status":400,"code":-32602}),
            ),
        )
    }
    fn stateless_check(
        &mut self,
        suffix: &str,
        operation: &str,
        predicate: bool,
    ) -> Result<(), String> {
        self.semantic(
            "server-stateless",
            &format!("sep-2575-{suffix}"),
            operation,
            predicate,
        )
    }
    #[allow(clippy::too_many_lines)] // Keep each ordered protocol scenario together.
    pub(super) fn requirement_stateless(&mut self) -> Result<(), String> {
        for (field, suffix) in [
            (None, "missing-meta"),
            (
                Some("io.modelcontextprotocol/protocolVersion"),
                "missing-protocol-version",
            ),
            (
                Some("io.modelcontextprotocol/clientCapabilities"),
                "missing-client-capabilities",
            ),
        ] {
            let mut message = live::modern("server/discover", json!({}));
            if let Some(field) = field {
                message["params"]["_meta"]
                    .as_object_mut()
                    .ok_or("meta")?
                    .remove(field);
            } else {
                message["params"]
                    .as_object_mut()
                    .ok_or("params")?
                    .remove("_meta");
            }
            let response = self.exchange(&message, "reader", false, &BTreeMap::new())?;
            self.stateless_check(
                &format!("request-meta-invalid-{suffix}"),
                "server/discover",
                rule(Rule::ErrorCode, &response.value, &json!(-32602)),
            )?;
            if field.is_none() {
                self.stateless_check(
                    "http-server-meta-invalid-400",
                    "server/discover",
                    rule(Rule::Status, &json!(response.status), &json!(400)),
                )?;
            }
        }
        let d = self.success("server/discover", json!({}))?;
        for (suffix, predicate) in [
            (
                "request-meta-client-info-optional",
                rule(Rule::Completed, &d, &Value::Null),
            ),
            (
                "server-implements-discover",
                rule(Rule::SupportedVersions, &d, &Value::Null),
            ),
            (
                "server-identifies-in-result-meta",
                rule(Rule::ServerInfo, &d, &Value::Null),
            ),
            (
                "server-declares-prompts-in-discover",
                rule(Rule::NoPrompts, &d, &Value::Null),
            ),
            (
                "discover-capabilities-match-handlers",
                rule(Rule::Capabilities, &d, &Value::Null),
            ),
        ] {
            self.stateless_check(suffix, "server/discover", predicate)?;
        }
        let mut message = live::modern("server/discover", json!({}));
        message["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!("future");
        let response = self.exchange(
            &message,
            "reader",
            false,
            &BTreeMap::from([("MCP-Protocol-Version".into(), Some("future".into()))]),
        )?;
        self.stateless_check(
            "server-unsupported-version-error",
            "server/discover",
            rule(Rule::Unsupported, &response.value, &Value::Null),
        )?;
        self.stateless_check(
            "http-server-unsupported-version-400",
            "server/discover",
            rule(Rule::Status, &json!(response.status), &json!(400)),
        )?;
        let response = self.exchange(
            &live::modern("server/discover", json!({})),
            "reader",
            false,
            &BTreeMap::from([("MCP-Protocol-Version".into(), Some("future".into()))]),
        )?;
        self.stateless_check(
            "http-server-header-mismatch-400",
            "server/discover",
            rule(
                Rule::HttpError,
                &json!({"status":response.status,"response":response.value}),
                &json!({"status":400,"code":-32020}),
            ),
        )?;
        for (method, suffix) in [
            ("initialize", "initialize"),
            ("ping", "ping"),
            ("logging/setLevel", "logging-setlevel"),
            ("resources/subscribe", "resources-subscribe"),
            ("resources/unsubscribe", "resources-unsubscribe"),
            ("unknown/method", ""),
        ] {
            let response = self.exchange(
                &live::modern(method, json!({})),
                "reader",
                false,
                &BTreeMap::new(),
            )?;
            let check = if suffix.is_empty() {
                "http-server-method-not-found-404".into()
            } else {
                format!("http-server-method-not-found-404-{suffix}")
            };
            self.stateless_check(
                &check,
                method,
                rule(
                    Rule::HttpError,
                    &json!({"status":response.status,"response":response.value}),
                    &json!({"status":404,"code":-32601}),
                ),
            )?;
            self.stateless_check(
                "http-server-error-jsonrpc-id",
                method,
                rule(Rule::Id, &response.value, &Value::Null),
            )?;
        }
        Ok(())
    }
    pub(super) fn requirement_headers(&mut self) -> Result<(), String> {
        for field in ["Accept", "MCP-Protocol-Version", "Mcp-Method"] {
            let r = self.exchange(
                &live::modern("tools/list", json!({})),
                "reader",
                false,
                &BTreeMap::from([(field.into(), None)]),
            )?;
            self.semantic(
                "http-header-validation",
                "sep-2243-server-reject-invalid-headers",
                field,
                rule(Rule::Status, &json!(r.status), &json!(400)),
            )?;
            self.semantic(
                "http-header-validation",
                "sep-2243-server-reject-error-code",
                field,
                rule(Rule::ErrorCode, &r.value, &json!(-32020)),
            )?;
        }
        for (changes, check) in [
            (
                BTreeMap::from([("Mcp-Method".into(), Some(" tools/list ".into()))]),
                "sep-2243-server-accepts-whitespace-header-value",
            ),
            (
                BTreeMap::from([
                    ("Mcp-Method".into(), None),
                    ("mcp-method".into(), Some("tools/list".into())),
                ]),
                "sep-2243-header-name-case-insensitive",
            ),
        ] {
            let r = self.exchange(
                &live::modern("tools/list", json!({})),
                "reader",
                false,
                &changes,
            )?;
            self.semantic(
                "http-header-validation",
                check,
                "tools/list",
                rule(
                    Rule::ResultTools,
                    &json!({"status":r.status,"response":r.value}),
                    &Value::Null,
                ),
            )?;
        }
        let message = live::modern("server/discover", json!({}));
        let mut h = live::headers(&message);
        h.insert("Origin".into(), "https://hostile.invalid".into());
        let r = self.endpoint.call(
            "POST",
            Some(&live::token("reader")),
            Some(&message.to_string()),
            &h,
        )?;
        self.semantic(
            "dns-rebinding-protection",
            "localhost-host-rebinding-rejected",
            "server/discover",
            rule(Rule::Status, &json!(r.status), &json!(403)),
        )?;
        let r = self.exchange(&message, "reader", false, &BTreeMap::new())?;
        self.semantic(
            "dns-rebinding-protection",
            "localhost-host-valid-accepted",
            "server/discover",
            rule(
                Rule::DiscoveryHttp,
                &json!({"status":r.status,"response":r.value}),
                &Value::Null,
            ),
        )
    }
    pub(super) fn requirement_wire(&mut self) -> Result<(), String> {
        let (skills, _) = self.pages("skills/list", "skills")?;
        let binary = skills
            .iter()
            .filter_map(|s| s["resources"].as_array())
            .flatten()
            .find(|r| {
                r["uri"]
                    .as_str()
                    .is_some_and(|s| s.ends_with("/assets/payload.bin"))
            })
            .ok_or("binary member")?["uri"]
            .clone();
        for (scenario, method, params) in [
            ("caching", "tools/list", json!({})),
            ("caching", "resources/list", json!({})),
            ("caching", "resources/templates/list", json!({})),
            (
                "caching",
                "resources/read",
                json!({"uri":"okf://catalog/manifest"}),
            ),
            ("http-header-validation", "tools/list", json!({})),
            ("json-schema-2020-12", "tools/list", json!({})),
            ("resources-list", "resources/list", json!({})),
            (
                "resources-read-binary",
                "resources/read",
                json!({"uri":binary}),
            ),
            (
                "resources-read-text",
                "resources/read",
                json!({"uri":skill_uri()}),
            ),
            (
                "resources-templates-read",
                "resources/templates/list",
                json!({}),
            ),
            (
                "resources-templates-read",
                "resources/read",
                json!({"uri":skill_uri()}),
            ),
            (
                "sep-2164-resource-not-found",
                "resources/read",
                json!({"uri":"okf://catalog/bundles/999999"}),
            ),
            (
                "tools-call-error",
                "tools/call",
                json!({"name":"get_concept","arguments":{"unadvertised_argument":true}}),
            ),
            (
                "tools-call-simple-text",
                "tools/call",
                json!({"name":"get_concept","arguments":{"bundle_id":1,"concept_id":"services/widget"}}),
            ),
            ("tools-list", "tools/list", json!({})),
            ("sep-2640-skills-enumeration", "skills/list", json!({})),
            (
                "sep-2640-skills-enumeration",
                "skills/get",
                json!({"uri":skill_uri()}),
            ),
            (
                "sep-2640-skills-manifest",
                "resources/read",
                json!({"uri":skill_uri()}),
            ),
            (
                "sep-2640-skills-directory",
                "resources/directory/read",
                json!({"uri":skill_uri().trim_end_matches("/SKILL.md")}),
            ),
        ] {
            let message = live::modern(method, params);
            let response = self.exchange(&message, "reader", false, &BTreeMap::new())?;
            let valid = self.wire.exchange(&message, &response.value).is_ok();
            self.semantic(scenario, "wire-schema-valid", method, valid)?;
        }
        Ok(())
    }
}
