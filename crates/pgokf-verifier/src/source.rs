//! Repository source inventory and explicit Python tooling rejection.
//! No baseline allowlist of repository Python exists.
use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    path::{Path, PathBuf},
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
    let git = trusted_git_program()?;
    check_index_matches_head(&git, &root, &mut problems)?;
    let index_modes = git_index_modes(&git, &root, &mut problems)?;
    let head_entries = git_tree_entries(&git, &root)?;
    let names = source_names(&git, &root, &index_modes, &head_entries)?;
    let mut actual = BTreeMap::new();
    for name in names {
        let path = root.join(&name);
        let metadata = path
            .symlink_metadata()
            .map_err(|e| format!("{name}: {e}"))?;
        check_checkout_mode(
            &name,
            &metadata,
            index_modes.get(&name),
            "Git index",
            &mut problems,
        );
        check_checkout_mode(
            &name,
            &metadata,
            head_entries.get(&name).map(|entry| &entry.mode),
            "committed HEAD",
            &mut problems,
        );
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
                name.clone(),
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
            check_workflow(&name, &text, &mut problems)?;
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

fn trusted_git_program() -> Result<PathBuf, String> {
    trusted_git_from_build_environment(option_env!("PGOKF_BUILD_GIT"), option_env!("PATH"))
}

fn trusted_git_from_build_environment(
    explicit: Option<&str>,
    build_path: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(explicit) = explicit {
        return validate_git_program(Path::new(explicit));
    }
    let build_path = build_path.ok_or("Git was not pinned by the verifier build environment")?;
    #[cfg(windows)]
    let executable = "git.exe";
    #[cfg(not(windows))]
    let executable = "git";
    for directory in std::env::split_paths(OsStr::new(build_path)) {
        if directory.is_absolute() {
            let candidate = directory.join(executable);
            if candidate.exists() {
                return validate_git_program(&candidate);
            }
        }
    }
    Err("verifier build PATH contained no trusted absolute Git executable; rebuild with PGOKF_BUILD_GIT set to an absolute path".into())
}

fn validate_git_program(candidate: &Path) -> Result<PathBuf, String> {
    if !candidate.is_absolute() {
        return Err("trusted Git executable path must be absolute".into());
    }
    let canonical = candidate
        .canonicalize()
        .map_err(|e| format!("trusted Git executable {}: {e}", candidate.display()))?;
    let metadata = canonical
        .metadata()
        .map_err(|e| format!("trusted Git executable {}: {e}", canonical.display()))?;
    if !metadata.is_file() || !is_executable(&metadata) {
        return Err(format!(
            "trusted Git executable is not an executable regular file: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn git_command(git: &Path, root: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    const REDIRECTING_ENV: [&str; 14] = [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_SYSTEM",
        "GIT_DIR",
        "GIT_EXEC_PATH",
        "GIT_INDEX_FILE",
        "GIT_NAMESPACE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_REPLACE_REF_BASE",
        "GIT_WORK_TREE",
    ];
    let mut command = crate::process::CommandSpec::new(git);
    command.cwd = Some(root.to_owned());
    command.args = [
        "--no-replace-objects",
        "-c",
        "core.excludesFile=/dev/null",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.ignoreCase=false",
        "-c",
        "core.precomposeUnicode=false",
        "-c",
        "core.untrackedCache=false",
    ]
    .into_iter()
    .chain(args.iter().copied())
    .map(Into::into)
    .collect();
    command.env_remove = REDIRECTING_ENV.map(Into::into).into();
    command.env.extend([
        ("GIT_ATTR_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
        ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
    ]);
    command.run()
}

fn check_index_matches_head(
    git: &Path,
    root: &Path,
    problems: &mut Vec<String>,
) -> Result<(), String> {
    let head = git_command(
        git,
        root,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
    )?;
    if !head.status.success() {
        if head.status.code() == Some(1) {
            return Err("source guard requires a committed HEAD".into());
        }
        return Err(format!(
            "git rev-parse HEAD: {}: {}",
            head.status,
            String::from_utf8_lossy(&head.stderr)
        ));
    }
    let diff = git_command(
        git,
        root,
        &["diff-index", "--cached", "--quiet", "HEAD", "--"],
    )?;
    if !diff.status.success() {
        if diff.status.code() == Some(1) {
            problems.push("Git index differs from committed HEAD".into());
        } else {
            return Err(format!(
                "git diff-index: {}: {}",
                diff.status,
                String::from_utf8_lossy(&diff.stderr)
            ));
        }
    }
    Ok(())
}

fn git_index_modes(
    git: &Path,
    root: &Path,
    problems: &mut Vec<String>,
) -> Result<BTreeMap<String, String>, String> {
    let output = git_command(git, root, &["ls-files", "--cached", "--stage", "-z"])?;
    if !output.status.success() {
        return Err(format!(
            "git ls-files --stage: {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    parse_index_modes(&output.stdout, problems)
}

fn parse_index_modes(
    bytes: &[u8],
    problems: &mut Vec<String>,
) -> Result<BTreeMap<String, String>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let mut modes = BTreeMap::new();
    for record in text
        .split(char::from(0))
        .filter(|record| !record.is_empty())
    {
        let (identity, name) = record
            .split_once('\t')
            .ok_or_else(|| "malformed git index entry".to_owned())?;
        let mut fields = identity.split(' ');
        let mode = fields.next().ok_or("missing git index mode")?;
        let _object = fields.next().ok_or("missing git index object")?;
        let stage = fields.next().ok_or("missing git index stage")?;
        if fields.next().is_some() {
            return Err("malformed git index identity".into());
        }
        if stage != "0" {
            problems.push(format!("{name}: unmerged Git index entry"));
        }
        if modes.insert(name.to_owned(), mode.to_owned()).is_some() {
            problems.push(format!("{name}: duplicate Git index entry"));
        }
    }
    Ok(modes)
}

#[derive(Debug)]
struct GitTreeEntry {
    mode: String,
    object: String,
}

fn git_tree_entries(git: &Path, root: &Path) -> Result<BTreeMap<String, GitTreeEntry>, String> {
    let output = git_command(git, root, &["ls-tree", "-r", "-z", "--full-tree", "HEAD"])?;
    if !output.status.success() {
        return Err(format!(
            "git ls-tree HEAD: {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    parse_tree_entries(&output.stdout)
}

fn parse_tree_entries(bytes: &[u8]) -> Result<BTreeMap<String, GitTreeEntry>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let mut entries = BTreeMap::new();
    for record in text
        .split(char::from(0))
        .filter(|record| !record.is_empty())
    {
        let (identity, name) = record
            .split_once('\t')
            .ok_or_else(|| "malformed Git tree entry".to_owned())?;
        let mut fields = identity.split(' ');
        let mode = fields.next().ok_or("missing Git tree mode")?;
        let _kind = fields.next().ok_or("missing Git tree type")?;
        let object = fields.next().ok_or("missing Git tree object")?;
        if fields.next().is_some() {
            return Err("malformed Git tree identity".into());
        }
        if entries
            .insert(
                name.to_owned(),
                GitTreeEntry {
                    mode: mode.to_owned(),
                    object: object.to_owned(),
                },
            )
            .is_some()
        {
            return Err(format!("{name}: duplicate Git tree entry"));
        }
    }
    Ok(entries)
}

fn source_names(
    git: &Path,
    root: &Path,
    index_modes: &BTreeMap<String, String>,
    head_entries: &BTreeMap<String, GitTreeEntry>,
) -> Result<BTreeSet<String>, String> {
    let matchers = committed_ignore_matchers(git, root, head_entries)?;
    let tracked: BTreeSet<_> = index_modes.keys().cloned().collect();
    let mut names = tracked.clone();
    walk_source_names(root, root, &tracked, &matchers, &mut names)?;
    Ok(names)
}

fn committed_ignore_matchers(
    git: &Path,
    root: &Path,
    head_entries: &BTreeMap<String, GitTreeEntry>,
) -> Result<Vec<(PathBuf, Gitignore)>, String> {
    let mut matchers = Vec::new();
    for (name, entry) in head_entries {
        let path = Path::new(name);
        if path.file_name() != Some(OsStr::new(".gitignore"))
            || !matches!(entry.mode.as_str(), "100644" | "100755")
        {
            continue;
        }
        let output = git_command(git, root, &["cat-file", "blob", entry.object.as_str()])?;
        if !output.status.success() {
            return Err(format!(
                "git cat-file {}: {}: {}",
                entry.object,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let contents = std::str::from_utf8(&output.stdout)
            .map_err(|e| format!("{name}: committed ignore definition is not UTF-8: {e}"))?;
        let relative_base = path.parent().unwrap_or_else(|| Path::new("")).to_owned();
        let absolute_ignore = root.join(path);
        let mut builder = GitignoreBuilder::new(root.join(&relative_base));
        for (index, line) in contents.lines().enumerate() {
            let line = if index == 0 {
                line.trim_start_matches('\u{feff}')
            } else {
                line
            };
            builder
                .add_line(Some(absolute_ignore.clone()), line)
                .map_err(|e| format!("{name}: invalid committed ignore definition: {e}"))?;
        }
        let matcher = builder
            .build()
            .map_err(|e| format!("{name}: invalid committed ignore definition: {e}"))?;
        matchers.push((relative_base, matcher));
    }
    matchers.sort_by(|(left, _), (right, _)| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    Ok(matchers)
}

fn walk_source_names(
    root: &Path,
    directory: &Path,
    tracked: &BTreeSet<String>,
    matchers: &[(PathBuf, Gitignore)],
    names: &mut BTreeSet<String>,
) -> Result<(), String> {
    let mut entries = std::fs::read_dir(directory)
        .map_err(|e| format!("{}: {e}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root).map_err(|e| e.to_string())?;
        if relative == Path::new(".git") {
            continue;
        }
        let name = relative
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 source path: {}", relative.display()))?;
        let metadata = path
            .symlink_metadata()
            .map_err(|e| format!("{name}: {e}"))?;
        let is_directory = metadata.is_dir() && !metadata.file_type().is_symlink();
        if is_directory {
            if !ignored_by_committed_rules(root, relative, true, matchers) {
                walk_source_names(root, &path, tracked, matchers, names)?;
            }
        } else if tracked.contains(name)
            || !ignored_by_committed_rules(root, relative, false, matchers)
        {
            names.insert(name.to_owned());
        }
    }
    Ok(())
}

fn ignored_by_committed_rules(
    root: &Path,
    relative: &Path,
    is_directory: bool,
    matchers: &[(PathBuf, Gitignore)],
) -> bool {
    let mut ignored = false;
    for (base, matcher) in matchers {
        if relative.starts_with(base) {
            match matcher.matched(root.join(relative), is_directory) {
                Match::Ignore(_) => ignored = true,
                Match::Whitelist(_) => ignored = false,
                Match::None => {}
            }
        }
    }
    ignored
}

fn check_checkout_mode(
    name: &str,
    metadata: &std::fs::Metadata,
    expected: Option<&String>,
    source: &str,
    problems: &mut Vec<String>,
) {
    if let Some(expected) = expected
        && checkout_git_mode(metadata) != Some(expected.as_str())
    {
        problems.push(format!("{name}: checkout mode/type differs from {source}"));
    }
}

fn checkout_git_mode(metadata: &std::fs::Metadata) -> Option<&'static str> {
    if metadata.file_type().is_symlink() {
        Some("120000")
    } else if metadata.is_file() {
        if is_executable(metadata) {
            Some("100755")
        } else {
            Some("100644")
        }
    } else {
        None
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
        git(&[
            "-c",
            "user.name=Source Guard Test",
            "-c",
            "user.email=source-guard@example.invalid",
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "fixture root",
        ]);
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
    #[cfg(unix)]
    #[test]
    fn build_pinned_git_accepts_non_fhs_absolute_location_and_rejects_relative_override() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("custom-prefix/tools/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let git = bin.join("git");
        std::fs::write(&git, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let canonical = git.canonicalize().unwrap();
        let build_path = std::env::join_paths([&bin]).unwrap();

        assert_eq!(
            trusted_git_from_build_environment(None, build_path.to_str()).unwrap(),
            canonical
        );
        assert_eq!(
            trusted_git_from_build_environment(Some(git.to_str().unwrap()), None).unwrap(),
            canonical
        );
        assert!(trusted_git_from_build_environment(Some("relative/git"), None).is_err());
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
