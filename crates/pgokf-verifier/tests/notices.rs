use base64::Engine as _;
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use pgokf_verifier::notices;
use serde_json::json;
use sha2::{Digest, Sha256, Sha512};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::sync::{Mutex, MutexGuard};
use std::{fmt::Write as _, fs, path::Path};

/// Serialize every execution of the authenticated generator.
///
/// `notices::render_with_identity_and_provenance` copies the generator to a
/// fresh tempfile, keeps it open for writing, closes it, and immediately
/// execve's it. On Linux a forked child inherits that write-open file
/// descriptor until the child itself execve's (close-on-exec only fires at
/// the child's own exec), so the inode stays write-open even after the
/// owner closes it and the immediate execve returns ETXTBSY ("Text file
/// busy", os error 26) whenever it wins the race against another test
/// thread's spawn child. macOS never shows the race, which is why the suite
/// is green there but flaky in CI. Every exec in this suite goes through
/// [`render_fixture`] or the gated production call below, so holding one
/// gate across a full generator execution removes every concurrent fork
/// source and makes the suite deterministic in parallel on Linux; the
/// fixture trees are per-test tempdirs, so nothing else is shared.
static EXEC_GATE: Mutex<()> = Mutex::new(());

fn exec_gate() -> MutexGuard<'static, ()> {
    match EXEC_GATE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing String cannot fail");
            output
        })
}

fn source_content_digest(rows: &[(&str, &str)]) -> String {
    let mut hasher = Sha256::new();
    for (path, content) in rows {
        for bytes in [path.as_bytes(), content.as_bytes()] {
            hasher.update(u64::try_from(bytes.len()).unwrap().to_be_bytes());
            hasher.update(bytes);
        }
    }
    hasher
        .finalize()
        .iter()
        .fold(String::new(), |mut output, byte| {
            write!(output, "{byte:02x}").unwrap();
            output
        })
}

