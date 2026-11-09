use pgokf_verifier::workflow::*;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};
fn workflow(name: &str) -> Value {
    parse(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(format!(".github/workflows/{name}.yml")),
        )
        .unwrap(),
    )
    .unwrap()
}
fn contexts() -> Contexts {
    serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("tests/workflow-contexts.json"),
        )
        .unwrap(),
    )
    .unwrap()
}

fn pages_metadata(version: &str) -> Value {
    let pgokf = format!("path+file:///workspace/crates/extension#pgokf@{version}");
    let parser = format!("path+file:///workspace/crates/okf-parser#okf-parser@{version}");
    json!({
        "packages": [
            {"id": pgokf, "name": "pgokf", "version": version},
            {"id": parser, "name": "okf-parser", "version": version}
        ],
        "workspace_members": [pgokf, parser]
    })
}

fn run_pages_policy(event: &str, manual_allow: &str, metadata: &Value) -> (bool, String) {
    let output = tempfile::NamedTempFile::new().unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("scripts/pages-deploy-policy.sh");
    let mut child = Command::new("bash")
        .arg(script)
        .env("GITHUB_EVENT_NAME", event)
        .env("INPUT_DEPLOY_PAGES", manual_allow)
        .env("GITHUB_OUTPUT", output.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(metadata).unwrap().as_bytes())
        .unwrap();
    let success = child.wait().unwrap().success();
    let outputs = std::fs::read_to_string(output.path()).unwrap();
    (success, outputs)
}

#[test]
fn pages_policy_allows_only_explicit_manual_dispatch() {
    for version in ["0.4.0", "0.4.0-dev3"] {
        let (success, outputs) =
            run_pages_policy("workflow_dispatch", "true", &pages_metadata(version));
        assert!(success, "{version}");
        assert_eq!(
            outputs,
            format!("version={version}\ndeploy=true\n"),
            "{version}"
        );
    }
}

#[test]
fn pages_policy_blocks_every_push_and_manual_default_or_false() {
    for version in ["0.3.1", "0.4.0", "0.4.0-dev3"] {
        let (success, outputs) = run_pages_policy("push", "", &pages_metadata(version));
        assert!(success, "push {version}");
        assert_eq!(
            outputs,
            format!("version={version}\ndeploy=false\n"),
            "push {version}"
        );
    }
    for manual_allow in ["", "false"] {
        let (success, outputs) =
            run_pages_policy("workflow_dispatch", manual_allow, &pages_metadata("0.4.0"));
        assert!(success, "manual {manual_allow:?}");
        assert_eq!(
            outputs, "version=0.4.0\ndeploy=false\n",
            "manual {manual_allow:?}"
        );
    }
}

#[test]
fn pages_policy_rejects_missing_malformed_and_mismatched_versions() {
    let mut mismatch = pages_metadata("0.4.0");
    mismatch["packages"][1]["version"] = json!("0.4.1");
    for metadata in [
        json!({}),
        pages_metadata("dev3"),
        pages_metadata("0.4.0-01"),
        pages_metadata("0.4.0\n"),
        json!({
            "packages":[{"id":"pgokf","name":"pgokf","version":"0.4.0"}],
            "workspace_members":"pgokf"
        }),
        json!({
            "packages":{"id":"pgokf","name":"pgokf","version":"0.4.0"},
            "workspace_members":["pgokf"]
        }),
        json!({
            "packages":[
                {"id":"A","name":"pgokf","version":"0.4.0"},
                {"id":"B","name":"okf-parser","version":"0.4.0"},
                {"id":"B","name":"okf-parser","version":"0.4.0"},
                {"id":"C-actual","name":"third","version":"0.4.0-dev3"}
            ],
            "workspace_members":["A","B","C"]
        }),
        mismatch,
    ] {
        let (success, outputs) = run_pages_policy("push", "", &metadata);
        assert!(!success, "{metadata}");
        assert!(outputs.is_empty(), "{metadata}: {outputs}");
    }
    let (success, outputs) = run_pages_policy("workflow_dispatch", "yes", &pages_metadata("0.4.0"));
    assert!(!success);
    assert!(outputs.is_empty());
}

#[test]
fn docs_pages_workflow_has_a_fail_closed_deployment_gate() {
    assert_eq!(
        check_pages_policy(&workflow("docs"), &contexts()),
        Vec::<String>::new()
    );
}

