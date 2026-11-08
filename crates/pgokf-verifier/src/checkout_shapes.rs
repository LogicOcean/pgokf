//! Execute the reviewed Rust gates in the actual declared Git checkout shapes.
use crate::{
    process::CommandSpec,
    release_runtime::{command, output},
};
use serde::Serialize;
use serde_json::Value;
use std::{
    path::{Component, Path, PathBuf},
    time::Duration,
};
fn safe_path(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err("checkout path must stay within workspace".into());
    }
    Ok(path
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect())
}
fn checkout(root: &Path, head: &str, step: &Value, workspace: &Path) -> Result<(), String> {
    if step.get("if").is_some() || step["with"].get("repository").is_some() {
        return Err("conditional/external checkout unsupported".into());
    }
    let path = safe_path(step["with"]["path"].as_str().unwrap_or("."))?;
    let target = workspace.join(path);
    if target.exists() {
        std::fs::remove_dir_all(&target).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
    command(&target, "git", &["init", "--quiet"]).checked()?;
    let depth = match step["with"].get("fetch-depth") {
        None => 1,
        Some(Value::Number(n)) => n.as_u64().ok_or("invalid fetch depth")?,
        Some(Value::String(n)) => n.parse::<u64>().map_err(|e| e.to_string())?,
        _ => return Err("unsupported fetch depth".into()),
    };
    let mut fetch = command(&target, "git", &["fetch", "--quiet", "--no-tags"]);
    if depth > 0 {
        fetch
            .args
            .extend(["--depth".into(), depth.to_string().into()]);
    }
    fetch
        .args
        .extend([format!("file://{}", root.display()).into(), head.into()]);
    if depth == 0 || step["with"]["fetch-tags"] == true || step["with"]["fetch-tags"] == "true" {
        fetch.args.push("+refs/tags/*:refs/tags/*".into());
    }
    fetch.checked()?;
    command(&target, "git", &["checkout", "--quiet", "--detach", head]).checked()?;
    Ok(())
}
#[derive(Debug, Serialize)]
pub struct ShapeReport {
    pub commit: String,
    pub steps: Vec<String>,
    pub commands: usize,
}
/// Rebuild each selected gate's preceding checkouts from immutable local HEAD.
/// Fail on unknown shell language, zero selected commands or unsupported hosts.
/// # Errors
/// Checkout or gate failures fail with durable per-step output.
pub fn check(root: &Path, evidence: &Path) -> Result<ShapeReport, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    std::fs::create_dir(evidence).map_err(|e| e.to_string())?;
    let head = output(&command(&root, "git", &["rev-parse", "HEAD"]))?;
    let mut report = ShapeReport {
        commit: head.clone(),
        steps: Vec::new(),
        commands: 0,
    };
    for file in ["ci", "packages", "pgrx-test"] {
        let source = std::fs::read_to_string(root.join(format!(".github/workflows/{file}.yml")))
            .map_err(|e| e.to_string())?;
        let workflow = crate::workflow::parse(&source)?;
        if workflow["defaults"]["run"]["shell"] != crate::workflow::CLEAN_SHELL {
            return Err("reviewed clean shell required".into());
        }
        for (job, definition) in workflow["jobs"]
            .as_object()
            .ok_or("workflow jobs missing")?
        {
            let Some(steps) = definition["steps"].as_array() else {
                continue;
            };
            for (index, step) in steps.iter().enumerate() {
                let name = step["name"].as_str().unwrap_or("");
                if !matches!(
                    name,
                    "Test required PostgreSQL coverage guards"
                        | "Portable source-packaging and cleanup guards"
                        | "Execute actual Homebrew environment policy"
                        | "Validate source compatibility identity"
                ) {
                    continue;
                }
                if definition["runs-on"]
                    .as_str()
                    .is_some_and(|s| s.contains("macos"))
                    && !cfg!(target_os = "macos")
                {
                    return Err("selected Homebrew gate requires macOS".into());
                }
                let work = tempfile::tempdir().map_err(|e| e.to_string())?;
                let workspace = work.path().join("workspace");
                std::fs::create_dir(&workspace).map_err(|e| e.to_string())?;
                for checkout_step in &steps[..index] {
                    if checkout_step["uses"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("actions/checkout@"))
                    {
                        checkout(&root, &head, checkout_step, &workspace)?;
                    }
                }
                let cwd = workspace.join(safe_path(
                    step["working-directory"].as_str().unwrap_or("."),
                )?);
                let run = step["run"].as_str().ok_or("selected gate run missing")?;
                let mut count = 0;
                for (line_no, line) in run.trim().lines().enumerate() {
                    let args = rust_command(line)?;
                    let mut spec = CommandSpec::new("cargo");
                    spec.args = args.iter().skip(1).map(Into::into).collect();
                    spec.cwd = Some(cwd.clone());
                    spec.timeout = Duration::from_hours(1);
                    spec.output_cap = 64 * 1024 * 1024;
                    spec.env.insert("CARGO_BUILD_JOBS".into(), "2".into());
                    spec.env.insert("CARGO_INCREMENTAL".into(), "0".into());
                    let output = spec.run()?;
                    let label = format!("{file}-{job}-{index}-{line_no}");
                    let mut log = output.stdout;
                    log.extend(output.stderr);
                    std::fs::write(evidence.join(format!("{label}.log")), log)
                        .map_err(|e| e.to_string())?;
                    if !output.status.success() {
                        return Err(format!("checkout gate {label}: {}", output.status));
                    }
                    count += 1;
                }
                if count == 0 {
                    return Err("selected empty gate".into());
                }
                report.commands += count;
                report.steps.push(format!("{file}/{job}/{name}"));
            }
        }
    }
    if report.steps.len() != 5 || report.commands == 0 {
        return Err(format!(
            "selected checkout gate inventory changed: {:?}",
            report.steps
        ));
    }
    crate::owned_docker::publish(&evidence.join("checkout-shapes.json"), &report)?;
    Ok(report)
}
fn rust_command(line: &str) -> Result<Vec<String>, String> {
    if line.contains(['\'', '"', ';', '|', '&', '$', '`', '\\', '<', '>', '#']) {
        return Err("gate must use literal direct Rust argv".into());
    }
    let args: Vec<_> = line.split_ascii_whitespace().map(str::to_owned).collect();
    if args.first().map(String::as_str) != Some("cargo")
        || !args.iter().any(|a| a == "--locked")
        || !args.iter().any(|a| a == "pgokf-verifier")
    {
        return Err("Rust verifier command required".into());
    }
    if !matches!(args.get(1).map(String::as_str), Some("run" | "test")) {
        return Err("unsupported Cargo gate".into());
    }
    Ok(args)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn argv_and_checkout_paths_fail_closed() {
        rust_command("cargo run --locked -p pgokf-verifier -- history-check .").unwrap();
        for s in [
            "cargo test -p pgokf-verifier",
            "cargo test --locked -p pgokf-verifier || true",
            "# cargo test --locked -p pgokf-verifier",
            "python3 test.py",
        ] {
            assert!(rust_command(s).is_err());
        }
        for path in ["../escape", "/tmp/escape", "tools/../../escape"] {
            assert!(safe_path(path).is_err());
        }
        safe_path("release-tools").unwrap();
        assert_eq!(safe_path(".").unwrap(), PathBuf::new());
    }
}