fn npm_tarball_with_typed_and_directory(
    name: &str,
    version: &str,
    spdx: &str,
    license: &[u8],
    bundle: &[u8],
    extras: &[(&str, &str, u8, &[u8])],
    repository_directory: Option<&str>,
) -> Vec<u8> {
    fn append(output: &mut Vec<u8>, prefix: &str, name: &str, kind: u8, bytes: &[u8]) {
        let mut header = [0_u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size = format!("{:011o}\0", bytes.len());
        header[124..136].copy_from_slice(size.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].fill(b' ');
        header[156] = kind;
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let sum: usize = header.iter().map(|byte| usize::from(*byte)).sum();
        let checksum = format!("{sum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());
        output.extend_from_slice(&header);
        output.extend_from_slice(bytes);
        output.resize(output.len().div_ceil(512) * 512, 0);
    }
    let repository = if name == "example" {
        "https://example.invalid/project".to_owned()
    } else {
        format!("https://example.invalid/{name}")
    };
    let repository_json = repository_directory.map_or_else(
        || format!(r#""{repository}""#),
        |directory| format!(r#"{{"type":"git","url":"{repository}","directory":"{directory}"}}"#),
    );
    let package = format!(
        r#"{{"name":"{name}","version":"{version}","license":"{spdx}","repository":{repository_json}}}"#
    );
    let mut tar = Vec::new();
    append(
        &mut tar,
        "",
        "package/package.json",
        b'0',
        package.as_bytes(),
    );
    append(&mut tar, "", "package/LICENSE", b'0', license);
    append(&mut tar, "", "package/dist/bundle.min.js", b'0', bundle);
    for (prefix, path, kind, bytes) in extras {
        append(&mut tar, prefix, path, *kind, bytes);
    }
    tar.extend_from_slice(&[0_u8; 1024]);
    let mut gzip = GzEncoder::new(Vec::new(), Compression::best());
    std::io::Write::write_all(&mut gzip, &tar).unwrap();
    gzip.finish().unwrap()
}

fn npm_tarball_with_typed(
    name: &str,
    version: &str,
    spdx: &str,
    license: &[u8],
    bundle: &[u8],
    extras: &[(&str, &str, u8, &[u8])],
) -> Vec<u8> {
    npm_tarball_with_typed_and_directory(name, version, spdx, license, bundle, extras, None)
}

fn npm_tarball_with(
    name: &str,
    version: &str,
    spdx: &str,
    license: &[u8],
    bundle: &[u8],
    extras: &[(&str, &[u8])],
) -> Vec<u8> {
    let extras = extras
        .iter()
        .map(|(path, bytes)| ("", *path, b'0', *bytes))
        .collect::<Vec<_>>();
    npm_tarball_with_typed(name, version, spdx, license, bundle, &extras)
}

fn npm_tarball(name: &str, version: &str, spdx: &str, license: &[u8], bundle: &[u8]) -> Vec<u8> {
    npm_tarball_with(name, version, spdx, license, bundle, &[])
}

fn integrity(bytes: &[u8]) -> String {
    format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes))
    )
}

fn replace_tarball_identity(manifest: &str, old: &[u8], new: &[u8]) -> String {
    manifest
        .replace(&digest(old), &digest(new))
        .replace(&integrity(old), &integrity(new))
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("crates/pgokf-web/static/vendor/licenses")).unwrap();
    fs::create_dir_all(root.path().join("crates/pgokf-web/src")).unwrap();
    fs::create_dir_all(root.path().join("evidence")).unwrap();
    fs::write(root.path().join("Cargo.lock"), "fixture\n").unwrap();
    fs::write(root.path().join("about.toml"), "accepted = [\"MIT\"]\n").unwrap();
    fs::write(root.path().join("about.hbs"), "fixture\n").unwrap();
    let asset = b"bundle bytes\n";
    let license = b"Exact upstream license text\n";
    let tarball = npm_tarball("example", "1.2.3", "MIT", license, asset);
    fs::write(root.path().join("evidence/example-1.2.3.tgz"), &tarball).unwrap();
    fs::write(
        root.path()
            .join("crates/pgokf-web/static/vendor/example.min.js"),
        asset,
    )
    .unwrap();
    fs::write(
        root.path()
            .join("crates/pgokf-web/static/vendor/licenses/example-LICENSE.txt"),
        license,
    )
    .unwrap();
    fs::write(
        root.path().join("crates/pgokf-web/src/routes.rs"),
        "const X: &str = include_str!(\"../static/vendor/example.min.js\");\n",
    )
    .unwrap();
    let manifest = format!(
        r#"schema_version = 1

[[asset]]
name = "example"
version = "1.2.3"
spdx = "MIT"
source = "https://example.invalid/project"
path = "crates/pgokf-web/static/vendor/example.min.js"
tarball_member = "package/dist/bundle.min.js"
sha256 = "{}"

[[asset.component]]
name = "example"
version = "1.2.3"
spdx = "MIT"
source = "https://example.invalid/project"
source_count = 1
tarball = "https://example.invalid/example-1.2.3.tgz"
tarball_sha256 = "{}"
tarball_path = "evidence/example-1.2.3.tgz"
integrity = "{}"
license_path = "crates/pgokf-web/static/vendor/licenses/example-LICENSE.txt"
license_member = "package/LICENSE"
license_sha256 = "{}"
"#,
        digest(asset),
        digest(&tarball),
        integrity(&tarball),
        digest(license)
    );
    fs::write(root.path().join("vendored-notices.toml"), manifest).unwrap();
    fs::write(
        root.path().join("fixture-provenance.json"),
        fixture_provenance(root.path()),
    )
    .unwrap();

    let fake = root.path().join("fake-cargo-about");
    fs::write(
        &fake,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'fake-cargo-about 1.0\\n'; exit 0; fi\nprintf 'Rust notices   \\n'\n",
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    (root, fake)
}

fn fixture_provenance(root: &Path) -> String {
    let asset_path = "crates/pgokf-web/static/vendor/example.min.js";
    let tarball_path = "evidence/example-1.2.3.tgz";
    let license_path = "crates/pgokf-web/static/vendor/licenses/example-LICENSE.txt";
    let tarball = fs::read(root.join(tarball_path)).unwrap();
    json!({
        "schema_version": 1,
        "assets": [{
            "name": "example",
            "version": "1.2.3",
            "spdx": "MIT",
            "source": "https://example.invalid/project",
            "path": asset_path,
            "tarball_member": "package/dist/bundle.min.js",
            "asset_sha256": digest(&fs::read(root.join(asset_path)).unwrap()),
            "npm_tarball_sha256": digest(&tarball),
            "source_map_sha256": null,
            "lockfile_sha256": null,
            "components": [{
                "name": "example",
                "npm_name": null,
                "version": "1.2.3",
                "spdx": "MIT",
                "source": "https://example.invalid/project",
                "tarball": "https://example.invalid/example-1.2.3.tgz",
                "tarball_sha256": digest(&tarball),
                "integrity": integrity(&tarball),
                "license_sha256": digest(&fs::read(root.join(license_path)).unwrap()),
                "selectors": [],
                "source_content_sha256": digest(b"")
            }]
        }]
    })
    .to_string()
}

fn render_fixture(root: &Path, fake: &Path) -> Result<Vec<u8>, String> {
    let _gate = exec_gate();
    let sha256 = digest(&fs::read(fake).unwrap());
    let provenance = fs::read_to_string(root.join("fixture-provenance.json"))
        .map_err(|error| error.to_string())?;
    notices::render_with_identity_and_provenance(
        root,
        fake,
        &sha256,
        "fake-cargo-about 1.0",
        &provenance,
    )
}

fn verify_fixture(root: &Path, fake: &Path) -> Result<(), String> {
    let expected =
        fs::read(root.join("THIRD-PARTY-NOTICES.txt")).map_err(|error| error.to_string())?;
    let actual = render_fixture(root, fake)?;
    if expected != actual {
        return Err("THIRD-PARTY-NOTICES.txt is stale".into());
    }
    Ok(())
}

#[test]
fn deterministic_render_binds_assets_components_and_exact_license_text() {
    let (root, fake) = fixture();
    let first = render_fixture(root.path(), &fake).unwrap();
    let second = render_fixture(root.path(), &fake).unwrap();
    assert_eq!(first, second);
    let text = String::from_utf8(first.clone()).unwrap();
    assert!(text.starts_with("Rust notices\n"));
    assert!(text.contains("example 1.2.3 (MIT)"));
    assert!(text.contains("Bundle SHA-256:"));
    assert!(text.contains("Exact upstream license text"));
    assert!(text.ends_with('\n'));
    assert!(!text.ends_with("\n\n"));
    fs::write(root.path().join("THIRD-PARTY-NOTICES.txt"), first).unwrap();
    verify_fixture(root.path(), &fake).unwrap();
}

#[test]
fn production_render_rejects_an_unpinned_generator() {
    let (root, fake) = fixture();
    let error = {
        let _gate = exec_gate();
        notices::render(root.path(), &fake)
    }
    .unwrap_err();
    // Both rejections fail closed: on platforms with a pinned generator the
    // fixture's identity mismatch is detected, on unpinned platforms (e.g.
    // linux aarch64 dev containers) the missing pin rejects the same call.
    assert!(
        error.contains("cargo-about SHA-256 mismatch")
            || error.contains("cargo-about identity is not pinned"),
        "{error}"
    );
}

#[test]
fn standalone_asset_identity_is_bound_to_independent_provenance() {
    let (root, fake) = fixture();
    let path = root.path().join("vendored-notices.toml");
    let manifest = fs::read_to_string(&path).unwrap();
    fs::write(
        path,
        manifest.replacen(
            "source = \"https://example.invalid/project\"",
            "source = \"https://attacker.invalid/rebound\"",
            1,
        ),
    )
    .unwrap();
    let error = render_fixture(root.path(), &fake).unwrap_err();
    assert!(error.contains("independent upstream provenance"), "{error}");
}

#[test]
fn npm_repository_directory_is_part_of_component_identity() {
    let (root, fake) = fixture();
    let archive_path = root.path().join("evidence/example-1.2.3.tgz");
    let original = fs::read(&archive_path).unwrap();
    let changed = npm_tarball_with_typed_and_directory(
        "example",
        "1.2.3",
        "MIT",
        b"Exact upstream license text\n",
        b"bundle bytes\n",
        &[],
        Some("packages/example"),
    );
    fs::write(&archive_path, &changed).unwrap();
    for relative in ["vendored-notices.toml", "fixture-provenance.json"] {
        let path = root.path().join(relative);
        let contents = fs::read_to_string(&path).unwrap();
        fs::write(
            path,
            replace_tarball_identity(&contents, &original, &changed),
        )
        .unwrap();
    }
    let error = render_fixture(root.path(), &fake).unwrap_err();
    assert!(error.contains("repository"), "{error}");
}

#[test]
fn asset_mutation_and_manifest_hash_rebind_fail_against_authenticated_tarball() {
    let (root, fake) = fixture();
    let rendered = render_fixture(root.path(), &fake).unwrap();
    fs::write(root.path().join("THIRD-PARTY-NOTICES.txt"), rendered).unwrap();
    let changed = b"changed bundle bytes\n";
    fs::write(
        root.path()
            .join("crates/pgokf-web/static/vendor/example.min.js"),
        changed,
    )
    .unwrap();
    let path = root.path().join("vendored-notices.toml");
    let manifest = fs::read_to_string(&path).unwrap();
    let old = digest(b"bundle bytes\n");
    fs::write(path, manifest.replace(&old, &digest(changed))).unwrap();
    let error = verify_fixture(root.path(), &fake).unwrap_err();
    assert!(
        error.contains("independent upstream provenance")
            || error.contains("authenticated npm tarball"),
        "{error}"
    );
}

#[test]
fn license_mutation_and_manifest_hash_rebind_fail_against_authenticated_tarball() {
    let (root, fake) = fixture();
    let changed = b"Substituted license text\n";
    fs::write(
        root.path()
            .join("crates/pgokf-web/static/vendor/licenses/example-LICENSE.txt"),
        changed,
    )
    .unwrap();
    let path = root.path().join("vendored-notices.toml");
    let manifest = fs::read_to_string(&path).unwrap();
    fs::write(
        path,
        manifest.replace(&digest(b"Exact upstream license text\n"), &digest(changed)),
    )
    .unwrap();
    let error = render_fixture(root.path(), &fake).unwrap_err();
    assert!(
        error.contains("independent upstream provenance")
            || error.contains("authenticated tarball"),
        "{error}"
    );
}

#[test]
fn nondeterministic_generator_hash_mismatch_and_unknown_manifest_fields_fail_closed() {
    let (root, fake) = fixture();
    let counter = root.path().join("counter");
    fs::write(
        &fake,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'fake-cargo-about 1.0\\n'; exit 0; fi\nn=0; [ -f '{}' ] && n=$(cat '{}'); n=$((n+1)); printf '%s' \"$n\" > '{}'; printf 'run %s\\n' \"$n\"\n",
            counter.display(),
            counter.display(),
            counter.display()
        ),
    )
    .unwrap();
    assert!(
        render_fixture(root.path(), &fake)
            .unwrap_err()
            .contains("deterministic")
    );

    let (root, fake) = fixture();
    fs::write(
        root.path()
            .join("crates/pgokf-web/static/vendor/example.min.js"),
        "wrong",
    )
    .unwrap();
    assert!(
        render_fixture(root.path(), &fake)
            .unwrap_err()
            .contains("SHA-256")
    );

    let (root, fake) = fixture();
    let path = root.path().join("vendored-notices.toml");
    let mut manifest = fs::read_to_string(&path).unwrap();
    manifest.push_str("unknown = true\n");
    fs::write(path, manifest).unwrap();
    assert!(render_fixture(root.path(), &fake).is_err());
}