#[test]
fn docs_pages_build_is_not_suppressed_by_workflow_concurrency() {
    let docs = workflow("docs");
    assert!(docs.get("concurrency").is_none());
    assert_eq!(
        docs["jobs"]["deploy"]["concurrency"],
        json!({"group":"pages","cancel-in-progress":false})
    );
}

#[test]
fn docs_pages_gate_rejects_bypasses_and_failed_build_deployment() {
    for case in [
        "manual-default",
        "push-path-filter",
        "policy-continue",
        "build-continue",
        "missing-build-need",
        "failed-build-bypass",
        "push-event-bypass",
        "manual-input-bypass",
        "deploy-continue",
        "workflow-write-permission",
        "policy-write-permission",
        "build-write-permission",
        "deploy-missing-concurrency",
        "policy-script-replacement",
        "unknown-pages-writer",
        "workflow-default-shell",
        "policy-default-shell",
        "policy-container",
        "build-default-shell",
        "build-site-replacement",
    ] {
        let mut docs = workflow("docs");
        match case {
            "manual-default" => {
                docs["on"]["workflow_dispatch"]["inputs"]["deploy_pages"]["default"] = json!(true);
            }
            "push-path-filter" => {
                docs["on"]["push"]["paths"] = json!(["docs/**"]);
            }
            "policy-continue" => docs["jobs"]["pages-policy"]["continue-on-error"] = json!(true),
            "build-continue" => docs["jobs"]["build"]["continue-on-error"] = json!(true),
            "missing-build-need" => docs["jobs"]["deploy"]["needs"] = json!(["pages-policy"]),
            "failed-build-bypass" => {
                docs["jobs"]["deploy"]["if"] =
                    json!("${{ github.event_name == 'workflow_dispatch' && inputs.deploy_pages == true && needs.pages-policy.result == 'success' && needs.pages-policy.outputs.deploy == 'true' }}");
            }
            "push-event-bypass" => {
                docs["jobs"]["deploy"]["if"] =
                    json!("${{ inputs.deploy_pages == true && needs.build.result == 'success' && needs.pages-policy.result == 'success' && needs.pages-policy.outputs.deploy == 'true' }}");
            }
            "manual-input-bypass" => {
                docs["jobs"]["deploy"]["if"] =
                    json!("${{ github.event_name == 'workflow_dispatch' && needs.build.result == 'success' && needs.pages-policy.result == 'success' && needs.pages-policy.outputs.deploy == 'true' }}");
            }
            "deploy-continue" => docs["jobs"]["deploy"]["continue-on-error"] = json!(true),
            "workflow-write-permission" => docs["permissions"]["pages"] = json!("write"),
            "policy-write-permission" => {
                docs["jobs"]["pages-policy"]["permissions"] = json!({"pages":"write"});
            }
            "build-write-permission" => {
                docs["jobs"]["build"]["permissions"] = json!({"id-token":"write"});
            }
            "deploy-missing-concurrency" => {
                docs["jobs"]["deploy"].as_object_mut().unwrap().remove("concurrency");
            }
            "policy-script-replacement" => docs["jobs"]["pages-policy"]["steps"]
                .as_array_mut()
                .unwrap()
                .insert(
                    1,
                    json!({"run":"printf '#!/bin/sh\\nprintf deploy=true\\n' > scripts/pages-deploy-policy.sh"}),
                ),
            "workflow-default-shell" => {
                docs["defaults"] = json!({"run":{"shell":"bash scripts/hostile-shell.sh {0}"}});
            }
            "policy-default-shell" => {
                docs["jobs"]["pages-policy"]["defaults"] =
                    json!({"run":{"shell":"bash scripts/hostile-shell.sh {0}"}});
            }
            "policy-container" => {
                docs["jobs"]["pages-policy"]["container"] = json!("hostile:latest");
            }
            "build-default-shell" => {
                docs["jobs"]["build"]["defaults"] =
                    json!({"run":{"shell":"bash scripts/ignore-failures.sh {0}"}});
            }
            "build-site-replacement" => docs["jobs"]["build"]["steps"]
                .as_array_mut()
                .unwrap()
                .push(json!({"run":"rm -rf site && cp -R hostile-site site"})),
            _ => {
                docs["jobs"]["hostile"] = json!({
                    "runs-on":"ubuntu-latest",
                    "permissions":{"pages":"write","id-token":"write"},
                    "steps":[{"run":"deploy-with-custom-client"}]
                });
            }
        }
        assert!(!check_pages_policy(&docs, &contexts()).is_empty(), "{case}");
    }
}
#[test]
fn reviewed_context_digests_are_exactly_equal() {
    let contracts = contexts();
    for file in ["ci", "docs", "packages", "pgrx-test"] {
        let w = workflow(file);
        assert_eq!(
            &execution_context(&w).unwrap(),
            &contracts[w["name"].as_str().unwrap()],
            "{file}"
        );
        if file != "docs" {
            assert!(check_shell_policy(&w, &contracts).is_empty());
        }
    }
}
#[test]
fn reviewed_context_binds_top_level_triggers_and_permissions() {
    let contracts = contexts();
    for file in ["ci", "packages"] {
        let original = workflow(file);
        for (field, replacement) in [
            ("permissions", json!("write-all")),
            ("on", json!({"workflow_dispatch":{}})),
            (
                "concurrency",
                json!({"group":"packages","cancel-in-progress":true}),
            ),
        ] {
            let mut mutated = original.clone();
            mutated[field] = replacement;
            assert_ne!(
                execution_context(&mutated).unwrap(),
                contracts[original["name"].as_str().unwrap()],
                "{file} {field}"
            );
        }
    }
}

