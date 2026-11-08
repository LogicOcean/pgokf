//! Required workflow attack tests and actual Homebrew formula substitutions.
use crate::{owned_docker::publish, process::CommandSpec};
use serde_json::json;
use std::{fs, path::Path, time::Duration};
fn execute(mut command: CommandSpec, log: &Path) -> Result<(bool, String), String> {
    command.timeout = Duration::from_mins(5);
    let output = command.run()?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    publish(
        log,
        &json!({"success":output.status.success(),"output":text}),
    )?;
    Ok((output.status.success(), text))
}
/// Execute the portable workflow controls and live formula policy variants.
/// # Errors
/// Missing Homebrew, baseline failures and failures outside intended assertions fail.
#[allow(clippy::too_many_lines)] // Keep baseline, three live substitutions and cleanup in one audit sequence.
pub fn run(root: &Path, output: &Path) -> Result<(), String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let parent = output
        .parent()
        .ok_or("evidence parent required")?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if parent.starts_with(&root) {
        return Err("policy evidence must be outside source".into());
    }
    fs::create_dir(output).map_err(|e| e.to_string())?;

    let mut gate = CommandSpec::new("cargo");
    gate.cwd = Some(root.clone());
    gate.env.insert(
        "CARGO_BUILD_JOBS".into(),
        if std::env::var("CARGO_BUILD_JOBS").as_deref() == Ok("1") {
            "1".into()
        } else {
            "2".into()
        },
    );
    gate.env.insert("CARGO_INCREMENTAL".into(), "0".into());
    gate.args = [
        "test",
        "--locked",
        "-p",
        "pgokf-verifier",
        "--test",
        "policy_mutations",
        "--",
        "--nocapture",
    ]
    .map(Into::into)
    .into();
    let (ok, text) = execute(gate, &output.join("workflow-controls.json"))?;
    if !ok || !text.contains("6 passed; 0 failed") {
        return Err("workflow mutation controls failed or missing".into());
    }
    let binary = std::env::current_exe().map_err(|e| e.to_string())?;
    let formula = root.join("packaging/homebrew/pgokf.rb");
    let original = fs::read_to_string(&formula).map_err(|e| e.to_string())?;
    let invoke = |path: &Path| {
        let mut command = CommandSpec::new(&binary);
        command.cwd = Some(root.clone());
        command.args = vec![
            "homebrew-policy".into(),
            root.clone().into_os_string(),
            path.as_os_str().into(),
        ];
        command
    };
    let (ok, text) = execute(invoke(&formula), &output.join("formula-baseline.json"))?;
    if !ok || !text.contains("PASS: four actual formula environment composition cases") {
        return Err("actual Homebrew formula baseline failed".into());
    }
    let temporary = tempfile::Builder::new()
        .prefix("pgokf-policy-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    publish(
        &output.join("ownership.json"),
        &json!({"owned_directory":temporary.path()}),
    )?;
    let mut results = Vec::new();
    for (name, before, after, reason) in [
        (
            "commented-deployment",
            "      ENV[\"MACOSX_DEPLOYMENT_TARGET\"]",
            "      # ENV[\"MACOSX_DEPLOYMENT_TARGET\"]",
            "host deployment target lost",
        ),
        (
            "commented-flags",
            "        ENV.append",
            "        # ENV.append",
            "plain flags lost",
        ),
        (
            "dead-formula-branch",
            "if OS.mac?",
            "if false",
            "host deployment target lost",
        ),
    ] {
        if !original.contains(before) {
            return Err(format!("ineffective formula mutation: {name}"));
        }
        let path = temporary.path().join(format!("{name}.rb"));
        fs::write(&path, original.replace(before, after)).map_err(|e| e.to_string())?;
        let result = execute(invoke(&path), &output.join(format!("{name}.json")));
        let killed = matches!(result,Ok((false,ref text)) if text.contains(reason));
        results.push(json!({"name":name,"intended_assertion_reached":killed,"reason":reason}));
    }
    temporary.close().map_err(|e| e.to_string())?;
    publish(
        &output.join("cleanup.json"),
        &json!({"owned_directory_removed":true}),
    )?;
    publish(&output.join("results.json"), &results)?;
    if results
        .iter()
        .all(|r| r["intended_assertion_reached"] == true)
    {
        Ok(())
    } else {
        Err("formula policy mutation survived or failed outside intended assertion".into())
    }
}