#[test]
fn missing_or_extra_vendor_files_and_unembedded_assets_fail() {
    let (root, fake) = fixture();
    fs::write(
        root.path()
            .join("crates/pgokf-web/static/vendor/extra.min.js"),
        "extra",
    )
    .unwrap();
    assert!(
        render_fixture(root.path(), &fake)
            .unwrap_err()
            .contains("inventory")
    );

    let (root, fake) = fixture();
    fs::write(root.path().join("crates/pgokf-web/src/routes.rs"), "").unwrap();
    assert!(
        render_fixture(root.path(), &fake)
            .unwrap_err()
            .contains("served JavaScript")
    );

    let (root, fake) = fixture();
    fs::write(
        root.path().join("crates/pgokf-web/src/other.rs"),
        "const B: &str = include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/static/app.js\"));\n",
    )
    .unwrap();
    assert!(
        render_fixture(root.path(), &fake)
            .unwrap_err()
            .contains("served JavaScript")
    );
}

#[test]
fn path_qualified_and_aliased_include_macros_are_classified() {
    for source in [
        concat!(
            "const X: &str = include_str!(\"../static/vendor/example.min.js\");\n",
            "const Q: &str = std::include_str!(\"../static/qualified.js\");\n"
        ),
        concat!(
            "use std::include_str as embedded_text;\n",
            "const X: &str = include_str!(\"../static/vendor/example.min.js\");\n",
            "const Q: &str = embedded_text!(\"../static/qualified.js\");\n"
        ),
        concat!(
            "const X: &str = include_str!(\"../static/vendor/example.min.js\");\n",
            "const Q: &str = embedded_text!(\"../static/qualified.js\");\n",
            "use std::include_str as embedded_text;\n"
        ),
        concat!(
            "use std::include_str as first_alias;\n",
            "use first_alias as embedded_text;\n",
            "const X: &str = include_str!(\"../static/vendor/example.min.js\");\n",
            "const Q: &str = embedded_text!(\"../static/qualified.js\");\n"
        ),
        concat!(
            "macro_rules! embed_js { ($path:literal) => { include_str!($path) }; }\n",
            "const X: &str = include_str!(\"../static/vendor/example.min.js\");\n",
            "const Q: &str = embed_js!(\"../static/qualified.js\");\n"
        ),
    ] {
        let (root, fake) = fixture();
        fs::write(
            root.path().join("crates/pgokf-web/static/qualified.js"),
            "extra",
        )
        .unwrap();
        fs::write(root.path().join("crates/pgokf-web/src/routes.rs"), source).unwrap();
        let error = render_fixture(root.path(), &fake).unwrap_err();
        assert!(
            error.contains("served JavaScript") || error.contains("embedded include macro wrapper"),
            "{error}"
        );
    }
}