#[test]
fn package_write_authority_is_dispatch_only_and_checkouts_drop_credentials() {
    let packages = workflow("packages");
    let jobs = packages["jobs"].as_object().unwrap();
    for (name, job) in jobs {
        if job["permissions"]["packages"] == "write" {
            assert_eq!(
                job["if"], "needs.prep.outputs.publish == 'true'",
                "{name} receives package-write authority outside authenticated publication"
            );
            let steps = job["steps"].as_array().unwrap();
            let all_uses: Vec<_> = steps
                .iter()
                .filter_map(|step| step["uses"].as_str())
                .collect();
            assert!(
                !all_uses.iter().any(|uses| uses.contains("checkout@")),
                "{name} checks out repository-controlled code under package-write authority"
            );
            let all_runs = steps
                .iter()
                .filter_map(|step| step["run"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !all_runs.contains("cargo run"),
                "{name} executes repository-controlled Rust under package-write authority"
            );
            if matches!(name.as_str(), "docker-publish" | "companions-publish") {
                let uses = all_uses;
                assert!(uses.iter().any(|uses| uses.contains("download-artifact@")));
                assert!(!uses.iter().any(|uses| {
                    uses.contains("checkout@") || uses.contains("build-push-action@")
                }));
                let runs = steps
                    .iter()
                    .filter_map(|step| step["run"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(runs.contains("docker load --input"));
                assert!(runs.contains("docker push"));
            }
        }
        for checkout in job["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|step| {
                step["uses"]
                    .as_str()
                    .is_some_and(|uses| uses.starts_with("actions/checkout@"))
            })
        {
            assert_eq!(
                checkout["with"]["persist-credentials"], false,
                "{name} checkout persists credentials"
            );
        }
    }
}

#[test]
fn required_support_contract_passes() {
    assert_eq!(
        check_support(&workflow("packages"), &workflow("pgrx-test"), &contexts()),
        Vec::<String>::new()
    );
}
#[test]
fn boundary_mutations() {
    let packages = workflow("packages");
    let pgrx = workflow("pgrx-test");
    let c = contexts();
    for job in ["deb", "docker", "docker-manifest"] {
        let mut bad = packages.clone();
        bad["jobs"][job]["strategy"]["matrix"]["pg"]
            .as_array_mut()
            .unwrap()
            .push(json!(19));
        assert!(
            check_support(&bad, &pgrx, &c)
                .iter()
                .any(|e| e.starts_with("stable GA boundary"))
        );
        let mut bad = packages.clone();
        bad["jobs"][job]["strategy"]["matrix"]["include"] =
            json!([{"pg":19,"arch":"amd64","runner":"ubuntu-24.04"}]);
        assert!(
            check_support(&bad, &pgrx, &c)
                .iter()
                .any(|e| e.starts_with("stable GA boundary"))
        );
    }
    for include in [json!([]), json!([{"pg":19,"image_tag":"19"}]), Value::Null] {
        let mut bad = pgrx.clone();
        bad["jobs"]["test"]["strategy"]["matrix"]["include"] = include;
        let errors = check_support(&packages, &bad, &c);
        assert!(
            errors
                .iter()
                .any(|e| e == "required pinned beta compatibility")
        );
        assert!(
            errors
                .iter()
                .any(|e| e == "effective compatibility coverage changed")
        );
    }
}
#[test]
fn yaml_exclusion_variants_change_required_coverage() {
    let packages = workflow("packages");
    let pgrx = workflow("pgrx-test");
    let c = contexts();
    for source in [
        "[{pg: 18}]",
        "- pg: 18",
        "[{pg: 15}, {pg: 16}, {pg: 17}, {pg: 18}]",
        "[&leg {pg: 18}, *leg]",
    ] {
        let mut bad = pgrx.clone();
        let exclusion: serde_yaml::Value = serde_yaml::from_str(source).unwrap();
        bad["jobs"]["test"]["strategy"]["matrix"]["exclude"] =
            serde_json::to_value(exclusion).unwrap();
        assert!(
            check_support(&packages, &bad, &c)
                .iter()
                .any(|e| e == "effective compatibility coverage changed")
        );
    }
}
#[test]
fn publication_dependency_mutations() {
    let packages = workflow("packages");
    let pgrx = workflow("pgrx-test");
    let c = contexts();
    for job in [
        "docker",
        "companions",
        "docker-manifest",
        "companions-manifest",
    ] {
        let needs = packages["jobs"][job]["needs"].as_array().unwrap();
        for dependency in needs {
            let mut bad = packages.clone();
            bad["jobs"][job]["needs"]
                .as_array_mut()
                .unwrap()
                .retain(|value| value != dependency);
            assert!(!check_support(&bad, &pgrx, &c).is_empty());
        }
    }
    for (field, value) in [("if", json!("false")), ("continue-on-error", json!(true))] {
        let mut bad = pgrx.clone();
        let step = bad["jobs"]["test"]["steps"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|step| step["name"] == "Test")
            .unwrap();
        step[field] = value;
        assert!(!check_support(&packages, &bad, &c).is_empty());
    }
}

#[test]
fn workflow_release_topology_and_required_commands() {
    let packages = workflow("packages");
    let ci = workflow("ci");
    let pgrx = workflow("pgrx-test");
    assert_eq!(check_release(&packages, &ci, &pgrx), Vec::<String>::new());
    for value in [json!("v0.4.0"), json!(false), Value::Null] {
        let mut bad = packages.clone();
        bad["on"]["workflow_dispatch"]["inputs"]["release_tag"]["default"] = value;
        assert!(
            check_release(&bad, &ci, &pgrx)
                .iter()
                .any(|error| error == "immutable release tag input required")
        );
    }
    for mutator in [
        "missing-history",
        "moved-cwd",
        "wrong-source",
        "wrong-version",
        "prep-bypass",
        "new-artifact",
    ] {
        let mut bad = packages.clone();
        match mutator {
            "missing-history" => bad["jobs"]["prep"]["steps"][1]["with"]["fetch-depth"] = json!(1),
            "moved-cwd" => {
                bad["jobs"]["prep"]["steps"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|s| s["id"] == "resolve")
                    .unwrap()["working-directory"] = json!("release-tools");
            }
            "wrong-source" => {
                bad["jobs"]["deb"]["steps"][0]["with"]["ref"] = json!("${{ github.sha }}");
            }
            "wrong-version" => bad["jobs"]["docker"]["env"]["VERSION"] = json!("0.2.0"),
            "prep-bypass" => bad["jobs"]["docker-publish"]["if"] = json!("always()"),
            _ => {
                bad["jobs"]["hostile"] =
                    json!({"steps":[{"uses":"actions/upload-artifact@deadbeef"}]});
            }
        }
        assert!(!check_release(&bad, &ci, &pgrx).is_empty(), "{mutator}");
    }
    for field in ["if", "continue-on-error", "run"] {
        let mut bad = ci.clone();
        let step = bad["jobs"]["homebrew-policy"]["steps"]
            .as_array_mut()
            .unwrap()
            .last_mut()
            .unwrap();
        step[field] = if field == "run" {
            json!("true")
        } else {
            json!(true)
        };
        assert!(!check_release(&packages, &bad, &pgrx).is_empty());
    }
    for job in [
        "deb",
        "docker",
        "docker-publish",
        "companions",
        "companions-publish",
        "docker-manifest",
        "companions-manifest",
    ] {
        let mut bad = packages.clone();
        bad["jobs"][job]["needs"]
            .as_array_mut()
            .unwrap()
            .retain(|dependency| dependency != "notices");
        assert!(
            check_release(&bad, &ci, &pgrx)
                .iter()
                .any(|error| error == &format!("artifact bypasses notices: {job}")),
            "{job}"
        );
    }
}

#[test]
fn release_graph_rejects_cycles_and_unknown_dependencies() {
    let packages = workflow("packages");
    let ci = workflow("ci");
    let pgrx = workflow("pgrx-test");
    for dependency in [json!("docker"), json!("nonexistent"), json!(17)] {
        let mut bad = packages.clone();
        bad["jobs"]["docker"]["needs"]
            .as_array_mut()
            .unwrap()
            .push(dependency);
        assert!(!check_release(&bad, &ci, &pgrx).is_empty());
    }
}

#[test]
fn support_predicates_fail_independently_of_context_hashes() {
    let packages = workflow("packages");
    let pgrx = workflow("pgrx-test");
    let c = contexts();
    for (case, diagnostic) in [
        ("beta-disabled", "compatibility must fail closed"),
        ("compat-disabled", "compatibility source/dependency bypass"),
        ("compat-uses", "publication requires compatibility"),
        ("build-needs", "publication gate: docker"),
        (
            "manifest-needs",
            "all builds before release tags: docker-manifest",
        ),
    ] {
        let mut p = packages.clone();
        let mut x = pgrx.clone();
        match case {
            "beta-disabled" => x["jobs"]["test"]["continue-on-error"] = json!(true),
            "compat-disabled" => p["jobs"]["compatibility"]["if"] = json!("false"),
            "compat-uses" => p["jobs"]["compatibility"]["uses"] = json!("wrong.yml"),
            "build-needs" => p["jobs"]["docker"]["needs"] = json!(["prep"]),
            _ => p["jobs"]["docker-manifest"]["needs"] = json!(["docker"]),
        }
        assert!(
            check_support(&p, &x, &c).iter().any(|e| e == diagnostic),
            "{case}"
        );
    }
}

fn step<'a>(w: &'a mut Value, job: &str, key: &str, value: &str) -> &'a mut Value {
    w["jobs"][job]["steps"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|s| s[key] == value)
        .unwrap()
}

