//! Execution policy attack families exercised against reviewed repository workflows.
use pgokf_verifier::{
    json::from_slice,
    workflow::{self, Contexts},
};
use serde_json::{Value, json};
use std::path::PathBuf;
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}
fn fixture(file: &str) -> (Value, Contexts) {
    let workflow = workflow::parse(
        &std::fs::read_to_string(root().join(format!(".github/workflows/{file}.yml"))).unwrap(),
    )
    .unwrap();
    let contexts =
        from_slice(&std::fs::read(root().join("tests/workflow-contexts.json")).unwrap()).unwrap();
    assert!(workflow::check_shell_policy(&workflow, &contexts).is_empty());
    (workflow, contexts)
}
fn rejects(before: &Value, after: &Value, contexts: &Contexts, name: &str) {
    assert_ne!(before, after, "ineffective mutation: {name}");
    let errors = workflow::check_shell_policy(after, contexts);
    assert!(
        errors
            .iter()
            .any(|error| error == "unreviewed shell-selection execution context")
            || errors
                .iter()
                .any(|error| error == "explicit clean strict shell required"),
        "survived: {name}: {errors:?}"
    );
}
#[test]
fn workflow_job_step_environment_shell_and_action_attacks() {
    for (file, job) in [
        ("pgrx-test", "test"),
        ("ci", "homebrew-policy"),
        ("packages", "homebrew-policy"),
    ] {
        let (base, contexts) = fixture(file);
        for pointer in [
            String::new(),
            format!("/jobs/{job}"),
            format!("/jobs/{job}/steps/0"),
        ] {
            for (key, value) in [
                ("PATH", "/tmp/mask"),
                ("BASH_ENV", "/tmp/startup"),
                ("ENV", "/tmp/startup"),
                ("BASH_FUNC_gate%%", "() { true; }"),
            ] {
                let mut bad = base.clone();
                bad.pointer_mut(&pointer).unwrap()["env"] = json!({key:value});
                rejects(&base, &bad, &contexts, &format!("{file}:{pointer}:{key}"));
            }
        }
        for shell in ["bash {0}", "bash", "/tmp/bash", "/tmp/system-bash-link"] {
            let mut bad = base.clone();
            bad["defaults"]["run"]["shell"] = json!(shell);
            rejects(&base, &bad, &contexts, shell);
        }
        for scope in ["job", "step"] {
            let mut bad = base.clone();
            if scope == "job" {
                bad["jobs"][job]["defaults"] = json!({"run":{"shell":"bash {0}"}});
            } else {
                bad["jobs"][job]["steps"][0]["shell"] = json!("bash {0}");
            }
            rejects(&base, &bad, &contexts, scope);
        }
        let mut bad = base.clone();
        bad["defaults"]["run"]
            .as_object_mut()
            .unwrap()
            .remove("shell");
        rejects(&base, &bad, &contexts, "removed-shell");
    }
}
#[test]
fn disabled_masked_or_commented_commands_and_release_graph_fail() {
    for (file, job) in [("packages", "homebrew-policy"), ("pgrx-test", "test")] {
        let (base, contexts) = fixture(file);
        let index = base["jobs"][job]["steps"]
            .as_array()
            .unwrap()
            .iter()
            .position(|s| s.get("run").is_some())
            .unwrap();
        let command = base["jobs"][job]["steps"][index]["run"].as_str().unwrap();
        for run in [
            format!("# {command}"),
            format!("if false; then {command}; fi"),
            format!("{command} || true"),
            "true".into(),
        ] {
            let mut bad = base.clone();
            bad["jobs"][job]["steps"][index]["run"] = json!(run);
            rejects(&base, &bad, &contexts, &run);
        }
        for (field, value) in [
            ("if", json!(false)),
            ("if", json!("always()")),
            ("continue-on-error", json!(true)),
        ] {
            for step in [false, true] {
                let mut bad = base.clone();
                let scope = if step {
                    &mut bad["jobs"][job]["steps"][index]
                } else {
                    &mut bad["jobs"][job]
                };
                scope[field] = value.clone();
                rejects(&base, &bad, &contexts, field);
            }
        }
    }
    let (base, contexts) = fixture("packages");
    for job in [
        "docker",
        "companions",
        "docker-manifest",
        "companions-manifest",
    ] {
        for (field, value) in [
            ("needs", json!([])),
            ("if", json!("always()")),
            ("continue-on-error", json!(true)),
        ] {
            let mut bad = base.clone();
            bad["jobs"][job][field] = value;
            rejects(&base, &bad, &contexts, &format!("{job}:{field}"));
        }
    }
}
#[test]
fn computed_transitive_wrappers_and_environment_files_fail() {
    let (base, contexts) = fixture("pgrx-test");
    for command in [
        "export PATH=/tmp/mask:$PATH",
        "echo /tmp/mask >> \"$GITHUB_PATH\"",
        "echo /tmp/mask >> \"${GITHUB_PATH}\"",
        "echo /tmp/mask >> '${{ github.path }}'",
        "echo /tmp/mask >> '${{ env.GITHUB_PATH }}'",
        "echo /tmp/mask | tee -a \"$GITHUB_PATH\"",
        "key=GITHUB_; key+=PATH; echo /tmp/mask >> \"${!key}\"",
        "echo /tmp/mask >> \"$(printenv GITHUB_\"PATH\")\"",
        "echo /tmp/mask >> \"${{ steps.setup.outputs.file }}\"",
        "echo PATH=/tmp/mask >> \"${GITHUB_ENV}\"",
        "echo PATH=/tmp/mask >> '${{ github.env }}'",
        "echo \"::add-path::/tmp/mask\"",
        "printf '#!/bin/sh\\nexit 0\\n' > /tmp/bash; chmod +x /tmp/bash",
        "ln -s /tmp/mask /tmp/bash",
        "p=py; q=thon; $p$q -c pass",
    ] {
        let mut bad = base.clone();
        bad["jobs"]["test"]["steps"]
            .as_array_mut()
            .unwrap()
            .insert(0, json!({"run":command}));
        rejects(&base, &bad, &contexts, command);
    }
    for step in [
        json!({"uses":"./mask-shell"}),
        json!({"uses":"actions/github-script@60a0d83039c74a4aee543508d2ffcb1c3799cdea","with":{"script":"core.addPath('/tmp/mask')"}}),
        json!({"uses":"toolchain/action@pin","with":{"toolchain":"${{ steps.setup.outputs.toolchain }}"}}),
        json!({"run":"echo /tmp/mask >> $FILE","env":{"FILE":"${{ github.path }}"}}),
    ] {
        let mut bad = base.clone();
        bad["jobs"]["test"]["steps"]
            .as_array_mut()
            .unwrap()
            .insert(0, step);
        rejects(&base, &bad, &contexts, "action or environment indirection");
    }
}
#[test]
#[allow(clippy::format_collect)]
fn aliases_merges_startup_and_matrix_exclusions_fail() {
    let (base, contexts) = fixture("pgrx-test");
    for fragment in [
        "- run: 'true'\n  env: &mask {PATH: /tmp/mask}\n- run: 'true'\n  env: *mask",
        "- run: 'true'\n  env: &mask {PATH: /tmp/mask}\n- run: 'true'\n  env: {<<: *mask}",
        "- run: &write echo /tmp/mask >> $GITHUB_PATH\n- run: *write",
    ] {
        let steps = workflow::parse(&format!(
            "steps:\n{}",
            fragment
                .lines()
                .map(|l| format!("  {l}\n"))
                .collect::<String>()
        ))
        .unwrap();
        let mut bad = base.clone();
        bad["jobs"]["test"]["steps"]
            .as_array_mut()
            .unwrap()
            .extend(steps["steps"].as_array().unwrap().clone());
        rejects(&base, &bad, &contexts, "alias");
    }
    for fragment in [
        "[{pg: 18}]",
        "\n- pg: 18",
        "[{pg: 15}, {pg: 16}, {pg: 17}, {pg: 18}]",
        "[&required {pg: 18}, *required]",
    ] {
        let yaml: serde_yaml::Value = serde_yaml::from_str(fragment).unwrap();
        let mut bad = base.clone();
        bad["jobs"]["test"]["strategy"]["matrix"]["exclude"] = serde_json::to_value(yaml).unwrap();
        rejects(&base, &bad, &contexts, "matrix exclusion");
    }
}
#[test]
fn every_checkout_history_or_activation_change_fails() {
    for file in ["ci", "packages", "pgrx-test"] {
        let (base, contexts) = fixture(file);
        for (job, value) in base["jobs"].as_object().unwrap() {
            let Some(steps) = value["steps"].as_array() else {
                continue;
            };
            for (index, step) in steps.iter().enumerate().filter(|(_, s)| {
                s["uses"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("actions/checkout@"))
            }) {
                for (field, value) in [("if", json!(false)), ("continue-on-error", json!(true))] {
                    let mut bad = base.clone();
                    bad["jobs"][job]["steps"][index][field] = value;
                    rejects(&base, &bad, &contexts, "disabled checkout");
                }
                if step["with"]["fetch-depth"].as_u64() == Some(0) {
                    let mut bad = base.clone();
                    bad["jobs"][job]["steps"][index]["with"]
                        .as_object_mut()
                        .unwrap()
                        .remove("fetch-depth");
                    rejects(&base, &bad, &contexts, "shallow history");
                }
            }
        }
    }
}
#[path = "support/policy_cases.rs"]
mod policy_cases;
#[test]
fn complete_policy_case_registry() {
    let mut names = std::collections::BTreeSet::new();
    for name in policy_cases::NAMES {
        assert!(names.insert(name), "duplicate policy mutation {name}");
        let (baseline, contexts) = fixture(policy_cases::file(name));
        let mut changed = baseline.clone();
        policy_cases::apply(&mut changed, name);
        rejects(&baseline, &changed, &contexts, name);
        println!("policy mutation rejected: {name}");
    }
    assert_eq!(names.len(), 73);
}
