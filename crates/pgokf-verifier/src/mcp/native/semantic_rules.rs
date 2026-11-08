//! Pure field-sensitive predicates shared by live scenarios and negative controls.
use super::{PREFIX, Value, bytes, json};
use std::collections::BTreeSet;
#[derive(Clone, Copy, Debug)]
pub(super) enum Rule {
    CacheScope,
    CachePositive,
    CacheTtl,
    ToolList,
    ToolName,
    SchemaDraft,
    ToolText,
    ToolError,
    ResourceList,
    ResourceText,
    ResourceBinary,
    ResourceTemplate,
    ResourceError,
    ErrorCode,
    Status,
    HttpError,
    ResultTools,
    DiscoveryHttp,
    Id,
    SkillCapability,
    SkillDeclared,
    ResourcesDeclared,
    SkillCount,
    SkillPagination,
    SkillCommit,
    SkillGetUri,
    SkillShape,
    SkillNoCursor,
    SkillAtomic,
    UriRequired,
    Frontmatter,
    NameMatches,
    SkillScheme,
    MembersRequired,
    MembersComplete,
    MembersWithin,
    DigestFormat,
    SizeRequired,
    CountLimit,
    SizeLimit,
    ReservedMetadata,
    NameRules,
    Authority,
    SkillMdRequired,
    FrontmatterParsed,
    FrontmatterEqual,
    Mime,
    MetaPrefix,
    Completed,
    SupportedVersions,
    ServerInfo,
    NoPrompts,
    Capabilities,
    Unsupported,
}
fn array(v: &Value) -> Option<&Vec<Value>> {
    v.as_array()
}
fn text(v: &Value) -> bool {
    v["type"] == "text" && v["text"].as_str().is_some_and(|s| !s.is_empty())
}
fn one(v: &Value) -> bool {
    array(&v["contents"]).is_some_and(|a| a.len() == 1)
}
#[allow(clippy::too_many_lines)] // One arm per independently controlled normative rule.
pub(super) fn check(rule: Rule, v: &Value, e: &Value) -> bool {
    match rule {
        Rule::CacheScope => v["cacheScope"] == *e,
        Rule::CachePositive => v["ttlMs"].as_u64().is_some_and(|n| n > 0),
        Rule::CacheTtl => v["ttlMs"].as_u64().is_some_and(|n| Some(n) == e.as_u64()),
        Rule::ToolList => array(&v["tools"]).is_some_and(|a| !a.is_empty()),
        Rule::ToolName => v.as_str().is_some_and(|s| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        }),
        Rule::SchemaDraft => {
            v["inputSchema"]["$schema"] == "https://json-schema.org/draft/2020-12/schema"
        }
        Rule::ToolText => {
            v["isError"] == false && array(&v["content"]).is_some_and(|a| a.iter().any(text))
        }
        Rule::ToolError => {
            v["isError"] == true
                && array(&v["content"]).is_some_and(|a| !a.is_empty() && a.iter().all(text))
        }
        Rule::ResourceList => array(v).is_some_and(|a| {
            !a.is_empty()
                && a.iter()
                    .all(|r| r["uri"].is_string() && r["name"].is_string())
        }),
        Rule::ResourceText => {
            one(v)
                && v["contents"][0]["uri"] == e["uri"]
                && bytes(&v["contents"][0])
                    .is_ok_and(|b| Some(b.as_slice()) == e["text"].as_str().map(str::as_bytes))
        }
        Rule::ResourceBinary => {
            one(v)
                && v["contents"][0].get("blob").is_some()
                && bytes(&v["contents"][0]).is_ok_and(|b| b == [0, 255, 128])
        }
        Rule::ResourceTemplate => {
            one(v)
                && v["contents"][0]["uri"] == *e
                && bytes(&v["contents"][0]).is_ok_and(|b| !b.is_empty())
        }
        Rule::ResourceError => {
            v["status"] == 400
                && v["response"].get("result").is_none()
                && v["response"].get("error").is_some()
        }
        Rule::ErrorCode => v["error"]["code"] == *e,
        Rule::Status => v.as_u64().zip(e.as_u64()).is_some_and(|(a, b)| a == b),
        Rule::HttpError => {
            v["status"] == e["status"] && v["response"]["error"]["code"] == e["code"]
        }
        Rule::ResultTools => {
            v["status"] == 200
                && array(&v["response"]["result"]["tools"]).is_some_and(|a| !a.is_empty())
        }
        Rule::DiscoveryHttp => {
            v["status"] == 200 && v["response"]["result"]["resultType"] == "complete"
        }
        Rule::Id => v["id"].as_i64() == Some(1),
        Rule::SkillCapability => v["extensions"]["io.modelcontextprotocol/skills"] == json!({}),
        Rule::SkillDeclared => v["extensions"]
            .get("io.modelcontextprotocol/skills")
            .is_some(),
        Rule::ResourcesDeclared => v.get("resources").is_some(),
        Rule::SkillCount => array(v).is_some_and(|a| a.len() == 55),
        Rule::SkillPagination => {
            array(&v["cursors"]).is_some_and(|a| !a.is_empty())
                && array(&v["skills"]).is_some_and(|a| {
                    a.iter()
                        .map(|s| s["uri"].as_str())
                        .collect::<BTreeSet<_>>()
                        .len()
                        == a.len()
                })
        }
        Rule::SkillCommit => v["fetched"]["skill"] == v["skill"],
        Rule::SkillGetUri => v["skill"]["uri"] == *e,
        Rule::SkillShape => v["skill"].as_object().is_some_and(|m| {
            m.len() == 4
                && ["uri", "frontmatter", "resources", "_meta"]
                    .iter()
                    .all(|k| m.contains_key(*k))
        }),
        Rule::SkillNoCursor => v.get("nextCursor").is_none(),
        Rule::SkillAtomic => {
            v["fetched"]["skill"] == v["skill"] && v["skill"]["resources"].is_array()
        }
        Rule::UriRequired => v.as_str().is_some_and(|s| !s.is_empty()),
        Rule::Frontmatter => {
            v.is_object()
                && ["name", "description"]
                    .iter()
                    .all(|k| v[k].as_str().is_some_and(|s| !s.is_empty()))
        }
        Rule::NameMatches => v["base"]
            .as_str()
            .is_some_and(|s| s.rsplit('/').next() == v["name"].as_str()),
        Rule::SkillScheme => v.as_str().is_some_and(|s| s.starts_with("skill:")),
        Rule::MembersRequired => array(v).is_some_and(|a| !a.is_empty()),
        Rule::MembersComplete => array(v).zip(array(e)).is_some_and(|(a, b)| {
            a.iter()
                .map(|r| r["uri"].as_str().unwrap_or_default())
                .collect::<BTreeSet<_>>()
                == b.iter().filter_map(Value::as_str).collect::<BTreeSet<_>>()
        }),
        Rule::MembersWithin => array(v).zip(e.as_str()).is_some_and(|(a, base)| {
            a.iter().all(|r| {
                r["uri"]
                    .as_str()
                    .is_some_and(|s| s.starts_with(&format!("{base}/")))
            })
        }),
        Rule::DigestFormat => array(v).is_some_and(|a| {
            a.iter().all(|r| {
                r["digest"].as_str().is_some_and(|s| {
                    s.starts_with("sha256:")
                        && s.len() == 71
                        && s[7..]
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
            })
        }),
        Rule::SizeRequired => {
            array(v).is_some_and(|a| a.iter().all(|r| r["size"].as_u64().is_some()))
        }
        Rule::CountLimit => array(v).is_some_and(|a| a.len() <= 512),
        Rule::SizeLimit => array(v).is_some_and(|a| {
            a.iter()
                .try_fold(0u64, |sum, r| sum.checked_add(r["size"].as_u64()?))
                .is_some_and(|n| n <= 16 * 1024 * 1024)
        }),
        Rule::ReservedMetadata => v
            .get("metadata")
            .and_then(Value::as_object)
            .is_none_or(|m| m.keys().all(|k| !k.starts_with("io.modelcontextprotocol/"))),
        Rule::NameRules => v.as_str().is_some_and(|s| {
            !s.is_empty()
                && s.len() <= 64
                && s.split('-').all(|p| {
                    !p.is_empty()
                        && p.bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                })
        }),
        Rule::Authority => v.as_str().is_some_and(|s| s.starts_with("skill://pgokf/")),
        Rule::SkillMdRequired => {
            v["uri"].as_str().is_some_and(|s| s.ends_with("/SKILL.md"))
                && array(&v["members"]).is_some_and(|a| a.iter().any(|r| r["uri"] == v["uri"]))
        }
        Rule::FrontmatterParsed => {
            v["raw"].as_str().is_some_and(|s| s.starts_with("---\n"))
                && v["parsed"].get("name").is_some()
                && v["parsed"].get("description").is_some()
        }
        Rule::FrontmatterEqual => *v == *e,
        Rule::Mime => v["mimeType"] == "text/markdown",
        Rule::MetaPrefix => v["_meta"]
            .as_object()
            .is_some_and(|m| m.keys().all(|k| k.starts_with(PREFIX))),
        Rule::Completed => v["resultType"] == "complete",
        Rule::SupportedVersions => v["supportedVersions"] == json!(["2026-07-28"]),
        Rule::ServerInfo => {
            v["_meta"]["io.modelcontextprotocol/serverInfo"]["name"] == "pgokf-mcp"
                && v["_meta"]["io.modelcontextprotocol/serverInfo"]["version"].is_string()
        }
        Rule::NoPrompts => v["capabilities"].get("prompts").is_none(),
        Rule::Capabilities => {
            v["capabilities"]
                == json!({"tools":{"listChanged":false},"resources":{},"extensions":{"io.modelcontextprotocol/skills":{}}})
        }
        Rule::Unsupported => {
            v["error"]["code"] == -32022
                && v["error"]["data"] == json!({"requested":"future","supported":["2026-07-28"]})
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn changed(v: &Value, p: &str, x: Value) -> Value {
        let mut v = v.clone();
        *v.pointer_mut(p).unwrap() = x;
        v
    }
    struct Case {
        rule: Rule,
        good: Value,
        expected: Value,
        bad: Vec<Value>,
    }
    fn case(rule: Rule, good: Value, expected: Value, bad: Vec<Value>) -> Case {
        Case {
            rule,
            good,
            expected,
            bad,
        }
    }
    #[allow(clippy::too_many_lines)] // Independent response mutations enumerate every rule and its fields.
    fn cases() -> Vec<Case> {
        use Rule::*;
        let text = json!({"isError":false,"content":[{"type":"text","text":"hello"}]});
        let error = json!({"isError":true,"content":[{"type":"text","text":"denied"}]});
        let resource = json!({"contents":[{"uri":"okf://document","text":"exact bytes"}]});
        let member = json!([{"uri":"skill://pgokf/deploy/SKILL.md","digest":format!("sha256:{}","a".repeat(64)),"size":1}]);
        let front = json!({"name":"deploy","description":"Use safely","metadata":{"team":"ops"}});
        let skill = json!({"uri":"skill://pgokf/deploy/SKILL.md","frontmatter":front,"resources":member,"_meta":{}});
        let fetched = json!({"skill":skill});
        let pair = json!({"fetched":fetched,"skill":skill});
        let mut unequal = pair.clone();
        unequal["fetched"]["skill"]["uri"] = json!("different");
        let caps = json!({"tools":{"listChanged":false},"resources":{},"extensions":{"io.modelcontextprotocol/skills":{}}});
        let discover = json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":caps,"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"pgokf-mcp","version":"1"}}});
        let unsupported = json!({"error":{"code":-32022,"data":{"requested":"future","supported":["2026-07-28"]}}});
        let paged = json!({"skills":[{"uri":"a"},{"uri":"b"}],"cursors":["cursor"]});
        let http = json!({"status":400,"response":{"error":{"code":-32602}}});
        vec![
            case(
                CacheScope,
                json!({"cacheScope":"private"}),
                json!("private"),
                vec![json!({"cacheScope":"public"}), json!({})],
            ),
            case(
                CachePositive,
                json!({"ttlMs":1}),
                Value::Null,
                vec![
                    json!({"ttlMs":0}),
                    json!({"ttlMs":-1}),
                    json!({"ttlMs":1.5}),
                    json!({"ttlMs":"1"}),
                ],
            ),
            case(
                CacheTtl,
                json!({"ttlMs":30000}),
                json!(30000),
                vec![json!({"ttlMs":30001}), json!({"ttlMs":"30000"})],
            ),
            case(
                ToolList,
                json!({"tools":[{}]}),
                Value::Null,
                vec![json!({"tools":[]}), json!({"tools":{}})],
            ),
            case(
                ToolName,
                json!("get_tool-1.v2"),
                Value::Null,
                vec![
                    json!(""),
                    json!("bad/name"),
                    json!("x".repeat(129)),
                    json!(2),
                ],
            ),
            case(
                SchemaDraft,
                json!({"inputSchema":{"$schema":"https://json-schema.org/draft/2020-12/schema"}}),
                Value::Null,
                vec![
                    json!({"inputSchema":{"$schema":"http://json-schema.org/draft-07/schema#"}}),
                    json!({}),
                ],
            ),
            case(
                ToolText,
                text.clone(),
                Value::Null,
                vec![
                    changed(&text, "/isError", json!(true)),
                    changed(&text, "/content/0/type", json!("image")),
                    changed(&text, "/content/0/text", json!("")),
                    changed(&text, "/content", json!([])),
                ],
            ),
            case(
                ToolError,
                error.clone(),
                Value::Null,
                vec![
                    changed(&error, "/isError", json!(false)),
                    changed(&error, "/content/0/type", json!("image")),
                    changed(&error, "/content/0/text", json!("")),
                    changed(&error, "/content", json!([])),
                ],
            ),
            case(
                ResourceList,
                json!([{"uri":"a","name":"name"}]),
                Value::Null,
                vec![
                    json!([]),
                    json!([{"uri":1,"name":"n"}]),
                    json!([{"uri":"a","name":null}]),
                ],
            ),
            case(
                ResourceText,
                resource.clone(),
                json!({"uri":"okf://document","text":"exact bytes"}),
                vec![
                    changed(&resource, "/contents", json!([])),
                    changed(&resource, "/contents/0/uri", json!("other")),
                    changed(&resource, "/contents/0/text", json!("changed")),
                ],
            ),
            case(
                ResourceBinary,
                json!({"contents":[{"blob":"AP+A"}]}),
                Value::Null,
                vec![
                    json!({"contents":[]}),
                    json!({"contents":[{"blob":"AAAA"}]}),
                    json!({"contents":[{"text":"AP+A"}]}),
                ],
            ),
            case(
                ResourceTemplate,
                resource.clone(),
                json!("okf://document"),
                vec![
                    changed(&resource, "/contents", json!([])),
                    changed(&resource, "/contents/0/uri", json!("other")),
                    changed(&resource, "/contents/0/text", json!("")),
                ],
            ),
            case(
                ResourceError,
                http.clone(),
                Value::Null,
                vec![
                    changed(&http, "/status", json!(200)),
                    json!({"status":400,"response":{"error":{},"result":{}}}),
                    json!({"status":400,"response":{}}),
                ],
            ),
            case(
                ErrorCode,
                json!({"error":{"code":-32602}}),
                json!(-32602),
                vec![json!({"error":{"code":-32020}}), json!({})],
            ),
            case(
                Status,
                json!(400),
                json!(400),
                vec![json!(200), json!("400")],
            ),
            case(
                HttpError,
                http.clone(),
                json!({"status":400,"code":-32602}),
                vec![
                    changed(&http, "/status", json!(404)),
                    changed(&http, "/response/error/code", json!(-32601)),
                ],
            ),
            case(
                ResultTools,
                json!({"status":200,"response":{"result":{"tools":[{}]}}}),
                Value::Null,
                vec![
                    json!({"status":400,"response":{"result":{"tools":[{}]}}}),
                    json!({"status":200,"response":{"result":{"tools":[]}}}),
                ],
            ),
            case(
                DiscoveryHttp,
                json!({"status":200,"response":{"result":{"resultType":"complete"}}}),
                Value::Null,
                vec![
                    json!({"status":400,"response":{"result":{"resultType":"complete"}}}),
                    json!({"status":200,"response":{"result":{"resultType":"task"}}}),
                ],
            ),
            case(
                Id,
                json!({"id":1}),
                Value::Null,
                vec![json!({"id":"1"}), json!({"id":true}), json!({"id":2})],
            ),
            case(
                SkillCapability,
                caps.clone(),
                Value::Null,
                vec![
                    changed(
                        &caps,
                        "/extensions/io.modelcontextprotocol~1skills",
                        json!({"unexpected":true}),
                    ),
                    json!({}),
                ],
            ),
            case(
                SkillDeclared,
                caps.clone(),
                Value::Null,
                vec![json!({"extensions":{}})],
            ),
            case(
                ResourcesDeclared,
                caps.clone(),
                Value::Null,
                vec![json!({})],
            ),
            case(
                SkillCount,
                json!(vec![json!({}); 55]),
                Value::Null,
                vec![json!(vec![json!({}); 54]), json!(vec![json!({}); 56])],
            ),
            case(
                SkillPagination,
                paged.clone(),
                Value::Null,
                vec![
                    changed(&paged, "/cursors", json!([])),
                    changed(&paged, "/skills/1/uri", json!("a")),
                ],
            ),
            case(
                SkillCommit,
                pair.clone(),
                Value::Null,
                vec![unequal.clone()],
            ),
            case(
                SkillGetUri,
                fetched.clone(),
                skill["uri"].clone(),
                vec![changed(&fetched, "/skill/uri", json!("other"))],
            ),
            case(
                SkillShape,
                fetched.clone(),
                Value::Null,
                vec![
                    json!({"skill":{"uri":"x","frontmatter":{},"resources":[],"wrong":{}}}),
                    json!({"skill":{"uri":"x","frontmatter":{},"resources":[],"_meta":{},"extra":0}}),
                ],
            ),
            case(
                SkillNoCursor,
                fetched.clone(),
                Value::Null,
                vec![json!({"skill":skill,"nextCursor":"cursor"})],
            ),
            case(
                SkillAtomic,
                pair,
                Value::Null,
                vec![
                    unequal,
                    json!({"skill":{"resources":{}},"fetched":{"skill":{"resources":{}}}}),
                ],
            ),
            case(
                UriRequired,
                json!("skill://pgokf/deploy/SKILL.md"),
                Value::Null,
                vec![json!(""), json!(null)],
            ),
            case(
                Frontmatter,
                front.clone(),
                Value::Null,
                vec![
                    changed(&front, "/name", json!("")),
                    changed(&front, "/description", json!("")),
                    json!([]),
                ],
            ),
            case(
                NameMatches,
                json!({"base":"skill://pgokf/deploy","name":"deploy"}),
                Value::Null,
                vec![
                    json!({"base":"skill://pgokf/deploy","name":"other"}),
                    json!({"base":"skill://pgokf/deploy/","name":"deploy"}),
                ],
            ),
            case(
                SkillScheme,
                json!("skill://pgokf/deploy/SKILL.md"),
                Value::Null,
                vec![json!("https://pgokf/deploy/SKILL.md")],
            ),
            case(
                MembersRequired,
                member.clone(),
                Value::Null,
                vec![json!([]), json!({})],
            ),
            case(
                MembersComplete,
                member.clone(),
                json!(["skill://pgokf/deploy/SKILL.md"]),
                vec![json!([]), changed(&member, "/0/uri", json!("other"))],
            ),
            case(
                MembersWithin,
                member.clone(),
                json!("skill://pgokf/deploy"),
                vec![
                    changed(
                        &member,
                        "/0/uri",
                        json!("skill://pgokf/deploy-other/SKILL.md"),
                    ),
                    changed(&member, "/0/uri", json!(7)),
                ],
            ),
            case(
                DigestFormat,
                member.clone(),
                Value::Null,
                vec![
                    changed(
                        &member,
                        "/0/digest",
                        json!(format!("sha512:{}", "a".repeat(64))),
                    ),
                    changed(&member, "/0/digest", json!("sha256:a")),
                    changed(
                        &member,
                        "/0/digest",
                        json!(format!("sha256:{}", "G".repeat(64))),
                    ),
                ],
            ),
            case(
                SizeRequired,
                member.clone(),
                Value::Null,
                vec![
                    changed(&member, "/0/size", json!(-1)),
                    changed(&member, "/0/size", json!("1")),
                ],
            ),
            case(
                CountLimit,
                json!(vec![json!({}); 512]),
                Value::Null,
                vec![json!(vec![json!({}); 513])],
            ),
            case(
                SizeLimit,
                json!([{"size":16_777_216}]),
                Value::Null,
                vec![
                    json!([{"size":16_777_217}]),
                    json!([{"size":u64::MAX},{"size":1}]),
                    json!([{}]),
                ],
            ),
            case(
                ReservedMetadata,
                front.clone(),
                Value::Null,
                vec![json!({"metadata":{"io.modelcontextprotocol/secret":"x"}})],
            ),
            case(
                NameRules,
                json!("deploy-123"),
                Value::Null,
                vec![
                    json!(""),
                    json!("a".repeat(65)),
                    json!("Upper"),
                    json!("double--dash"),
                    json!("leading-"),
                ],
            ),
            case(
                Authority,
                json!("skill://pgokf/deploy/SKILL.md"),
                Value::Null,
                vec![
                    json!("skill://other/deploy/SKILL.md"),
                    json!("skill://pgokf.evil/deploy/SKILL.md"),
                ],
            ),
            case(
                SkillMdRequired,
                json!({"uri":skill["uri"],"members":member}),
                Value::Null,
                vec![
                    json!({"uri":"skill://pgokf/deploy/OTHER.md","members":[{"uri":"skill://pgokf/deploy/OTHER.md"}]}),
                    json!({"uri":skill["uri"],"members":[]}),
                ],
            ),
            case(
                FrontmatterParsed,
                json!({"raw":"---\nname: deploy\n---","parsed":front}),
                Value::Null,
                vec![
                    json!({"raw":"missing delimiter","parsed":front}),
                    json!({"raw":"---\n","parsed":{"description":"x"}}),
                    json!({"raw":"---\n","parsed":{"name":"x"}}),
                ],
            ),
            case(
                FrontmatterEqual,
                front.clone(),
                front.clone(),
                vec![changed(&front, "/description", json!("changed"))],
            ),
            case(
                Mime,
                json!({"mimeType":"text/markdown"}),
                Value::Null,
                vec![json!({"mimeType":"text/plain"}), json!({})],
            ),
            case(
                MetaPrefix,
                json!({"_meta":{format!("{PREFIX}member"):1}}),
                Value::Null,
                vec![json!({"_meta":{"other/name":1}}), json!({})],
            ),
            case(
                Completed,
                discover.clone(),
                Value::Null,
                vec![changed(&discover, "/resultType", json!("task"))],
            ),
            case(
                SupportedVersions,
                discover.clone(),
                Value::Null,
                vec![
                    changed(&discover, "/supportedVersions", json!(["2025-06-18"])),
                    changed(
                        &discover,
                        "/supportedVersions",
                        json!(["2026-07-28", "future"]),
                    ),
                ],
            ),
            case(
                ServerInfo,
                discover.clone(),
                Value::Null,
                vec![
                    changed(
                        &discover,
                        "/_meta/io.modelcontextprotocol~1serverInfo/name",
                        json!("other"),
                    ),
                    changed(
                        &discover,
                        "/_meta/io.modelcontextprotocol~1serverInfo/version",
                        json!(1),
                    ),
                ],
            ),
            case(
                NoPrompts,
                discover.clone(),
                Value::Null,
                vec![json!({"capabilities":{"prompts":{}}})],
            ),
            case(
                Capabilities,
                discover.clone(),
                Value::Null,
                vec![
                    changed(&discover, "/capabilities/tools/listChanged", json!(true)),
                    json!({"capabilities":{}}),
                ],
            ),
            case(
                Unsupported,
                unsupported.clone(),
                Value::Null,
                vec![
                    changed(&unsupported, "/error/code", json!(-32602)),
                    changed(&unsupported, "/error/data/requested", json!("other")),
                    changed(&unsupported, "/error/data/supported", json!([])),
                ],
            ),
        ]
    }
    #[test]
    fn every_rule_accepts_observation_and_rejects_field_corruptions() {
        let cases = cases();
        assert_eq!(cases.len(), 54);
        let mut names = BTreeSet::new();
        for case in cases {
            assert!(names.insert(format!("{:?}", case.rule)));
            assert!(
                check(case.rule, &case.good, &case.expected),
                "positive {:?}",
                case.rule
            );
            assert!(!case.bad.is_empty());
            for (i, bad) in case.bad.iter().enumerate() {
                assert!(
                    !check(case.rule, bad, &case.expected),
                    "negative {:?} {i}",
                    case.rule
                );
            }
        }
    }
}
