use pgokf_verifier::workflow::*;
use serde_json::{Value, json};
use std::path::Path;
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
#[test]
fn reviewed_context_digests_are_exactly_equal() {
    let contracts = contexts();
    for file in ["ci", "packages", "pgrx-test"] {
        let w = workflow(file);
        assert_eq!(
            &execution_context(&w).unwrap(),
            &contracts[w["name"].as_str().unwrap()],
            "{file}"
        );
        assert!(check_shell_policy(&w, &contracts).is_empty());
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
            "prep-bypass" => bad["jobs"]["docker"]["if"] = json!("always()"),
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
fn release_predicates_report_each_independent_boundary() {
    for (case, diagnostic) in [
        ("input", "immutable release tag input required"),
        ("prep-bypass", "publication bypasses prep: docker"),
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
            "prep-bypass" => p["jobs"]["docker"]["if"] = json!("always()"),
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
