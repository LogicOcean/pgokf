use pgokf_verifier::workflow;
use serde_json::{Value, json};
pub const NAMES: [&str; 73] = [
    "disabled-release-step",
    "disabled-macos-job",
    "disabled-macos-step",
    "commented-execution",
    "dead-execution",
    "success-mask",
    "removed-execution",
    "bypassed-dependency",
    "bypassed-condition",
    "manifest-bypass",
    "masked-shell-default",
    "disabled-tooling-checkout",
    "masked-beta-build",
    "waived-failure",
    "commented-pgrx",
    "exclude-pg18-flow",
    "exclude-pg18-block",
    "exclude-multiple",
    "exclude-alias",
    "startup-trap-workflow",
    "startup-trap-job",
    "startup-trap-step",
    "github-env-startup",
    "step-shell-override",
    "workflow-shell-override",
    "test-success-mask",
    "exact-wrapper",
    "symlink-wrapper",
    "pgrx-test-path-workflow",
    "pgrx-test-path-job",
    "pgrx-test-path-step",
    "pgrx-test-relative-shell",
    "pgrx-test-alternate-shell",
    "pgrx-test-symlink-shell",
    "pgrx-test-removed-shell",
    "ci-path-workflow",
    "ci-path-job",
    "ci-path-step",
    "ci-relative-shell",
    "ci-alternate-shell",
    "ci-symlink-shell",
    "ci-removed-shell",
    "packages-path-workflow",
    "packages-path-job",
    "packages-path-step",
    "packages-relative-shell",
    "packages-alternate-shell",
    "packages-symlink-shell",
    "packages-removed-shell",
    "path-command",
    "github-path-plain",
    "github-path-braced",
    "github-path-expression",
    "github-path-env-expression",
    "github-path-tee",
    "github-path-python",
    "github-path-computed",
    "github-path-split",
    "github-path-output",
    "github-env-braced",
    "github-env-expression",
    "legacy-add-path",
    "path-env-alias",
    "path-env-merge",
    "github-path-run-alias",
    "github-path-env-alias",
    "action-path-indirection",
    "local-action-indirection",
    "action-input-indirection",
    "shallow-guard-history",
    "shallow-release-guard-history",
    "shallow-resolver-history",
    "shell-anchor-replacement",
];
pub fn file(name: &str) -> &'static str {
    if name.starts_with("ci-") || name == "shallow-guard-history" {
        "ci"
    } else if name.starts_with("packages-")
        || matches!(
            name,
            "disabled-release-step"
                | "disabled-macos-job"
                | "disabled-macos-step"
                | "commented-execution"
                | "dead-execution"
                | "success-mask"
                | "removed-execution"
                | "bypassed-dependency"
                | "bypassed-condition"
                | "manifest-bypass"
                | "waived-failure"
                | "shallow-release-guard-history"
                | "shallow-resolver-history"
        )
    {
        "packages"
    } else {
        "pgrx-test"
    }
}
fn step<'a>(w: &'a mut Value, job: &str, name: &str) -> &'a mut Value {
    w["jobs"][job]["steps"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("missing required step {job}/{name}"))
}
fn prepend(w: &mut Value, value: Value) {
    w["jobs"]["test"]["steps"]
        .as_array_mut()
        .unwrap()
        .insert(0, value);
}
fn prepend_yaml(w: &mut Value, source: &str) {
    let mut text = String::from("steps:\n");
    for line in source.lines() {
        text.push_str("  ");
        text.push_str(line);
        text.push('\n');
    }
    let parsed = workflow::parse(&text).unwrap();
    for step in parsed["steps"].as_array().unwrap().iter().rev() {
        prepend(w, step.clone());
    }
}
#[allow(clippy::too_many_lines)]
pub fn apply(w: &mut Value, name: &str) {
    let job = if file(name) == "pgrx-test" {
        "test"
    } else {
        "homebrew-policy"
    };
    let named = if job == "test" {
        "Clippy"
    } else {
        "Execute actual Homebrew environment policy"
    };
    if name.contains("-path-")
        && ["workflow", "job", "step"]
            .iter()
            .any(|s| name.ends_with(s))
    {
        let scope = if name.ends_with("workflow") {
            w
        } else if name.ends_with("job") {
            &mut w["jobs"][job]
        } else {
            step(w, job, named)
        };
        scope["env"]["PATH"] = json!("/tmp/mask");
        return;
    }
    for (suffix, shell) in [
        ("relative-shell", "bash"),
        ("alternate-shell", "/tmp/bash"),
        ("symlink-shell", "/tmp/system-bash-link"),
    ] {
        if name.ends_with(suffix) {
            w["defaults"]["run"]["shell"] =
                json!(workflow::CLEAN_SHELL.replace("/bin/bash", shell));
            return;
        }
    }
    if name.ends_with("removed-shell") {
        w["defaults"]["run"]
            .as_object_mut()
            .unwrap()
            .remove("shell");
        return;
    }
    if name.starts_with("startup-trap-") {
        let scope = if name.ends_with("workflow") {
            &mut *w
        } else if name.ends_with("job") {
            &mut w["jobs"]["test"]
        } else {
            step(w, "test", "Clippy")
        };
        scope["env"]["BASH_ENV"] = json!("/tmp/pgokf-mask.sh");
        prepend(
            w,
            json!({"run":"echo \"trap 'exit 0' EXIT\" > /tmp/pgokf-mask.sh"}),
        );
        return;
    }
    if name.starts_with("exclude-") {
        let source = match name {
            "exclude-pg18-flow" => "[{pg: 18}]",
            "exclude-pg18-block" => "- pg: 18",
            "exclude-multiple" => "[{pg: 15}, {pg: 16}, {pg: 17}, {pg: 18}]",
            "exclude-alias" => "[&required {pg: 18}, *required]",
            _ => panic!("unknown exclusion"),
        };
        let yaml: serde_yaml::Value = serde_yaml::from_str(source).unwrap();
        w["jobs"]["test"]["strategy"]["matrix"]["exclude"] = serde_json::to_value(yaml).unwrap();
        return;
    }
    let command = match name {
        "path-command" => Some("export PATH=/tmp/mask:$PATH"),
        "github-path-plain" => Some("echo /tmp/mask >> \"$GITHUB_PATH\""),
        "github-path-braced" => Some("echo /tmp/mask >> \"${GITHUB_PATH}\""),
        "github-path-expression" => Some("echo /tmp/mask >> '${{ github.path }}'"),
        "github-path-env-expression" => Some("echo /tmp/mask >> '${{ env.GITHUB_PATH }}'"),
        "github-path-tee" => Some("echo /tmp/mask | tee -a \"$GITHUB_PATH\""),
        "github-path-python" => Some(
            "python3 -c 'import os; open(os.environ[\"GITHUB_PATH\"], \"a\").write(\"/tmp/mask\\n\")'",
        ),
        "github-path-computed" => Some("key=GITHUB_; key+=PATH; echo /tmp/mask >> \"${!key}\""),
        "github-path-split" => Some("echo /tmp/mask >> \"$(printenv GITHUB_\"PATH\")\""),
        "github-path-output" => Some("echo /tmp/mask >> \"${{ steps.setup.outputs.file }}\""),
        "github-env-braced" => Some("echo PATH=/tmp/mask >> \"${GITHUB_ENV}\""),
        "github-env-expression" => Some("echo PATH=/tmp/mask >> '${{ github.env }}'"),
        "legacy-add-path" => Some("echo \"::add-path::/tmp/mask\""),
        "github-env-startup" => Some(
            "echo \"trap 'exit 0' EXIT\" > /tmp/pgokf-mask.sh\necho BASH_ENV=/tmp/pgokf-mask.sh >> \"$GITHUB_ENV\"",
        ),
        "exact-wrapper" => Some(
            "mkdir -p \"$RUNNER_TEMP/review-shell\"\nprintf '#!/bin/sh\\n/bin/bash \"$@\"\\nexit 0\\n' > \"$RUNNER_TEMP/review-shell/bash\"\nchmod +x \"$RUNNER_TEMP/review-shell/bash\"\necho \"$RUNNER_TEMP/review-shell\" >> \"$GITHUB_PATH\"",
        ),
        "symlink-wrapper" => Some(
            "mkdir -p \"$RUNNER_TEMP/review-shell\"\nprintf '#!/bin/sh\\n/bin/bash \"$@\"\\nexit 0\\n' > \"$RUNNER_TEMP/mask\"\nln -s \"$RUNNER_TEMP/mask\" \"$RUNNER_TEMP/review-shell/bash\"\nchmod +x \"$RUNNER_TEMP/review-shell/bash\"\necho \"$RUNNER_TEMP/review-shell\" >> \"$GITHUB_PATH\"",
        ),
        _ => None,
    };
    if let Some(command) = command {
        prepend(w, json!({"run":command}));
        return;
    }
    match name {
        "disabled-release-step" | "bypassed-condition" => {
            step(w, "lint", "Portable source-packaging and cleanup guards")["if"] =
                if name == "disabled-release-step" {
                    json!(false)
                } else {
                    json!("always()")
                }
        }
        "disabled-macos-job" => w["jobs"]["homebrew-policy"]["if"] = json!(false),
        "disabled-macos-step" => {
            step(
                w,
                "homebrew-policy",
                "Execute actual Homebrew environment policy",
            )["if"] = json!(false);
        }
        "waived-failure" => w["jobs"]["homebrew-policy"]["continue-on-error"] = json!(true),
        "commented-execution"
        | "dead-execution"
        | "success-mask"
        | "removed-execution"
        | "masked-beta-build"
        | "commented-pgrx"
        | "test-success-mask" => {
            let (job, named) = match name {
                "masked-beta-build" => (
                    "test",
                    "Nonproduction PostgreSQL 19 Beta 3 image compatibility",
                ),
                "commented-pgrx" | "test-success-mask" => ("test", "Test"),
                _ => (
                    "homebrew-policy",
                    "Execute actual Homebrew environment policy",
                ),
            };
            let scope = step(w, job, named);
            let run = scope["run"].as_str().unwrap();
            scope["run"] = json!(match name {
                "commented-execution" | "commented-pgrx" => format!("# {run}"),
                "dead-execution" => format!("if false; then {run}; fi"),
                "removed-execution" => "true".into(),
                _ => format!("{run} || true"),
            });
        }
        "bypassed-dependency" => {
            for job in ["docker", "companions"] {
                w["jobs"][job]["needs"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|v| v != "homebrew-policy");
            }
        }
        "manifest-bypass" => {
            for job in ["docker-manifest", "companions-manifest"] {
                w["jobs"][job]["if"] = json!("always()");
            }
        }
        "masked-shell-default" => {
            w["jobs"]["test"]["defaults"] = json!({"run":{"shell":"bash {0}"}});
        }
        "disabled-tooling-checkout" => {
            step(w, "test", "Checkout release validation tooling")["if"] = json!(false);
        }
        "step-shell-override" => step(w, "test", "Clippy")["shell"] = json!("bash {0}"),
        "workflow-shell-override" => w["defaults"]["run"]["shell"] = json!("bash {0}"),
        "path-env-alias" => prepend_yaml(
            w,
            "- run: 'true'\n  env: &mask {PATH: /tmp/mask}\n- run: 'true'\n  env: *mask",
        ),
        "path-env-merge" => prepend_yaml(
            w,
            "- run: 'true'\n  env: &mask {PATH: /tmp/mask}\n- run: 'true'\n  env: {<<: *mask}",
        ),
        "github-path-run-alias" => prepend_yaml(
            w,
            "- run: &write echo /tmp/mask >> $GITHUB_PATH\n- run: *write",
        ),
        "github-path-env-alias" => prepend(
            w,
            json!({"run":"echo /tmp/mask >> $FILE","env":{"FILE":"${{ github.path }}"}}),
        ),
        "action-path-indirection" => prepend(
            w,
            json!({"uses":"actions/github-script@60a0d83039c74a4aee543508d2ffcb1c3799cdea","with":{"script":"core.addPath('/tmp/mask')"}}),
        ),
        "local-action-indirection" => prepend(w, json!({"uses":"./mask-shell"})),
        "action-input-indirection" => {
            let action = w["jobs"]["test"]["steps"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|s| s["with"].get("toolchain").is_some())
                .unwrap();
            action["with"]["toolchain"] = json!("${{ steps.setup.outputs.toolchain }}");
        }
        "shallow-guard-history" | "shallow-release-guard-history" | "shallow-resolver-history" => {
            let job = match name {
                "shallow-guard-history" => "rust",
                "shallow-release-guard-history" => "lint",
                _ => "prep",
            };
            let checkout = w["jobs"][job]["steps"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|s| s["with"]["fetch-depth"] == 0)
                .unwrap();
            checkout["with"]
                .as_object_mut()
                .unwrap()
                .remove("fetch-depth");
        }
        "shell-anchor-replacement" => {
            let parsed =
                workflow::parse("defaults: &shell {run: {shell: 'bash {0}'}}\njob: *shell")
                    .unwrap();
            w["defaults"] = parsed["defaults"].clone();
            w["jobs"]["test"]["defaults"] = parsed["job"].clone();
        }
        _ => panic!("unmapped policy case {name}"),
    }
}
