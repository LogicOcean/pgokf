//! Bounded release and compatibility commands; no publication operations.
use crate::process::CommandSpec;
use crate::release_identity::{ReleaseIntent, SourceManifest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

pub const HISTORY: [(&str, &str, &str); 2] = [
    (
        "v0.2.0",
        "9dd70d7e3b547a4090f60f4a0a53f95e8548d024",
        "59d29d3dc4b78cd5c5000bf473b0f097c9fb2ea4",
    ),
    (
        "v0.3.0",
        "70d481849395a1ae9535aa91c91599fd90e9ab75",
        "4e3a45c371d6e8d79238c1fcff48a875cd30ed88",
    ),
];

pub(crate) fn command(root: &Path, program: &str, args: &[&str]) -> CommandSpec {
    let mut spec = CommandSpec::new(program);
    spec.cwd = Some(root.to_owned());
    spec.args = args.iter().map(OsString::from).collect();
    spec
}
pub(crate) fn output(spec: &CommandSpec) -> Result<String, String> {
    String::from_utf8(spec.checked()?.stdout)
        .map(|s| s.trim().to_owned())
        .map_err(|e| e.to_string())
}
/// # Errors
/// Absent or moved immutable tags fail; no skipped-history success.
pub fn history_check(root: &Path) -> Result<(), String> {
    for (tag, object, commit) in HISTORY {
        let found = output(&command(
            root,
            "git",
            &["rev-parse", "--verify", &format!("refs/tags/{tag}")],
        ))
        .map_err(|_| format!("release history absent: {tag}; fetch-depth: 0 required"))?;
        let peeled = output(&command(
            root,
            "git",
            &[
                "rev-parse",
                "--verify",
                &format!("refs/tags/{tag}^{{commit}}"),
            ],
        ))?;
        if found != object || peeled != commit {
            return Err(format!("immutable release tag {tag} moved"));
        }
        output(&command(
            root,
            "git",
            &["rev-parse", "--verify", &format!("{commit}^{{tree}}")],
        ))?;
    }
    Ok(())
}
/// # Errors
/// Inconsistent source or immutable tag identity fails before any output publication.
pub fn resolve(
    root: &Path,
    event_name: &str,
    event_ref: &str,
    dispatch: &str,
) -> Result<ReleaseIntent, String> {
    let identity = crate::release_identity::source_identity(root)?;
    let head = output(&command(root, "git", &["rev-parse", "HEAD"]))?;
    let clean = command(root, "git", &["diff", "--quiet", "HEAD", "--"])
        .run()?
        .status
        .success();
    crate::release_identity::release_intent(
        &identity.version,
        &head,
        clean,
        event_name,
        event_ref,
        dispatch,
        |tag| {
            output(&command(
                root,
                "git",
                &["rev-parse", "--verify", &format!("{tag}^{{commit}}")],
            ))
        },
    )
}
/// Write all validated fields together; this function is never called on refusal.
/// # Errors
/// Opening or synchronizing the caller-owned output fails.
pub fn write_intent(path: &Path, intent: &ReleaseIntent) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    write!(
        file,
        "version={}\npublish={}\nsource_ref={}\n",
        intent.version, intent.publish, intent.source_ref
    )
    .map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageManifest {
    pub source: SourceManifest,
    pub source_commit: String,
    pub image_id: String,
    pub postgres_major: u32,
    pub postgres_image_tag: String,
    pub tooling_commit: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceArtifact {
    pub source: SourceManifest,
    pub commit: String,
    pub tree: String,
    pub control_sha256: String,
}
/// # Errors
/// Dirty or inconsistent source cannot produce an immutable artifact contract.
pub fn source_manifest(root: &Path) -> Result<SourceArtifact, String> {
    command(root, "git", &["diff", "--exit-code", "HEAD", "--"]).checked()?;
    Ok(SourceArtifact {
        source: crate::release_identity::source_identity(root)?,
        commit: output(&command(root, "git", &["rev-parse", "HEAD"]))?,
        tree: output(&command(root, "git", &["rev-parse", "HEAD^{tree}"]))?,
        control_sha256: digest(
            &std::fs::read(root.join("crates/extension/pgokf.control"))
                .map_err(|e| e.to_string())?,
        ),
    })
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackManifest {
    pub schema_version: u32,
    pub from: ImageManifest,
    pub to: ImageManifest,
    pub previous_formula: crate::release_identity::FormulaTuple,
}
/// # Errors
/// Invalid immutable identities, unauthenticated rollback tuple or major changes fail.
pub fn validate_rollback(manifest: &RollbackManifest) -> Result<(), String> {
    if manifest.schema_version != 1 || manifest.from.postgres_major != manifest.to.postgres_major {
        return Err("rollback contract version/PostgreSQL major mismatch".into());
    }
    for image in [&manifest.from, &manifest.to] {
        semver::Version::parse(&image.source.version)
            .map_err(|e| format!("invalid source version: {e}"))?;
        if !image.source.packages.contains("pgokf") || image.source.version.is_empty() {
            return Err("rollback source package identity missing".into());
        }
        let distribution = if image.source.version.contains('-') {
            crate::release_identity::LAST_PUBLISHED
        } else {
            &image.source.version
        };
        if image.source.distribution_version != distribution || image.source.version == "0.3.0" {
            return Err("rollback source distribution identity mismatch".into());
        }
        if !(15..=19).contains(&image.postgres_major)
            || image.postgres_image_tag
                != if image.postgres_major == 19 {
                    "19rc2".into()
                } else {
                    image.postgres_major.to_string()
                }
        {
            return Err("unreviewed rollback PostgreSQL identity".into());
        }
        for oid in [&image.source_commit, &image.tooling_commit] {
            if oid.len() != 40 || !oid.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Err("invalid source commit".into());
            }
        }
        let digest = image
            .image_id
            .strip_prefix("sha256:")
            .ok_or("immutable image ID required")?;
        if digest.len() != 64 || !digest.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("invalid image digest".into());
        }
    }
    if manifest.previous_formula.version != manifest.to.source.version {
        return Err("rollback formula is unrelated to target source".into());
    }
    crate::release_identity::authenticate_formula(
        &manifest.previous_formula,
        &crate::release_identity::published_digests(),
    )
}

/// Authenticate both contracts against immutable local Git objects and Docker
/// image provenance. Old images lacking build provenance fail closed.
/// # Errors
/// Correlated contract forgeries, missing source objects and absent image labels fail.
pub fn validate_rollback_at(
    root: &Path,
    docker: &mut impl crate::owned_docker::DockerApi,
    manifest: &RollbackManifest,
) -> Result<(), String> {
    validate_rollback(manifest)?;
    for image in [&manifest.from, &manifest.to] {
        if image.source.version != env!("CARGO_PKG_VERSION")
            && !HISTORY.iter().any(|(tag, _, commit)| {
                *tag == format!("v{}", image.source.version) && *commit == image.source_commit
            })
        {
            return Err("rollback historical source is not in immutable release history".into());
        }
        let archived = source_archive(root, &image.source_commit)?;
        let actual = crate::release_identity::source_identity_version(
            archived.path(),
            &image.source.version,
        )?;
        if actual != image.source {
            return Err("rollback source differs from immutable Git inventory".into());
        }
        let source: toml::Value = toml::from_str(
            &std::fs::read_to_string(archived.path().join("crates/extension/Cargo.toml"))
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        if source
            .get("features")
            .and_then(|v| v.get(format!("pg{}", image.postgres_major)))
            .is_none()
        {
            return Err("source does not support rollback PostgreSQL major".into());
        }
        output(&command(
            root,
            "git",
            &[
                "rev-parse",
                "--verify",
                &format!("{}^{{commit}}", image.tooling_commit),
            ],
        ))?;
        let text = docker.call(&["image".into(), "inspect".into(), image.image_id.clone()])?;
        let values: Vec<serde_json::Value> = crate::json::from_slice(text.as_bytes())?;
        if values.len() != 1 || values[0]["Id"] != image.image_id {
            return Err("immutable rollback image missing".into());
        }
        let labels = &values[0]["Config"]["Labels"];
        for (key, expected) in [
            (
                "org.opencontainers.image.version",
                image.source.version.clone(),
            ),
            (
                "org.opencontainers.image.revision",
                image.source_commit.clone(),
            ),
            (
                "org.pgokf.verifier.tooling-revision",
                image.tooling_commit.clone(),
            ),
            (
                "org.pgokf.verifier.source-contract",
                digest(&serde_json::to_vec(&image.source).map_err(|e| e.to_string())?),
            ),
            (
                "org.pgokf.verifier.postgres",
                image.postgres_image_tag.clone(),
            ),
        ] {
            if labels[key] != expected {
                return Err(format!("rollback image provenance mismatch: {key}"));
            }
        }
    }
    Ok(())
}

fn source_archive(root: &Path, commit: &str) -> Result<tempfile::TempDir, String> {
    let work = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut archive = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
    let mut capture = command(root, "git", &["archive", "--format=tar", "--output"]);
    capture.args.extend([archive.path().into(), commit.into()]);
    capture.checked()?;
    archive.flush().map_err(|e| e.to_string())?;
    let mut extract = command(work.path(), "tar", &["-xf"]);
    extract.args.push(archive.path().into());
    extract.checked()?;
    Ok(work)
}

/// Build current source with current validation tooling in an isolated recipe.
/// # Errors
/// Historical source, recipe drift, failed build or invalid Docker evidence fails.
pub fn compatibility_image(
    source: &Path,
    tooling: &Path,
    image_tag: &str,
) -> Result<ImageManifest, String> {
    image_build(source, tooling, env!("CARGO_PKG_VERSION"), 19, image_tag)
}

fn image_source_commit(source: &Path, version: &str) -> Result<String, String> {
    if version == env!("CARGO_PKG_VERSION") {
        crate::release_identity::source_identity(source)?;
        command(source, "git", &["diff", "--exit-code", "HEAD", "--"]).checked()?;
        output(&command(source, "git", &["rev-parse", "HEAD"]))
    } else {
        history_check(source)?;
        if !crate::release_identity::published_digests().contains_key(version) {
            return Err("historical build requires authenticated published formula".into());
        }
        Ok(HISTORY
            .iter()
            .find(|(tag, _, _)| *tag == format!("v{version}"))
            .ok_or("historical build requires immutable release pin")?
            .2
            .to_owned())
    }
}

fn image_feature(context: &Path, postgres_major: u32) -> Result<(), String> {
    let manifest: toml::Value = toml::from_str(
        &std::fs::read_to_string(context.join("crates/extension/Cargo.toml"))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    if manifest
        .get("features")
        .and_then(|v| v.get(format!("pg{postgres_major}")))
        .is_none()
    {
        return Err("source does not support requested PostgreSQL major".into());
    }
    Ok(())
}

fn verify_base(postgres_major: u32) -> Result<(), String> {
    if postgres_major == 19 {
        crate::postgres_pin::beta_image(&mut crate::owned_docker::Docker::default())?;
    }
    Ok(())
}

/// Build a local provenance-bearing image from current or authenticated published
/// source. Historical builds are rollback artifacts, never release eligibility.
/// # Errors
/// Unauthenticated history, unsupported majors, dirty tooling or failed builds fail.
pub fn image_build(
    source: &Path,
    tooling: &Path,
    version: &str,
    postgres_major: u32,
    image_tag: &str,
) -> Result<ImageManifest, String> {
    image_build_inner(source, tooling, version, postgres_major, image_tag, false)
}
/// Retain the same immutable builder stage locally for compiler/check gates.
/// The builder tag is `IMAGE_TAG-builder`; the returned manifest is runtime-only.
/// # Errors
/// Source authentication, either build stage or immutable image inspection fails.
pub fn image_build_with_builder(
    source: &Path,
    tooling: &Path,
    version: &str,
    postgres_major: u32,
    image_tag: &str,
) -> Result<ImageManifest, String> {
    image_build_inner(source, tooling, version, postgres_major, image_tag, true)
}
fn execute_image_build(
    build: &CommandSpec,
    image_tag: &str,
    keep_builder: bool,
) -> Result<(), String> {
    if keep_builder {
        let mut builder = build.clone();
        let tag_position = builder.args.len() - 2;
        builder.args[tag_position] = format!("{image_tag}-builder").into();
        builder.args.pop();
        builder
            .args
            .extend(["--target".into(), "builder".into(), ".".into()]);
        builder.checked()?;
    }
    build.checked()?;
    Ok(())
}
fn image_build_inner(
    source: &Path,
    tooling: &Path,
    version: &str,
    postgres_major: u32,
    image_tag: &str,
    keep_builder: bool,
) -> Result<ImageManifest, String> {
    if !(15..=19).contains(&postgres_major) {
        return Err("unsupported PostgreSQL major".into());
    }
    let source = source.canonicalize().map_err(|e| e.to_string())?;
    let tooling = tooling.canonicalize().map_err(|e| e.to_string())?;
    command(&tooling, "git", &["diff", "--exit-code", "HEAD", "--"]).checked()?;
    let tooling_commit = output(&command(&tooling, "git", &["rev-parse", "HEAD"]))?;
    let tooling_archive = source_archive(&tooling, &tooling_commit)?;
    verify_base(postgres_major)?;
    let source_commit = image_source_commit(&source, version)?;
    let archived = source_archive(&source, &source_commit)?;
    let context = archived.path();
    let identity = crate::release_identity::source_identity_version(context, version)?;
    image_feature(context, postgres_major)?;
    let postgres_image_tag = if postgres_major == 19 {
        "19rc2".into()
    } else {
        postgres_major.to_string()
    };
    let recipe =
        std::fs::read_to_string(tooling_archive.path().join("packaging/docker/Dockerfile"))
            .map_err(|e| e.to_string())?;
    if recipe.matches("COPY . /verification").count() != 1 {
        return Err("unique beta validation COPY required".into());
    }
    let work = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = work.path().join("Dockerfile");
    std::fs::write(
        &path,
        recipe
            .replace(
                "COPY . /verification",
                "COPY --from=validation . /verification",
            )
            .replace(
                "RUN cargo",
                "RUN CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 cargo",
            ),
    )
    .map_err(|e| e.to_string())?;
    let mut build = command(context, "docker", &["build", "-f"]);
    build.args.extend([
        path.into_os_string(),
        "--build-context".into(),
        format!("validation={}", tooling_archive.path().display()).into(),
    ]);
    for arg in [
        format!("PG_MAJOR={postgres_major}"),
        format!("PG_IMAGE_TAG={postgres_image_tag}"),
        format!(
            "PG_BASE_IMAGE={}",
            crate::postgres_pin::base_image(postgres_major, &postgres_image_tag)?
        ),
        format!("PGOKF_VERSION={}", identity.version),
        "WITH_PGVECTOR=0".into(),
        "WITH_PG_CRON=0".into(),
        "WITH_PG_TEXTSEARCH=0".into(),
    ] {
        build.args.extend(["--build-arg".into(), arg.into()]);
    }
    for (key, value) in [
        ("org.opencontainers.image.revision", source_commit.clone()),
        (
            "org.pgokf.verifier.tooling-revision",
            tooling_commit.clone(),
        ),
        (
            "org.pgokf.verifier.source-contract",
            digest(&serde_json::to_vec(&identity).map_err(|e| e.to_string())?),
        ),
        ("org.pgokf.verifier.postgres", postgres_image_tag.clone()),
    ] {
        build
            .args
            .extend(["--label".into(), format!("{key}={value}").into()]);
    }
    build
        .args
        .extend(["-t".into(), image_tag.into(), ".".into()]);
    build.timeout = Duration::from_hours(2);
    build.output_cap = 64 * 1024 * 1024;
    execute_image_build(&build, image_tag, keep_builder)?;
    let image_id = output(&command(
        &source,
        "docker",
        &["image", "inspect", "--format", "{{.Id}}", image_tag],
    ))?;
    let manifest = ImageManifest {
        source: identity,
        source_commit,
        image_id,
        postgres_major,
        postgres_image_tag,
        tooling_commit,
    };
    Ok(manifest)
}

fn brew(root: &Path, args: &[&str]) -> CommandSpec {
    let mut spec = command(
        root,
        "/usr/bin/env",
        &[
            "-u",
            "RUSTFLAGS",
            "-u",
            "CARGO_ENCODED_RUSTFLAGS",
            "-u",
            "MACOSX_DEPLOYMENT_TARGET",
            "-u",
            "CARGO_TARGET_DIR",
            "-u",
            "PGRX_HOME",
            "-u",
            "CARGO_HOME",
            "brew",
        ],
    );
    spec.args.extend(args.iter().map(OsString::from));
    spec.env
        .insert("HOMEBREW_NO_AUTO_UPDATE".into(), "1".into());
    spec.env
        .insert("HOMEBREW_NO_INSTALL_CLEANUP".into(), "1".into());
    spec.env.insert("HOMEBREW_NO_AUTOREMOVE".into(), "1".into());
    spec.timeout = Duration::from_hours(2);
    spec.output_cap = 64 * 1024 * 1024;
    spec
}
fn tap(root: &Path, prefix: &str) -> Result<(tempfile::TempDir, String), String> {
    if !cfg!(target_os = "macos") {
        return Err("Homebrew API gate requires macOS".into());
    }
    let repo = PathBuf::from(output(&brew(root, &["--repository"]))?);
    let parent = repo.join("Library/Taps/pgokf-local");
    std::fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
    let dir = tempfile::Builder::new()
        .prefix(&format!("homebrew-{prefix}-"))
        .tempdir_in(parent)
        .map_err(|e| e.to_string())?;
    let name = dir
        .path()
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("tap name")?
        .strip_prefix("homebrew-")
        .ok_or("tap prefix")?;
    let name = format!("pgokf-local/{name}");
    std::fs::create_dir(dir.path().join("Formula")).map_err(|e| e.to_string())?;
    command(dir.path(), "git", &["init", "-q"]).checked()?;
    Ok((dir, name))
}
/// Execute the real Homebrew Formula API from a uniquely owned absolute path.
/// # Errors
/// Any of the four environment-composition assertions must fail closed.
pub fn homebrew_policy(root: &Path, formula: &Path) -> Result<String, String> {
    if !cfg!(target_os = "macos") {
        return Err("Homebrew API gate requires macOS".into());
    }
    let work = crate::scratch::directory("pgokf-formula-")?;
    let result = (|| {
        let owned_formula = work.path().join("pgokf.rb");
        std::fs::copy(formula, &owned_formula).map_err(|e| e.to_string())?;
        let mut spec = brew(root, &["ruby"]);
        // Homebrew's documented developer mode permits local formula paths.
        // An explicit operator prohibition remains authoritative.
        spec.env.insert("HOMEBREW_DEVELOPER".into(), "1".into());
        spec.args.extend([
            root.join("scripts/test-homebrew-environment.rb")
                .into_os_string(),
            owned_formula.into_os_string(),
        ]);
        let result = output(&spec)?;
        validate_homebrew_cases(&result)?;
        Ok(format!(
            "PASS: four actual formula environment composition cases\n{result}"
        ))
    })();
    work.close()
        .map_err(|e| format!("formula cleanup failed: {e}"))?;
    result
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrewCase {
    plain: Option<String>,
    encoded: Option<String>,
    rustflags: Option<String>,
    encoded_rustflags: Option<String>,
    homebrew_rustflags: Option<String>,
    deployment_target: Option<String>,
    host_target: String,
    status: String,
}
fn validate_homebrew_cases(output: &str) -> Result<(), String> {
    let cases: Vec<BrewCase> = crate::json::from_slice(output.as_bytes())?;
    let inputs = [
        (None, None),
        (Some("--cfg=existing_plain"), None),
        (
            Some("--cfg=ignored_by_cargo"),
            Some("--cfg=existing_encoded\u{1f}-Copt-level=1"),
        ),
        (None, Some("")),
    ];
    if cases.len() != inputs.len() {
        return Err("four actual formula API cases required".into());
    }
    for (case, (plain, encoded)) in cases.iter().zip(inputs) {
        if case.plain.as_deref() != plain || case.encoded.as_deref() != encoded {
            return Err("formula API input coverage changed".into());
        }
        if case.status != "environment_ready" {
            return Err("formula install command not reached".into());
        }
        if case.host_target.is_empty()
            || case.deployment_target.as_deref() != Some(&case.host_target)
        {
            return Err("host deployment target lost".into());
        }
        if case.homebrew_rustflags.as_deref() != Some("-Cdebuginfo=1") {
            return Err("Homebrew flags lost".into());
        }
        if let Some(encoded) = encoded {
            let expected = [encoded, "-Clink-arg=-Wl,-undefined,dynamic_lookup"]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\u{1f}");
            if case.encoded_rustflags.as_deref() != Some(&expected) {
                return Err("encoded flags lost".into());
            }
            if case.rustflags.as_deref() != plain {
                return Err("plain flags changed".into());
            }
        } else {
            let expected = [plain, Some("-C link-arg=-Wl,-undefined,dynamic_lookup")]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            if case.rustflags.as_deref() != Some(&expected) || case.encoded_rustflags.is_some() {
                return Err("plain flags lost".into());
            }
        }
    }
    Ok(())
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut text, byte| {
            use std::fmt::Write as _;
            write!(text, "{byte:02x}").expect("String write");
            text
        })
}
/// Exclusive durable JSON evidence; never overwrite another run's artifact.
/// # Errors
/// Existing paths, serialization or fsync failures are errors.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    file.write_all(&bytes).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    std::fs::File::open(path.parent().unwrap_or(Path::new(".")))
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())
}

fn logged(out: &Path, label: &str, spec: &CommandSpec) -> Result<(), String> {
    let result = spec.run()?;
    let mut bytes = result.stdout;
    bytes.extend(result.stderr);
    std::fs::write(out.join(format!("{label}.log")), bytes).map_err(|e| e.to_string())?;
    write_json(
        &out.join(format!("{label}.json")),
        &serde_json::json!({"program":spec.program.to_string_lossy(), "command":spec.args.iter().map(|s|s.to_string_lossy()).collect::<Vec<_>>(),"code":result.status.code()}),
    )?;
    if result.status.success() {
        Ok(())
    } else {
        Err(format!("{label}: {}", result.status))
    }
}
/// Build, audit and test a clean immutable HEAD with the real Homebrew formula.
/// Evidence and a recovery identity survive any build failure. Only the unique
/// probe formula is uninstalled; existing user installations are never touched.
/// # Errors
/// Unsupported host, dirty source, build/audit/test failure or cleanup failure fails.
#[allow(clippy::too_many_lines)] // Preserve creation/build/cleanup ordering.
pub fn homebrew_packaging(root: &Path, out: &Path) -> Result<(), String> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return Err("live packaging requires arm64 macOS".into());
    }
    std::fs::create_dir(out).map_err(|e| e.to_string())?;
    let out = out.canonicalize().map_err(|e| e.to_string())?;
    logged(
        &out,
        "clean",
        &command(root, "git", &["diff", "--exit-code", "HEAD"]),
    )?;
    let head = output(&command(root, "git", &["rev-parse", "HEAD"]))?;
    let identity = crate::release_identity::source_identity(root)?;
    let archive = out.join(format!("pgokf-{head}.tar.gz"));
    let mut spec = command(root, "git", &["archive", "--format=tar.gz"]);
    spec.args.extend([
        format!("--prefix=pgokf-{}/", identity.version).into(),
        "-o".into(),
        archive.clone().into_os_string(),
        head.clone().into(),
    ]);
    logged(&out, "archive", &spec)?;
    let sha = digest(&std::fs::read(&archive).map_err(|e| e.to_string())?);
    let mut perms = std::fs::metadata(&archive)
        .map_err(|e| e.to_string())?
        .permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&archive, perms).map_err(|e| e.to_string())?;
    let (tap, tap_name) = tap(root, "packaging")?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let probe_name = format!("pgokf-packaging-probe-{token}");
    let probe = format!("{tap_name}/{probe_name}");
    write_json(
        &out.join("identity.json"),
        &serde_json::json!({"sha":head,"sha256":sha,"archive":archive,"tap":tap_name,"probe":probe}),
    )?;
    let original = std::fs::read_to_string(root.join("packaging/homebrew/pgokf.rb"))
        .map_err(|e| e.to_string())?;
    let tuple = crate::release_identity::formula_tuple(&original)?;
    crate::release_identity::authenticate_formula(
        &tuple,
        &crate::release_identity::published_digests(),
    )?;
    let mut class_token = token.clone();
    class_token[0..1].make_ascii_uppercase();
    let local = original
        .replace(
            "class Pgokf < Formula",
            &format!("class PgokfPackagingProbe{class_token} < Formula"),
        )
        .replace(
            &format!(
                "https://github.com/LogicOcean/pgokf/archive/refs/tags/v{}.tar.gz",
                tuple.version
            ),
            &format!("file://{}", archive.display()),
        )
        .replace(
            &format!("  url \"file://{}\"", archive.display()),
            &format!(
                "  url \"file://{}\"\n  version \"{}\"",
                archive.display(),
                identity.version
            ),
        )
        .replace(&tuple.sha256, &sha)
        .replace(
            &format!("expected_control_version = \"{}\"", tuple.version),
            &format!("expected_control_version = \"{}\"", identity.version),
        )
        .replace(
            &format!("expected_extension_version = \"{}\"", tuple.version),
            &format!("expected_extension_version = \"{}\"", identity.version),
        );
    for (name, body) in [("pgokf".to_owned(), original), (probe_name.clone(), local)] {
        std::fs::write(tap.path().join(format!("Formula/{name}.rb")), &body)
            .map_err(|e| e.to_string())?;
        std::fs::write(out.join(format!("{name}.rb")), body).map_err(|e| e.to_string())?;
    }
    if !brew(root, &["list", "--versions", &probe])
        .run()?
        .stdout
        .is_empty()
    {
        return Err("probe formula was already installed; refusing ownership".into());
    }
    let result = (|| {
        let mut environment = brew(root, &["ruby"]);
        environment.args.extend([
            root.join("scripts/test-homebrew-environment.rb")
                .into_os_string(),
            format!("{tap_name}/pgokf").into(),
        ]);
        logged(&out, "environment", &environment)?;
        validate_homebrew_cases(
            &std::fs::read_to_string(out.join("environment.log")).map_err(|e| e.to_string())?,
        )?;
        logged(
            &out,
            "audit",
            &brew(root, &["audit", "--strict", &format!("{tap_name}/pgokf")]),
        )?;
        logged(
            &out,
            "install",
            &brew(
                root,
                &["install", "--build-from-source", "--verbose", &probe],
            ),
        )?;
        let prefix = std::fs::canonicalize(output(&brew(root, &["--prefix", &probe]))?)
            .map_err(|e| e.to_string())?;
        logged(
            &out,
            "installed-copy",
            &command(
                root,
                "cp",
                &[
                    "-R",
                    &prefix.to_string_lossy(),
                    &out.join("installed").to_string_lossy(),
                ],
            ),
        )?;
        let mut libraries = Vec::new();
        collect_libraries(&out.join("installed"), &mut libraries)?;
        if libraries.len() != 1 {
            return Err("expected one installed pgokf.dylib".into());
        }
        let mut macho = command(root, "otool", &["-l"]);
        macho.args.push(libraries.remove(0).into_os_string());
        logged(&out, "mach-o", &macho)?;
        logged(&out, "test", &brew(root, &["test", "--verbose", &probe]))?;
        crate::homebrew_observation::validate(
            &std::fs::read_to_string(out.join("test.log")).map_err(|e| e.to_string())?,
            &identity.version,
        )?;
        if digest(&std::fs::read(&archive).map_err(|e| e.to_string())?) != sha {
            return Err("immutable source archive changed".into());
        }
        Ok(())
    })();
    let installed = brew(root, &["list", "--versions", &probe]).run()?;
    if !installed.stdout.is_empty() {
        logged(
            &out,
            "uninstall",
            &brew(root, &["uninstall", "--force", &probe]),
        )?;
    }
    tap.close().map_err(|e| e.to_string())?;
    result
}
fn collect_libraries(path: &Path, result: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in std::fs::read_dir(path).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() {
            collect_libraries(&entry.path(), result)?;
        } else if kind.is_file() && entry.file_name() == "pgokf.dylib" {
            result.push(entry.path());
        }
    }
    Ok(())
}