#[test]
fn cross_file_and_transitive_include_wrappers_are_classified() {
    let cases = [
        (
            "cross-file",
            Some(
                "#[macro_export]\nmacro_rules! embed_js { ($path:literal) => { include_str!($path) }; }\n",
            ),
            "const X: &str = include_str!(\"../static/vendor/example.min.js\");\nconst Q: &str = crate::embed_js!(\"../static/qualified.js\");\n",
        ),
        (
            "transitive",
            None,
            concat!(
                "macro_rules! embed_js { ($path:literal) => { include_str!($path) }; }\n",
                "macro_rules! embed_js_again { ($path:literal) => { embed_js!($path) }; }\n",
                "const X: &str = include_str!(\"../static/vendor/example.min.js\");\n",
                "const Q: &str = embed_js_again!(\"../static/qualified.js\");\n"
            ),
        ),
    ];
    for (case, macros, routes) in cases {
        let (root, fake) = fixture();
        fs::write(root.path().join("crates/pgokf-web/src/routes.rs"), routes).unwrap();
        if let Some(macros) = macros {
            fs::write(root.path().join("crates/pgokf-web/src/macros.rs"), macros).unwrap();
        }
        fs::write(
            root.path().join("crates/pgokf-web/static/qualified.js"),
            b"qualified\n",
        )
        .unwrap();
        let error = render_fixture(root.path(), &fake).unwrap_err();
        assert!(
            error.contains("served JavaScript") || error.contains("embedded include macro wrapper"),
            "{case}: {error}"
        );
    }
}

