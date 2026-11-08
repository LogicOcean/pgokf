//! Repository source inventory and explicit Python tooling rejection.
//! No baseline allowlist of repository Python exists.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// Audit tracked and nonignored untracked files, including tracked build paths.
///
/// # Errors
/// Fails on unreadable entries, malformed workflows, or prohibited Python tooling.
pub fn check(root: &Path) -> Result<(), String> {
    let trusted = crate::json::from_slice(include_bytes!("../../../source-review.json"))?;
    check_reviewed(root, &trusted)
}

/// Audit against an independently trusted review, never a newly generated baseline.
/// The CLI uses the review embedded in the verifier binary at build time.
/// # Errors
/// Changed review data, source identities, or forbidden tooling fail closed.
#[allow(clippy::too_many_lines)] // One closed inventory pass, followed by trusted identity comparison.
pub fn check_reviewed(root: &Path, trusted: &Review) -> Result<(), String> {
    let mut problems = Vec::new();
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    if !root.join(".git").exists() {
        return Err("source guard requires the repository root".into());
    }
    let mut command = crate::process::CommandSpec::new("git");
    command.cwd = Some(root.clone());
    command.args = [
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
    ]
    .map(Into::into)
    .into();
    let output = command.checked()?;
    let names = std::str::from_utf8(&output.stdout).map_err(|e| e.to_string())?;
    let mut actual = BTreeMap::new();
    let names: BTreeSet<_> = names
        .split(char::from(0))
        .filter(|s| !s.is_empty())
        .collect();
    for name in names {
        let path = root.join(name);
        let metadata = path
            .symlink_metadata()
            .map_err(|e| format!("{name}: {e}"))?;
        if metadata.file_type().is_symlink() {
            problems.push(format!("{name}: source symlink requires explicit review"));
            continue;
        }
        if !metadata.is_file() {
            return Err(format!("{name}: source is not a regular file"));
        }
        if path.extension().is_some_and(|e| {
            e.eq_ignore_ascii_case("py")
                || e.eq_ignore_ascii_case("pyw")
                || e.eq_ignore_ascii_case("pyc")
        }) {
            problems.push(format!("{name}: repository-owned Python file"));
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        if name != "source-review.json" {
            actual.insert(
                name.to_owned(),
                ReviewedFile {
                    sha256: digest(&bytes),
                    executable: is_executable(&metadata),
                },
            );
        }
        let text = std::str::from_utf8(&bytes).ok();
        if text.is_some_and(|s| {
            s.starts_with("#!") && python_tooling(s.lines().next().unwrap_or_default())
        }) {
            problems.push(format!("{name}: Python shebang"));
        }
        if (name.starts_with(".github/")
            || matches!(
                path.file_name().and_then(|n| n.to_str()),
                Some("action.yml" | "action.yaml")
            ))
            && matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("yml" | "yaml")
            )
        {
            let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
            check_workflow(name, &text, &mut problems)?;
        } else if (matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("sh" | "bash" | "zsh" | "fish" | "toml" | "rb" | "js" | "ts")
        ) || path.extension().is_none()
            || path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("requirements"))
            || path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("Dockerfile")))
            && let Some(text) = text
            && python_tooling(text)
        {
            problems.push(format!("{name}: Python executable tooling"));
        }
    }
    let review_path = root.join("source-review.json");
    match std::fs::read(&review_path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| crate::json::from_slice::<Review>(&bytes))
    {
        Ok(review) => {
            if &review != trusted {
                problems.push("source review differs from trusted verifier inventory".into());
            }
            if review.version != 1 {
                problems.push("unsupported source review version".into());
            }
            for (name, hash) in &actual {
                if review.files.get(name) != Some(hash) {
                    problems.push(format!("{name}: unreviewed source execution input"));
                }
            }
            for name in review.files.keys() {
                if !actual.contains_key(name) {
                    problems.push(format!("{name}: stale source review entry"));
                }
            }
        }
        Err(error) => problems.push(format!("source-review.json: {error}")),
    }
    problems.sort();
    problems.dedup();

    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

/// A reviewed closed source language: every owned input has an exact identity.
/// This is a review boundary, not proof that arbitrary reviewed programs are safe.
/// The manifest itself is data, parsed duplicate-safely, and never executed.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub version: u32,
    pub files: BTreeMap<String, ReviewedFile>,
}
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewedFile {
    pub sha256: String,
    pub executable: bool,
}
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}
#[allow(clippy::format_collect)]
fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn python_tooling(text: &str) -> bool {
    // Concatenated shell quoting and line continuations must not hide a name.
    let lower = text
        .replace("\\\n", "")
        .replace(['\'', '"', '\\'], "")
        .to_ascii_lowercase();
    lower
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
        .any(|word| {
            word.starts_with("python")
                || word.starts_with("pypy")
                || matches!(
                    word,
                    "pytest" | "unittest" | "pyyaml" | "pip" | "pip3" | "setup-python"
                )
                || Path::new(word)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("py"))
        })
}