#[test]
#[allow(clippy::too_many_lines)] // One table-driven registry covers independent release boundaries.
fn release_predicates_report_each_independent_boundary() {
    for (case, diagnostic) in [
        ("input", "immutable release tag input required"),
        ("prep-bypass", "publication bypasses prep: docker-publish"),
        ("tooling", "current tooling checkout changed"),
        ("source", "deb: proven source checkout required"),
        ("version", "docker: proven package version required"),
        ("inventory", "artifact job inventory changed"),
        ("resolver", "required resolver execution changed"),
        (
            "history",
            "prep checkout must fetch-depth: 0 to prove tag identity",
        ),
        ("resolver-duplicate", "unique resolver required"),
        ("resolver-missing", "resolver steps missing"),
        (
            "pgrx-checkout",
            "compatibility source/tooling checkout contract changed",
        ),
        ("guard-disabled", "portable guards disabled"),
        (
            "guard-command",
            "required commands are not active direct execution",
        ),
        ("brew-disabled", "required macOS job disabled"),
        ("brew-command", "Homebrew API execution contract changed"),
        ("notice-job", "authoritative package notice job changed"),
        (
            "notice-source-checkout",
            "authoritative package notice job changed",
        ),
        ("ci-notice", "authoritative CI notice verification changed"),
        (
            "ci-notice-checkout",
            "authoritative CI notice verification changed",
        ),
        (
            "cycle",
            "release dependency graph is cyclic, malformed or incomplete",
        ),
    ] {
        let mut p = workflow("packages");
        let mut c = workflow("ci");
        let mut x = workflow("pgrx-test");
        match case {
            "input" => {
                p["on"]["workflow_dispatch"]["inputs"]
                    .as_object_mut()
                    .unwrap()
                    .remove("release_tag");
            }
            "prep-bypass" => p["jobs"]["docker-publish"]["if"] = json!("always()"),
            "tooling" => {
                step(
                    &mut p,
                    "meta",
                    "name",
                    "Checkout release validation tooling",
                )["with"]["ref"] = json!("old");
            }
            "source" => p["jobs"]["deb"]["steps"][0]["with"]["ref"] = json!("old"),
            "version" => p["jobs"]["docker"]["env"]["VERSION"] = json!("0.2.0"),
            "inventory" => {
                p["jobs"].as_object_mut().unwrap().remove("deb");
            }
            "resolver" => step(&mut p, "prep", "id", "resolve")["run"] = json!("true"),
            "history" => {
                step(&mut p, "prep", "id", "resolve")["working-directory"] = json!("release-tools");
            }
            "resolver-duplicate" => p["jobs"]["prep"]["steps"]
                .as_array_mut()
                .unwrap()
                .push(json!({"id":"resolve"})),
            "resolver-missing" => p["jobs"]["prep"]["steps"] = Value::Null,
            "pgrx-checkout" => x["jobs"]["test"]["steps"][0]["with"]["ref"] = json!("old"),
            "guard-disabled" => c["jobs"]["rust"]["if"] = json!("false"),
            "guard-command" => {
                step(
                    &mut c,
                    "rust",
                    "name",
                    "Test required PostgreSQL coverage guards",
                )["run"] = json!("true");
            }
            "brew-disabled" => c["jobs"]["homebrew-policy"]["runs-on"] = json!("ubuntu-24.04"),
            "brew-command" => {
                step(
                    &mut c,
                    "homebrew-policy",
                    "name",
                    "Execute actual Homebrew environment policy",
                )["run"] = json!("true");
            }
            "notice-job" => {
                step(
                    &mut p,
                    "notices",
                    "name",
                    "Verify deterministic third-party notices",
                )["run"] = json!("true");
            }
            "notice-source-checkout" => {
                p["jobs"]["notices"]["steps"][0]["with"]["ref"] = json!("${{ github.sha }}");
            }
            "ci-notice" => {
                step(
                    &mut c,
                    "notices",
                    "name",
                    "Verify deterministic third-party notices",
                )["run"] = json!("true");
            }
            "ci-notice-checkout" => {
                c["jobs"]["notices"]["steps"][0]["with"] = json!({"ref":"refs/heads/main"});
            }
            _ => p["jobs"]["docker"]["needs"]
                .as_array_mut()
                .unwrap()
                .push(json!("docker")),
        }
        assert!(
            check_release(&p, &c, &x).iter().any(|e| e == diagnostic),
            "{case}: {:?}",
            check_release(&p, &c, &x)
        );
    }
}