#[test]
fn every_tar_member_is_validated_after_archive_identity_rebind() {
    for (case, prefix, path, kind, bytes) in [
        ("traversal", "", "../../escape", b'0', b"hostile".as_slice()),
        ("absolute", "", "/absolute", b'0', b"hostile".as_slice()),
        (
            "backslash",
            "",
            r"package\escape",
            b'0',
            b"hostile".as_slice(),
        ),
        (
            "duplicate",
            "",
            "package/LICENSE",
            b'0',
            b"duplicate".as_slice(),
        ),
        (
            "canonical-duplicate",
            "",
            "package//LICENSE",
            b'0',
            b"duplicate".as_slice(),
        ),
        ("symlink", "", "package/link", b'2', b"target".as_slice()),
        ("special", "", "package/device", b'3', b"".as_slice()),
        ("ustar-prefix", "..", "escape", b'0', b"hostile".as_slice()),
    ] {
        let (root, fake) = fixture();
        let archive_path = root.path().join("evidence/example-1.2.3.tgz");
        let original = fs::read(&archive_path).unwrap();
        let changed = npm_tarball_with_typed(
            "example",
            "1.2.3",
            "MIT",
            b"Exact upstream license text\n",
            b"bundle bytes\n",
            &[(prefix, path, kind, bytes)],
        );
        fs::write(&archive_path, &changed).unwrap();
        let manifest_path = root.path().join("vendored-notices.toml");
        let manifest = fs::read_to_string(&manifest_path).unwrap();
        fs::write(
            manifest_path,
            replace_tarball_identity(&manifest, &original, &changed),
        )
        .unwrap();
        let provenance_path = root.path().join("fixture-provenance.json");
        let provenance = fs::read_to_string(&provenance_path).unwrap();
        fs::write(
            provenance_path,
            replace_tarball_identity(&provenance, &original, &changed),
        )
        .unwrap();
        let error = render_fixture(root.path(), &fake).unwrap_err();
        assert!(error.contains("npm tarball"), "{case}: {error}");
    }

    let (root, fake) = fixture();
    let archive_path = root.path().join("evidence/example-1.2.3.tgz");
    let original = fs::read(&archive_path).unwrap();
    let mut decoded = Vec::new();
    std::io::Read::read_to_end(&mut GzDecoder::new(original.as_slice()), &mut decoded).unwrap();
    decoded.extend_from_slice(b"nonzero trailer");
    let mut gzip = GzEncoder::new(Vec::new(), Compression::best());
    std::io::Write::write_all(&mut gzip, &decoded).unwrap();
    let changed = gzip.finish().unwrap();
    fs::write(&archive_path, &changed).unwrap();
    for relative in ["vendored-notices.toml", "fixture-provenance.json"] {
        let path = root.path().join(relative);
        let contents = fs::read_to_string(&path).unwrap();
        fs::write(
            path,
            replace_tarball_identity(&contents, &original, &changed),
        )
        .unwrap();
    }
    let error = render_fixture(root.path(), &fake).unwrap_err();
    assert!(error.contains("npm tarball"), "nonzero-trailer: {error}");
}