fn check_workflow(name: &str, text: &str, problems: &mut Vec<String>) -> Result<(), String> {
    let value: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| format!("{name}: {e}"))?;
    let mut value = value;
    value.apply_merge().map_err(|e| format!("{name}: {e}"))?;
    inspect_yaml(name, &value, problems);
    Ok(())
}

fn inspect_yaml(name: &str, value: &serde_yaml::Value, problems: &mut Vec<String>) {
    use serde_yaml::Value;
    if let Value::Mapping(mapping) = value {
        // These are the only third-party Python steps exempted from the policy.
        // Exact object equality prevents attached run/env/condition overrides.
        if name == ".github/workflows/docs.yml" {
            let allowed = [
                "name: Set up Python\nuses: actions/setup-python@5fda3b95a4ea91299a34e894583c3862153e4b97\nwith:\n  python-version: '3.12'\n",
                "name: Install MkDocs Material\nrun: pip install mkdocs-material\n",
            ];
            if allowed
                .iter()
                .any(|s| serde_yaml::from_str::<Value>(s).is_ok_and(|v| v == *value))
            {
                return;
            }
        }
        for (_, val) in mapping {
            inspect_yaml(name, val, problems);
        }
    } else if let Value::Sequence(values) = value {
        for item in values {
            inspect_yaml(name, item, problems);
        }
    } else if let Value::String(text) = value
        && python_tooling(text)
    {
        problems.push(format!("{name}: Python workflow tooling: {text}"));
    } else if matches!(value, Value::Tagged(_)) {
        problems.push(format!("{name}: unsupported tagged workflow value"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tracked_ignored_paths_and_symlinks_cannot_hide_sources() {
        let root = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(root.path())
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        };
        git(&["init", "--quiet"]);
        std::fs::write(root.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::fs::write(root.path().join("target/check.py"), "pass\n").unwrap();
        git(&["add", "-f", "target/check.py"]);
        assert!(check(root.path()).unwrap_err().contains("target/check.py"));
        git(&["rm", "-f", "target/check.py"]);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(".gitignore", root.path().join("check")).unwrap();
            assert!(check(root.path()).unwrap_err().contains("source symlink"));
            std::fs::remove_file(root.path().join("check")).unwrap();
        }
        assert!(
            check(root.path())
                .unwrap_err()
                .contains("source-review.json")
        );
        std::fs::write(root.path().join("untracked.py"), "pass\n").unwrap();
        assert!(check(root.path()).unwrap_err().contains("untracked.py"));
    }
    #[test]
    fn interpreter_and_module_variants_are_rejected() {
        for command in [
            "python3 tests/check.py",
            "python -m unittest",
            "env python3.12 check",
            "pytest",
            "UnItTeSt",
            "PyYaMl",
            "pip3 install package",
            "pip install package",
            "pypy verifier",
            "pip install PyYAML",
            "./tests/verify.py",
            "pypy check.py",
            "actions/setup-python@pinned-revision",
            "p'y'thon3 -c pass",
            "py\\\nthon3 -c pass",
        ] {
            assert!(python_tooling(command), "{command}");
        }
    }
    #[test]
    fn docs_exception_is_exact_and_cannot_hide_verification() {
        let step = "name: Install MkDocs Material\nrun: pip install mkdocs-material\n";
        let mut errors = vec![];
        check_workflow(".github/workflows/docs.yml", step, &mut errors).unwrap();
        assert!(errors.is_empty());
        check_workflow(".github/workflows/ci.yml", step, &mut errors).unwrap();
        assert!(!errors.is_empty());
        errors.clear();
        check_workflow(
            ".github/workflows/docs.yml",
            &format!("{step}env:\n  VERIFY: python3 tests/check.py\n"),
            &mut errors,
        )
        .unwrap();
        assert!(!errors.is_empty());
    }
    #[test]
    fn malformed_workflows_fail_closed() {
        assert!(check_workflow("ci.yml", "steps: [", &mut vec![]).is_err());
        let mut errors = Vec::new();
        check_workflow("ci.yml", "run: !command cargo test", &mut errors).unwrap();
        assert!(!errors.is_empty());
    }
}