#[test]
fn history_consumer_is_unique_and_owned_by_full_checkout() {
    let requirement = [HistoryRequirement {
        job: "rust".into(),
        step_name: "Test required PostgreSQL coverage guards".into(),
        consumer_path: ".".into(),
    }];
    for (case, diagnostic) in [
        ("missing-job", "history consumer job missing"),
        ("duplicate", "unique history consumer step required"),
        ("shallow", "needs release history from its owning checkout"),
    ] {
        let mut c = workflow("ci");
        match case {
            "missing-job" => c["jobs"]["rust"]["steps"] = Value::Null,
            "duplicate" => c["jobs"]["rust"]["steps"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name":"Test required PostgreSQL coverage guards"})),
            _ => c["jobs"]["rust"]["steps"][0]["with"]["fetch-depth"] = json!(1),
        }
        assert!(
            check_history_policy(&c, &requirement)
                .iter()
                .any(|e| e.contains(diagnostic)),
            "{case}"
        );
    }
}

#[test]
fn shell_diagnostics_are_independent_of_context_hashes() {
    for (case, diagnostic) in [
        ("startup", "startup environment injection"),
        ("path-env", "PATH environment override"),
        ("github-path", "GITHUB_PATH manipulation"),
        ("path-run", "PATH command override"),
        ("startup-run", "startup/environment-file manipulation"),
        ("job-shell", "job shell override"),
        ("step-shell", "step shell override"),
    ] {
        let mut w = workflow("ci");
        match case {
            "startup" => w["env"]["BASH_ENV"] = json!("hook"),
            "path-env" => w["env"]["PATH"] = json!("tools"),
            "github-path" => w["jobs"]["rust"]["steps"]
                .as_array_mut()
                .unwrap()
                .push(json!({"run":"echo tools >> $GITHUB_PATH"})),
            "path-run" => w["jobs"]["rust"]["steps"]
                .as_array_mut()
                .unwrap()
                .push(json!({"run":"PATH=tools cargo test"})),
            "startup-run" => w["jobs"]["rust"]["steps"]
                .as_array_mut()
                .unwrap()
                .push(json!({"run":"export BASH_ENV=hook"})),
            "job-shell" => w["jobs"]["rust"]["defaults"] = json!({"run":{"shell":"bash"}}),
            _ => w["jobs"]["rust"]["steps"][0]["shell"] = json!("bash"),
        }
        assert!(
            check_shell_policy(&w, &contexts())
                .iter()
                .any(|e| e == diagnostic),
            "{case}"
        );
    }
}