#[test]
#[allow(clippy::too_many_lines)] // One synthetic archive/source-map mutation boundary.
fn component_removal_and_identity_rebind_fail_against_authenticated_upstream_evidence() {
    let (root, _) = fixture();
    fs::create_dir_all(root.path().join("evidence")).unwrap();
    let map = br#"{"version":3,"file":"example.js","sources":["../node_modules/dep/index.js","../src/index.js"],"sourcesContent":["dep","root"],"names":[],"mappings":""}"#;
    let dep_license = b"Dependency license\n";
    let asset = b"bundle bytes\n";
    let root_license = b"Exact upstream license text\n";
    let dep_tarball = npm_tarball("dep", "2.0.0", "MIT", dep_license, b"dep bundle");
    let root_tarball = npm_tarball_with(
        "example",
        "1.2.3",
        "MIT",
        root_license,
        asset,
        &[("package/dist/example.js.map", map)],
    );
    let dep_integrity = integrity(&dep_tarball);
    let lock = format!(
        "dep@^2.0.0:\n  version \"2.0.0\"\n  resolved \"https://registry.yarnpkg.com/dep/-/dep-2.0.0.tgz\"\n  integrity {dep_integrity}\n"
    );
    fs::write(root.path().join("evidence/example.js.map"), map).unwrap();
    fs::write(root.path().join("evidence/yarn.lock"), &lock).unwrap();
    fs::write(root.path().join("evidence/dep-2.0.0.tgz"), &dep_tarball).unwrap();
    fs::write(
        root.path().join("evidence/example-1.2.3.tgz"),
        &root_tarball,
    )
    .unwrap();
    fs::write(
        root.path()
            .join("crates/pgokf-web/static/vendor/licenses/dep-LICENSE.txt"),
        dep_license,
    )
    .unwrap();
    let full = format!(
        r#"schema_version = 1

[[asset]]
name = "example"
version = "1.2.3"
spdx = "MIT"
source = "https://example.invalid/project"
path = "crates/pgokf-web/static/vendor/example.min.js"
tarball_member = "package/dist/bundle.min.js"
sha256 = "{}"

[asset.inventory]
basis = "fixture"
npm_tarball = "https://example.invalid/example.tgz"
npm_tarball_sha256 = "{}"
source_map = "https://example.invalid/example.js.map"
source_map_file = "example.js"
source_map_member = "package/dist/example.js.map"
source_map_path = "evidence/example.js.map"
source_map_sha256 = "{}"
lockfile_path = "evidence/yarn.lock"
lockfile_sha256 = "{}"
source_count = 2
root_source_count = 1
embedded_source_count = 1
embedded_component_count = 1

[[asset.component]]
name = "dep"
version = "2.0.0"
spdx = "MIT"
source = "https://example.invalid/dep"
source_count = 1
tarball = "https://example.invalid/dep.tgz"
tarball_sha256 = "{}"
tarball_path = "evidence/dep-2.0.0.tgz"
integrity = "{}"
license_path = "crates/pgokf-web/static/vendor/licenses/dep-LICENSE.txt"
license_member = "package/LICENSE"
license_sha256 = "{}"

[[asset.component]]
name = "example"
version = "1.2.3"
spdx = "MIT"
source = "https://example.invalid/project"
source_count = 1
tarball = "https://example.invalid/example.tgz"
tarball_sha256 = "{}"
tarball_path = "evidence/example-1.2.3.tgz"
integrity = "{}"
license_path = "crates/pgokf-web/static/vendor/licenses/example-LICENSE.txt"
license_member = "package/LICENSE"
license_sha256 = "{}"
"#,
        digest(asset),
        digest(&root_tarball),
        digest(map),
        digest(lock.as_bytes()),
        digest(&dep_tarball),
        dep_integrity,
        digest(dep_license),
        digest(&root_tarball),
        integrity(&root_tarball),
        digest(root_license),
    );
    let provenance = json!({
        "schema_version": 1,
        "assets": [{
            "name": "example",
            "version": "1.2.3",
            "spdx": "MIT",
            "source": "https://example.invalid/project",
            "path": "crates/pgokf-web/static/vendor/example.min.js",
            "tarball_member": "package/dist/bundle.min.js",
            "asset_sha256": digest(asset),
            "npm_tarball_sha256": digest(&root_tarball),
            "source_map_sha256": digest(map),
            "lockfile_sha256": digest(lock.as_bytes()),
            "components": [
                {
                    "name": "dep",
                    "npm_name": null,
                    "version": "2.0.0",
                    "spdx": "MIT",
                    "source": "https://example.invalid/dep",
                    "tarball": "https://example.invalid/dep.tgz",
                    "tarball_sha256": digest(&dep_tarball),
                    "integrity": dep_integrity,
                    "license_sha256": digest(dep_license),
                    "selectors": ["dep@^2.0.0"],
                    "source_content_sha256": source_content_digest(&[("../node_modules/dep/index.js", "dep")])
                },
                {
                    "name": "example",
                    "npm_name": null,
                    "version": "1.2.3",
                    "spdx": "MIT",
                    "source": "https://example.invalid/project",
                    "tarball": "https://example.invalid/example.tgz",
                    "tarball_sha256": digest(&root_tarball),
                    "integrity": integrity(&root_tarball),
                    "license_sha256": digest(root_license),
                    "selectors": [],
                    "source_content_sha256": source_content_digest(&[("../src/index.js", "root")])
                }
            ]
        }]
    })
    .to_string();
    fs::write(root.path().join("vendored-notices.toml"), &full).unwrap();
    notices::render_vendored_with_provenance(root.path(), &provenance).unwrap();

    let mut wrong_content: serde_json::Value = serde_json::from_str(&provenance).unwrap();
    wrong_content["assets"][0]["components"][0]["source_content_sha256"] =
        json!(digest(b"different mapped source content"));
    let error = notices::render_vendored_with_provenance(
        root.path(),
        &serde_json::to_string(&wrong_content).unwrap(),
    )
    .unwrap_err();
    assert!(error.contains("authenticated source map"), "{error}");

    let incompatible_lock = format!(
        "dep@^3.0.0:\n  version \"2.0.0\"\n  resolved \"https://registry.yarnpkg.com/dep/-/dep-2.0.0.tgz\"\n  integrity {dep_integrity}\n"
    );
    fs::write(root.path().join("evidence/yarn.lock"), &incompatible_lock).unwrap();
    fs::write(
        root.path().join("vendored-notices.toml"),
        full.replace(
            &digest(lock.as_bytes()),
            &digest(incompatible_lock.as_bytes()),
        ),
    )
    .unwrap();
    let mut incompatible_provenance: serde_json::Value = serde_json::from_str(&provenance).unwrap();
    incompatible_provenance["assets"][0]["lockfile_sha256"] =
        json!(digest(incompatible_lock.as_bytes()));
    incompatible_provenance["assets"][0]["components"][0]["selectors"] = json!(["dep@^3.0.0"]);
    let error = notices::render_vendored_with_provenance(
        root.path(),
        &serde_json::to_string(&incompatible_provenance).unwrap(),
    )
    .unwrap_err();
    assert!(error.contains("selector"), "{error}");
    fs::write(root.path().join("evidence/yarn.lock"), &lock).unwrap();
    fs::write(root.path().join("vendored-notices.toml"), &full).unwrap();

    let dep_start = full.find("[[asset.component]]\nname = \"dep\"").unwrap();
    let root_start = full[dep_start + 1..]
        .find("[[asset.component]]\nname = \"example\"")
        .map(|offset| dep_start + 1 + offset)
        .unwrap();
    let mut rebound = format!("{}{}", &full[..dep_start], &full[root_start..]);
    rebound = rebound
        .replace("source_count = 2", "source_count = 1")
        .replace("embedded_source_count = 1", "embedded_source_count = 0")
        .replace(
            "embedded_component_count = 1",
            "embedded_component_count = 0",
        );
    fs::write(root.path().join("vendored-notices.toml"), rebound).unwrap();
    let error = notices::render_vendored_with_provenance(root.path(), &provenance).unwrap_err();
    assert!(
        error.contains("provenance") || error.contains("authenticated source map"),
        "{error}"
    );

    let changed_license = b"Substituted dependency license\n";
    let changed_tarball = npm_tarball("dep", "9.9.9", "MIT", changed_license, b"replacement");
    let changed_integrity = integrity(&changed_tarball);
    let changed_lock = format!(
        "dep@^2.0.0:\n  version \"9.9.9\"\n  resolved \"https://registry.yarnpkg.com/dep/-/dep-9.9.9.tgz\"\n  integrity {changed_integrity}\n"
    );
    fs::write(root.path().join("evidence/yarn.lock"), &changed_lock).unwrap();
    fs::write(root.path().join("evidence/dep-2.0.0.tgz"), &changed_tarball).unwrap();
    fs::write(
        root.path()
            .join("crates/pgokf-web/static/vendor/licenses/dep-LICENSE.txt"),
        changed_license,
    )
    .unwrap();
    let rebound = full
        .replace("version = \"2.0.0\"", "version = \"9.9.9\"")
        .replace(
            "https://example.invalid/dep",
            "https://attacker.invalid/not-the-source",
        )
        .replace(&digest(lock.as_bytes()), &digest(changed_lock.as_bytes()))
        .replace(&digest(&dep_tarball), &digest(&changed_tarball))
        .replace(&dep_integrity, &changed_integrity)
        .replace(&digest(dep_license), &digest(changed_license));
    fs::write(root.path().join("vendored-notices.toml"), rebound).unwrap();
    let error = notices::render_vendored_with_provenance(root.path(), &provenance).unwrap_err();
    assert!(
        error.contains("provenance") || error.contains("selector") || error.contains("source"),
        "{error}"
    );
}

#[test]
fn repository_manifest_renders_with_complete_force_graph_inventory() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let appendix = notices::render_vendored(&root).unwrap();
    let text = String::from_utf8(appendix).unwrap();
    assert!(text.contains("3d-force-graph 1.80.0 (MIT)"));
    assert!(text.contains(
        "Bundle SHA-256: d96e738edcca580edd524730c1c6b05ed2efce028c23ca95db1bf43033a72e42"
    ));
    assert!(text.contains(
        "Source-map SHA-256: a631ad2b13b8acb5b90ca3acbbfb963a5281f3d03e973ccbd8fd730c23e9f514"
    ));
    assert!(text.contains("Embedded component inventory (30 packages, 171 mapped sources)"));
    assert_eq!(text.matches("\n  Component: ").count(), 32);
}