/// Render every shipped Compose profile using an isolated example environment.
/// No services or deployment resources are created.
/// # Errors
/// Missing environment data or invalid Compose configuration fails.
pub fn compose_check(root: &Path) -> Result<(), String> {
    let compose = root
        .join("deploy/compose")
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let source =
        std::fs::read_to_string(compose.join(".env.example")).map_err(|e| e.to_string())?;
    let mut file = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
    for line in source.lines() {
        let replacement = line.split_once('=').filter(|(key, value)| {
            value.is_empty()
                && (*key == "POSTGRES_PASSWORD"
                    || key
                        .strip_prefix("PGOKF_")
                        .and_then(|s| s.strip_suffix("_PASSWORD"))
                        .is_some_and(|s| {
                            !s.is_empty() && s.bytes().all(|c| c.is_ascii_uppercase())
                        }))
        });
        if let Some((key, _)) = replacement {
            writeln!(file, "{key}=placeholder").map_err(|e| e.to_string())?;
        } else {
            writeln!(file, "{line}").map_err(|e| e.to_string())?;
        }
    }
    file.flush().map_err(|e| e.to_string())?;
    let mut spec = command(&compose, "docker", &["compose", "--env-file"]);
    spec.args.push(file.path().into());
    for profile in ["ops", "ingest", "tools", "ui", "mcp-http"] {
        spec.args.extend(["--profile".into(), profile.into()]);
    }
    spec.args.extend(["config".into(), "--quiet".into()]);
    spec.checked()?;
    Ok(())
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use crate::owned_docker::DockerApi;
    #[test]
    fn homebrew_requires_all_actual_api_observations() {
        let common = serde_json::json!({"homebrew_rustflags":"-Cdebuginfo=1","deployment_target":"14.0","host_target":"14.0","status":"environment_ready"});
        let cases = [
            (serde_json::Value::Null, serde_json::Value::Null, serde_json::json!("-C link-arg=-Wl,-undefined,dynamic_lookup"), serde_json::Value::Null),
            (serde_json::json!("--cfg=existing_plain"), serde_json::Value::Null, serde_json::json!("--cfg=existing_plain -C link-arg=-Wl,-undefined,dynamic_lookup"), serde_json::Value::Null),
            (serde_json::json!("--cfg=ignored_by_cargo"), serde_json::json!("--cfg=existing_encoded\u{1f}-Copt-level=1"), serde_json::json!("--cfg=ignored_by_cargo"), serde_json::json!("--cfg=existing_encoded\u{1f}-Copt-level=1\u{1f}-Clink-arg=-Wl,-undefined,dynamic_lookup")),
            (serde_json::Value::Null, serde_json::json!(""), serde_json::Value::Null, serde_json::json!("-Clink-arg=-Wl,-undefined,dynamic_lookup")),
        ].map(|(plain,encoded,rustflags,encoded_rustflags)| {
            let mut value = common.clone();
            value["plain"]=plain; value["encoded"]=encoded; value["rustflags"]=rustflags; value["encoded_rustflags"]=encoded_rustflags; value
        });
        validate_homebrew_cases(&serde_json::to_string(&cases).unwrap()).unwrap();
        assert!(validate_homebrew_cases(&serde_json::to_string(&cases[..3]).unwrap()).is_err());
        for (index, key) in [
            (0, "plain"),
            (0, "status"),
            (0, "deployment_target"),
            (0, "homebrew_rustflags"),
            (0, "rustflags"),
            (2, "encoded_rustflags"),
            (2, "rustflags"),
            (3, "encoded"),
        ] {
            let mut bad = cases.clone();
            bad[index][key] = serde_json::json!("changed");
            assert!(
                validate_homebrew_cases(&serde_json::to_string(&bad).unwrap()).is_err(),
                "{index}:{key}"
            );
        }
    }
    struct Images(Vec<serde_json::Value>);
    impl DockerApi for Images {
        fn call(&mut self, args: &[String]) -> Result<String, String> {
            if args.len() != 3 || args[..2] != ["image", "inspect"] {
                return Err("unexpected command".into());
            }
            let image = self
                .0
                .iter()
                .find(|v| v["Id"] == args[2])
                .ok_or("unknown image")?;
            serde_json::to_string(&vec![image]).map_err(|e| e.to_string())
        }
    }
    fn inspect(image: &ImageManifest) -> serde_json::Value {
        serde_json::json!({"Id":image.image_id,"Config":{"Labels":{
            "org.opencontainers.image.version":image.source.version,
            "org.opencontainers.image.revision":image.source_commit,
            "org.pgokf.verifier.tooling-revision":image.tooling_commit,
            "org.pgokf.verifier.source-contract":digest(&serde_json::to_vec(&image.source).unwrap()),
            "org.pgokf.verifier.postgres":image.postgres_image_tag,
        }}})
    }
    #[test]
    fn rollback_provenance_is_bound_to_git_and_inspected_image() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let head = output(&command(&root, "git", &["rev-parse", "HEAD"])).unwrap();
        let archived = source_archive(&root, &head).unwrap();
        let from = ImageManifest {
            source: crate::release_identity::source_identity(archived.path()).unwrap(),
            source_commit: head.clone(),
            image_id: format!("sha256:{}", "a".repeat(64)),
            postgres_major: 18,
            postgres_image_tag: "18".into(),
            tooling_commit: head.clone(),
        };
        let previous = source_archive(&root, HISTORY[0].2).unwrap();
        let to = ImageManifest {
            source: crate::release_identity::source_identity_version(previous.path(), "0.2.0")
                .unwrap(),
            source_commit: HISTORY[0].2.into(),
            image_id: format!("sha256:{}", "b".repeat(64)),
            postgres_major: 18,
            postgres_image_tag: "18".into(),
            tooling_commit: head,
        };
        // Post-tag, the in-tree formula consumes the current release; rollback
        // targets the last verified published 0.2.0 tuple.
        let formula = crate::release_identity::FormulaTuple {
            version: "0.2.0".into(),
            sha256: crate::release_identity::PUBLISHED_DIGEST.into(),
            assertions: vec!["0.2.0".into(); 2],
        };
        let contract = RollbackManifest {
            schema_version: 1,
            from,
            to,
            previous_formula: formula,
        };
        let values = vec![inspect(&contract.from), inspect(&contract.to)];
        validate_rollback_at(&root, &mut Images(values.clone()), &contract).unwrap();
        for key in [
            "org.opencontainers.image.revision",
            "org.opencontainers.image.version",
            "org.pgokf.verifier.tooling-revision",
            "org.pgokf.verifier.source-contract",
            "org.pgokf.verifier.postgres",
        ] {
            let mut bad = values.clone();
            bad[1]["Config"]["Labels"][key] = serde_json::json!("forged");
            assert!(
                validate_rollback_at(&root, &mut Images(bad), &contract)
                    .unwrap_err()
                    .contains("provenance mismatch"),
                "{key}"
            );
        }
        let mut forged = contract.clone();
        forged.to.source.packages.insert("invented".into());
        let correlated = vec![inspect(&forged.from), inspect(&forged.to)];
        assert!(
            validate_rollback_at(&root, &mut Images(correlated), &forged)
                .unwrap_err()
                .contains("immutable Git inventory")
        );
    }
}
