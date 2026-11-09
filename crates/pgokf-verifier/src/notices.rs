//! Deterministic, manifest-bound third-party notice rendering.
use base64::Engine as _;
use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256, Sha512};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs,
    io::Read as _,
    path::{Component as PathComponent, Path, PathBuf},
    process::Command,
};
use syn::{
    Expr, ItemMacro, ItemUse, Lit, Macro, Token, UseTree,
    parse::Parser as _,
    punctuated::Punctuated,
    visit::{self, Visit},
};

const MANIFEST: &str = "vendored-notices.toml";
const NOTICE: &str = "THIRD-PARTY-NOTICES.txt";
const VENDOR_DIR: &str = "crates/pgokf-web/static/vendor";
const WEB_SOURCE_DIR: &str = "crates/pgokf-web/src";
const CARGO_ABOUT_VERSION: &str = "cargo-about 0.9.2";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const CARGO_ABOUT_LINUX_X86_64_SHA256: &str =
    "b06bd6a8bfd726cffb90e3e0588e3e0b1cfbb582bf6a34f4c1c2692ba8f2e7b8";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const CARGO_ABOUT_MACOS_AARCH64_SHA256: &str =
    "3f0f43d1da14dfa5f61244f3c8e4c66d271f03a80e8e7bbbe5cc4e9a57576972";
const UPSTREAM_PROVENANCE: &str = include_str!("notices-provenance.json");

fn production_generator_sha256() -> Result<&'static str, String> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return Ok(CARGO_ABOUT_LINUX_X86_64_SHA256);
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return Ok(CARGO_ABOUT_MACOS_AARCH64_SHA256);
    #[allow(unreachable_code)]
    Err(format!(
        "cargo-about identity is not pinned for {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    #[serde(default)]
    first_party_script: Vec<FirstPartyScript>,
    asset: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FirstPartyScript {
    path: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Asset {
    name: String,
    version: String,
    spdx: String,
    source: String,
    path: String,
    tarball_member: String,
    sha256: String,
    #[serde(default)]
    inventory: Option<Inventory>,
    component: Vec<NoticeComponent>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    basis: String,
    npm_tarball: String,
    npm_tarball_sha256: String,
    source_map: String,
    source_map_file: String,
    source_map_member: String,
    source_map_path: String,
    source_map_sha256: String,
    lockfile_path: String,
    lockfile_sha256: String,
    source_count: u32,
    root_source_count: u32,
    embedded_source_count: u32,
    embedded_component_count: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoticeComponent {
    name: String,
    #[serde(default)]
    npm_name: Option<String>,
    version: String,
    spdx: String,
    source: String,
    source_count: u32,
    tarball: String,
    tarball_sha256: String,
    tarball_path: String,
    integrity: String,
    license_path: String,
    license_member: String,
    license_sha256: String,
}

impl NoticeComponent {
    fn package_name(&self) -> &str {
        self.npm_name.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Debug, Deserialize)]
struct NpmPackage {
    name: String,
    version: String,
    license: String,
    repository: NpmRepository,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NpmRepository {
    String(String),
    Object(NpmRepositoryObject),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NpmRepositoryObject {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    url: String,
    #[serde(default)]
    directory: Option<String>,
}

impl NpmRepository {
    fn identity(&self) -> Result<String, String> {
        match self {
            Self::String(value) => normalized_repository(value),
            Self::Object(repository) => {
                if repository.kind.as_deref().is_some_and(|kind| kind != "git") {
                    return Err("unsupported npm repository type".into());
                }
                let base = normalized_repository(&repository.url)?;
                match &repository.directory {
                    Some(directory) => Ok(format!(
                        "{base}#directory={}",
                        normalized_repository_directory(directory)?
                    )),
                    None => Ok(base),
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Provenance {
    schema_version: u32,
    assets: Vec<ProvenanceAsset>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvenanceAsset {
    name: String,
    version: String,
    spdx: String,
    source: String,
    path: String,
    tarball_member: String,
    asset_sha256: String,
    npm_tarball_sha256: String,
    source_map_sha256: Option<String>,
    lockfile_sha256: Option<String>,
    components: Vec<ProvenanceComponent>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvenanceComponent {
    name: String,
    npm_name: Option<String>,
    version: String,
    spdx: String,
    source: String,
    tarball: String,
    tarball_sha256: String,
    integrity: String,
    license_sha256: String,
    selectors: Vec<String>,
    source_content_sha256: String,
}

impl ProvenanceComponent {
    fn package_name(&self) -> &str {
        self.npm_name.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Debug, Deserialize)]
struct SourceMap {
    version: u32,
    file: String,
    sources: Vec<String>,
    #[serde(rename = "sourcesContent")]
    sources_content: Vec<Option<String>>,
}

#[derive(Debug)]
struct LockEntry {
    selectors: Vec<String>,
    version: String,
    integrity: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut output, byte| {
        write!(output, "{byte:02x}").expect("writing String cannot fail");
        output
    })
}

fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn repository_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, PathComponent::Normal(_)))
    {
        return Err(format!("non-normal repository path: {relative}"));
    }
    Ok(root.join(path))
}

fn regular_file(root: &Path, relative: &str) -> Result<Vec<u8>, String> {
    let path = repository_path(root, relative)?;
    let metadata = fs::symlink_metadata(&path).map_err(|error| format!("{relative}: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{relative}: regular file required"));
    }
    fs::read(path).map_err(|error| format!("{relative}: {error}"))
}

fn authenticated_file(root: &Path, relative: &str, expected: &str) -> Result<Vec<u8>, String> {
    if !valid_digest(expected) {
        return Err(format!("{relative}: invalid declared SHA-256"));
    }
    let bytes = regular_file(root, relative)?;
    let actual = digest(&bytes);
    if actual != expected {
        return Err(format!(
            "{relative}: SHA-256 mismatch (declared {expected}, actual {actual})"
        ));
    }
    Ok(bytes)
}

fn source_content_digest<'a>(rows: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let mut hasher = Sha256::new();
    for (path, content) in rows {
        for bytes in [path.as_bytes(), content.as_bytes()] {
            hasher.update(u64::try_from(bytes.len()).unwrap().to_be_bytes());
            hasher.update(bytes);
        }
    }
    hex(&hasher.finalize())
}

type SourceMapInventory = (u32, BTreeMap<String, u32>, String, BTreeMap<String, String>);

fn source_map_inventory(bytes: &[u8], expected_file: &str) -> Result<SourceMapInventory, String> {
    let map: SourceMap = crate::json::from_slice(bytes)?;
    if map.version != 3
        || map.file != expected_file
        || map.sources.is_empty()
        || map.sources.len() != map.sources_content.len()
        || map.sources_content.iter().any(Option::is_none)
    {
        return Err("invalid or incomplete source-map evidence".into());
    }
    let mut root_sources = Vec::new();
    let mut packages: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (source, content) in map.sources.into_iter().zip(map.sources_content) {
        let content = content.ok_or("source-map content disappeared")?;
        if source.starts_with("../src/") {
            root_sources.push((source, content));
            continue;
        }
        let Some(path) = source.strip_prefix("../node_modules/") else {
            return Err(format!("unclassified source-map source: {source}"));
        };
        let mut parts = path.split('/');
        let first = parts.next().unwrap_or_default();
        let package = if first.starts_with('@') {
            let second = parts.next().unwrap_or_default();
            format!("{first}/{second}")
        } else {
            first.to_owned()
        };
        if package.is_empty() || package.ends_with('/') {
            return Err(format!("invalid source-map package path: {source}"));
        }
        packages.entry(package).or_default().push((source, content));
    }
    let root_count = u32::try_from(root_sources.len()).map_err(|_| "source count overflow")?;
    let root_hash = source_content_digest(
        root_sources
            .iter()
            .map(|(path, content)| (path.as_str(), content.as_str())),
    );
    let counts = packages
        .iter()
        .map(|(name, rows)| {
            Ok((
                name.clone(),
                u32::try_from(rows.len()).map_err(|_| "source count overflow")?,
            ))
        })
        .collect::<Result<_, String>>()?;
    let hashes = packages
        .into_iter()
        .map(|(name, rows)| {
            let hash = source_content_digest(
                rows.iter()
                    .map(|(path, content)| (path.as_str(), content.as_str())),
            );
            (name, hash)
        })
        .collect();
    Ok((root_count, counts, root_hash, hashes))
}

fn yarn_lock_entries(bytes: &[u8]) -> Result<Vec<LockEntry>, String> {
    let text = std::str::from_utf8(bytes).map_err(|error| format!("yarn.lock: {error}"))?;
    let mut output = Vec::new();
    let mut selectors = None;
    let mut version = None;
    let mut integrity = None;
    let finish = |output: &mut Vec<LockEntry>,
                  selectors: &mut Option<Vec<String>>,
                  version: &mut Option<String>,
                  integrity: &mut Option<String>|
     -> Result<(), String> {
        let Some(names) = selectors.take() else {
            return Ok(());
        };
        let version = version.take().ok_or("yarn.lock entry missing version")?;
        let integrity = integrity
            .take()
            .ok_or("yarn.lock entry missing integrity")?;
        output.push(LockEntry {
            selectors: names,
            version,
            integrity,
        });
        Ok(())
    };
    for line in text.lines() {
        if !line.starts_with([' ', '\t']) && line.ends_with(':') && !line.starts_with('#') {
            finish(&mut output, &mut selectors, &mut version, &mut integrity)?;
            let header = line.trim_end_matches(':');
            selectors = Some(
                header
                    .split(',')
                    .map(|selector| selector.trim().trim_matches('"').to_owned())
                    .collect(),
            );
        } else if let Some(value) = line.strip_prefix("  version ") {
            if version
                .replace(value.trim_matches('"').to_owned())
                .is_some()
            {
                return Err("duplicate yarn.lock version".into());
            }
        } else if let Some(value) = line.strip_prefix("  integrity ")
            && integrity.replace(value.to_owned()).is_some()
        {
            return Err("duplicate yarn.lock integrity".into());
        }
    }
    finish(&mut output, &mut selectors, &mut version, &mut integrity)?;
    if output.is_empty() {
        return Err("empty yarn.lock evidence".into());
    }
    Ok(output)
}

fn selector_matches(selector: &str, package: &str, version: &str) -> Result<bool, String> {
    let range = selector
        .strip_prefix(&format!("{package}@"))
        .ok_or_else(|| format!("selector names another package: {selector}"))?;
    let version = semver::Version::parse(version)
        .map_err(|error| format!("invalid resolved package version: {error}"))?;
    if let Some((low, high)) = range.split_once(" - ") {
        fn partial(value: &str) -> Result<(semver::Version, usize), String> {
            let fields: Vec<_> = value.split('.').collect();
            if fields.is_empty() || fields.len() > 3 {
                return Err(format!("unsupported npm version range endpoint: {value}"));
            }
            let mut numbers = [0_u64; 3];
            for (index, field) in fields.iter().enumerate() {
                numbers[index] = field
                    .parse()
                    .map_err(|_| format!("invalid npm version range endpoint: {value}"))?;
            }
            Ok((
                semver::Version::new(numbers[0], numbers[1], numbers[2]),
                fields.len(),
            ))
        }
        let (low, _) = partial(low)?;
        let (high, fields) = partial(high)?;
        let within_high = if fields == 3 {
            version <= high
        } else if fields == 2 {
            version < semver::Version::new(high.major, high.minor + 1, 0)
        } else {
            version < semver::Version::new(high.major + 1, 0, 0)
        };
        return Ok(version >= low && within_high);
    }
    let normalized = if range.contains(' ') {
        range.split_whitespace().collect::<Vec<_>>().join(", ")
    } else {
        range.to_owned()
    };
    let requirement = semver::VersionReq::parse(&normalized)
        .map_err(|error| format!("unsupported npm selector {selector}: {error}"))?;
    Ok(requirement.matches(&version))
}

fn lock_binding<'a>(
    entries: &'a [LockEntry],
    component: &NoticeComponent,
) -> Result<&'a LockEntry, String> {
    let prefix = format!("{}@", component.package_name());
    let matches: Vec<_> = entries
        .iter()
        .filter(|entry| {
            entry.version == component.version
                && entry.integrity == component.integrity
                && entry
                    .selectors
                    .iter()
                    .any(|selector| selector.starts_with(&prefix))
        })
        .collect();
    if matches.len() != 1 {
        return Err(format!(
            "{} {}: unique authenticated yarn.lock binding required",
            component.name, component.version
        ));
    }
    for selector in &matches[0].selectors {
        if !selector_matches(selector, component.package_name(), &component.version)? {
            return Err(format!(
                "{} {}: resolved version violates authenticated selector {selector}",
                component.name, component.version
            ));
        }
    }
    Ok(matches[0])
}

fn normalized_repository_directory(value: &str) -> Result<String, String> {
    if value.is_empty()
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains(['\\', '#', '?'])
        || value
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(format!("unsupported npm repository directory: {value}"));
    }
    Ok(value.to_owned())
}

fn normalized_repository_identity(value: &str) -> Result<String, String> {
    match value.split_once("#directory=") {
        Some((repository, directory)) => Ok(format!(
            "{}#directory={}",
            normalized_repository(repository)?,
            normalized_repository_directory(directory)?
        )),
        None => normalized_repository(value),
    }
}

fn normalized_repository(value: &str) -> Result<String, String> {
    let mut value = value.trim().trim_end_matches('/');
    value = value.strip_prefix("git+").unwrap_or(value);
    let expanded;
    if !value.contains("://") && value.split('/').count() == 2 {
        expanded = format!("https://github.com/{value}");
        value = &expanded;
    }
    let value = value.strip_prefix("git://github.com/").map_or_else(
        || value.to_owned(),
        |path| format!("https://github.com/{path}"),
    );
    let value = value.strip_suffix(".git").unwrap_or(&value);
    if !value.starts_with("https://") || value.contains(['?', '#']) {
        return Err(format!("unsupported npm repository identity: {value}"));
    }
    Ok(value.to_owned())
}

fn tar_octal(field: &[u8], label: &str) -> Result<usize, String> {
    let text = std::str::from_utf8(field)
        .map_err(|error| format!("tar {label}: {error}"))?
        .trim_matches(['\0', ' ']);
    if text.is_empty() || !text.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
        return Err(format!("invalid tar {label}"));
    }
    usize::from_str_radix(text, 8).map_err(|error| format!("tar {label}: {error}"))
}

#[allow(clippy::too_many_lines)] // One fail-closed pass validates the complete tar stream.
fn tar_members(bytes: &[u8], wanted: &[&str]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    fn nul(field: &[u8]) -> &[u8] {
        field.split(|byte| *byte == 0).next().unwrap_or_default()
    }
    const LIMIT: u64 = 128 * 1024 * 1024;
    let mut decoded = Vec::new();
    GzDecoder::new(bytes)
        .take(LIMIT + 1)
        .read_to_end(&mut decoded)
        .map_err(|error| format!("npm tarball gzip: {error}"))?;
    if decoded.len() as u64 > LIMIT {
        return Err("npm tarball exceeds decompression limit".into());
    }
    let wanted: BTreeSet<_> = wanted.iter().copied().collect();
    let mut seen = BTreeSet::new();
    let mut output = BTreeMap::new();
    let mut offset = 0_usize;
    let mut terminated = false;
    while offset + 512 <= decoded.len() {
        let header = &decoded[offset..offset + 512];
        if header.iter().all(|byte| *byte == 0) {
            let terminator_end = offset
                .checked_add(1024)
                .ok_or("tar terminator offset overflow")?;
            if terminator_end > decoded.len()
                || decoded[offset + 512..terminator_end]
                    .iter()
                    .any(|byte| *byte != 0)
                || decoded[terminator_end..].iter().any(|byte| *byte != 0)
            {
                return Err("npm tarball has an invalid terminator or nonzero trailer".into());
            }
            terminated = true;
            break;
        }
        let expected_checksum = tar_octal(&header[148..156], "checksum")?;
        let actual_checksum: usize = header
            .iter()
            .enumerate()
            .map(|(index, byte)| {
                if (148..156).contains(&index) {
                    usize::from(b' ')
                } else {
                    usize::from(*byte)
                }
            })
            .sum();
        if expected_checksum != actual_checksum {
            return Err("npm tarball header checksum mismatch".into());
        }
        let name = std::str::from_utf8(nul(&header[..100]))
            .map_err(|error| format!("tar path: {error}"))?;
        let prefix = std::str::from_utf8(nul(&header[345..500]))
            .map_err(|error| format!("tar prefix: {error}"))?;
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_empty()
            || path.starts_with('/')
            || path.contains('\\')
            || path.as_bytes().get(1) == Some(&b':')
            || path
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
            || Path::new(&path)
                .components()
                .any(|part| !matches!(part, PathComponent::Normal(_)))
        {
            return Err(format!("unsafe npm tarball member path: {path}"));
        }
        if !seen.insert(path.clone()) {
            return Err(format!("duplicate npm tarball member: {path}"));
        }
        let kind = header[156];
        if !matches!(kind, 0 | b'0' | b'5') {
            return Err(format!("unsupported npm tarball member type: {path}"));
        }
        let size = tar_octal(&header[124..136], "size")?;
        if kind == b'5' && size != 0 {
            return Err(format!("npm tarball directory has content: {path}"));
        }
        let start = offset.checked_add(512).ok_or("tar offset overflow")?;
        let end = start.checked_add(size).ok_or("tar size overflow")?;
        if end > decoded.len() {
            return Err("truncated npm tarball member".into());
        }
        if wanted.contains(path.as_str()) {
            if !matches!(kind, 0 | b'0') {
                return Err(format!(
                    "required npm tarball member is not regular: {path}"
                ));
            }
            output.insert(path.clone(), decoded[start..end].to_vec());
        }
        let padded = size.checked_add(511).ok_or("tar padding overflow")? / 512 * 512;
        offset = start.checked_add(padded).ok_or("tar offset overflow")?;
    }
    if !terminated {
        return Err("npm tarball is missing the two-block terminator".into());
    }
    let found: BTreeSet<_> = output.keys().map(String::as_str).collect();
    let missing: Vec<_> = wanted.difference(&found).copied().collect();
    if !missing.is_empty() {
        return Err(format!("npm tarball missing required members: {missing:?}"));
    }
    Ok(output)
}

fn integrity_matches(bytes: &[u8], declared: &str) -> bool {
    declared
        .strip_prefix("sha512-")
        .and_then(|encoded| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
        })
        .is_some_and(|expected| expected == Sha512::digest(bytes).as_slice())
}

fn files_under(root: &Path, relative: &str) -> Result<BTreeSet<String>, String> {
    fn walk(root: &Path, directory: &Path, output: &mut BTreeSet<String>) -> Result<(), String> {
        for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let metadata = entry.file_type().map_err(|error| error.to_string())?;
            if metadata.is_symlink() {
                return Err(format!(
                    "vendored notice inventory contains symlink: {}",
                    entry.path().display()
                ));
            }
            if metadata.is_dir() {
                walk(root, &entry.path(), output)?;
            } else if metadata.is_file() {
                output.insert(
                    entry
                        .path()
                        .strip_prefix(root)
                        .map_err(|error| error.to_string())?
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            } else {
                return Err("vendored notice inventory contains non-file entry".into());
            }
        }
        Ok(())
    }
    let mut output = BTreeSet::new();
    walk(root, &repository_path(root, relative)?, &mut output)?;
    Ok(output)
}

#[allow(clippy::too_many_lines)] // Closed structural scan of every embedded web asset expression.
fn served_javascript(root: &Path) -> Result<BTreeSet<String>, String> {
    fn expression_value(expression: &Expr) -> Result<String, String> {
        match expression {
            Expr::Lit(literal) => match &literal.lit {
                Lit::Str(value) => Ok(value.value()),
                _ => Err("embedded path contains a non-string literal".into()),
            },
            Expr::Macro(macro_expression) if macro_expression.mac.path.is_ident("env") => {
                let value: syn::LitStr = syn::parse2(macro_expression.mac.tokens.clone())
                    .map_err(|error| format!("embedded env!: {error}"))?;
                if value.value() == "CARGO_MANIFEST_DIR" {
                    Ok("crates/pgokf-web".into())
                } else {
                    Err("embedded path uses an unsupported environment variable".into())
                }
            }
            Expr::Macro(macro_expression) if macro_expression.mac.path.is_ident("concat") => {
                let expressions = Punctuated::<Expr, Token![,]>::parse_terminated
                    .parse2(macro_expression.mac.tokens.clone())
                    .map_err(|error| format!("embedded concat!: {error}"))?;
                expressions
                    .iter()
                    .try_fold(String::new(), |mut output, expression| {
                        output.push_str(&expression_value(expression)?);
                        Ok(output)
                    })
            }
            _ => Err("embedded path is not a closed literal expression".into()),
        }
    }

    fn normalized(base: &Path, value: &str) -> Result<String, String> {
        let joined = if value.starts_with("crates/pgokf-web/") {
            PathBuf::from(value)
        } else {
            base.join(value)
        };
        let mut output = PathBuf::new();
        for component in joined.components() {
            match component {
                PathComponent::Normal(value) => output.push(value),
                PathComponent::CurDir => {}
                PathComponent::ParentDir if output.pop() => {}
                _ => return Err(format!("embedded path escapes repository: {value}")),
            }
        }
        Ok(output.to_string_lossy().replace('\\', "/"))
    }

    fn collect_renames(tree: &UseTree, output: &mut Vec<(String, String)>) {
        match tree {
            UseTree::Rename(rename) => {
                output.push((rename.ident.to_string(), rename.rename.to_string()));
            }
            UseTree::Path(path) => collect_renames(&path.tree, output),
            UseTree::Group(group) => {
                for item in &group.items {
                    collect_renames(item, output);
                }
            }
            _ => {}
        }
    }

    struct Aliases<'a>(&'a mut Vec<(String, String)>);
    impl<'ast> Visit<'ast> for Aliases<'_> {
        fn visit_item_use(&mut self, node: &'ast ItemUse) {
            collect_renames(&node.tree, self.0);
            visit::visit_item_use(self, node);
        }
    }

    struct MacroWrappers<'a> {
        known_names: &'a BTreeSet<String>,
        wrappers: &'a mut BTreeSet<String>,
    }
    impl<'ast> Visit<'ast> for MacroWrappers<'_> {
        fn visit_item_macro(&mut self, node: &'ast ItemMacro) {
            if node.mac.path.is_ident("macro_rules")
                && let Some(name) = &node.ident
            {
                let body = node.mac.tokens.to_string();
                let embeds = body
                    .split(|character: char| !(character.is_alphanumeric() || character == '_'))
                    .any(|word| self.known_names.contains(word));
                if embeds {
                    self.wrappers.insert(name.to_string());
                }
            }
            visit::visit_item_macro(self, node);
        }
    }

    struct Includes<'a> {
        base: &'a Path,
        scripts: &'a mut BTreeSet<String>,
        include_names: &'a BTreeSet<String>,
        wrappers: &'a BTreeSet<String>,
        error: Option<String>,
    }
    impl<'ast> Visit<'ast> for Includes<'_> {
        fn visit_macro(&mut self, node: &'ast Macro) {
            let terminal = node
                .path
                .segments
                .last()
                .map(|segment| segment.ident.to_string());
            let is_direct_include = terminal
                .as_deref()
                .is_some_and(|name| self.include_names.contains(name));
            let is_wrapper = terminal
                .as_deref()
                .is_some_and(|name| self.wrappers.contains(name));
            if self.error.is_none() && is_wrapper {
                self.error = Some(
                    "embedded include macro wrapper is unsupported; use a direct include macro"
                        .into(),
                );
            } else if self.error.is_none() && is_direct_include {
                match syn::parse2::<Expr>(node.tokens.clone())
                    .map_err(|error| format!("embedded include: {error}"))
                    .and_then(|expression| expression_value(&expression))
                    .and_then(|value| normalized(self.base, &value))
                {
                    Ok(path)
                        if Path::new(&path)
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("js")) =>
                    {
                        self.scripts.insert(path);
                    }
                    Ok(_) => {}
                    Err(error) => self.error = Some(error),
                }
            }
            visit::visit_macro(self, node);
        }
    }

    let mut parsed_files = Vec::new();
    for relative in files_under(root, WEB_SOURCE_DIR)? {
        if !Path::new(&relative)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
        {
            continue;
        }
        let source = regular_file(root, &relative)?;
        let parsed = syn::parse_file(
            std::str::from_utf8(&source).map_err(|error| format!("{relative}: {error}"))?,
        )
        .map_err(|error| format!("{relative}: {error}"))?;
        let parent = Path::new(&relative)
            .parent()
            .ok_or_else(|| format!("{relative}: missing source parent"))?
            .to_path_buf();
        parsed_files.push((relative, parent, parsed));
    }

    let mut renames = Vec::new();
    for (_, _, parsed) in &parsed_files {
        Aliases(&mut renames).visit_file(parsed);
    }
    let mut include_names = BTreeSet::from(["include_str".into(), "include_bytes".into()]);
    loop {
        let additions: Vec<_> = renames
            .iter()
            .filter(|(source, _)| include_names.contains(source))
            .map(|(_, alias)| alias.clone())
            .collect();
        let before = include_names.len();
        include_names.extend(additions);
        if include_names.len() == before {
            break;
        }
    }

    let mut wrappers = BTreeSet::new();
    loop {
        let mut known_names = include_names.clone();
        known_names.extend(wrappers.iter().cloned());
        let before = wrappers.len();
        for (_, _, parsed) in &parsed_files {
            MacroWrappers {
                known_names: &known_names,
                wrappers: &mut wrappers,
            }
            .visit_file(parsed);
        }
        if wrappers.len() == before {
            break;
        }
    }

    let mut scripts = BTreeSet::new();
    for (relative, parent, parsed) in &parsed_files {
        let mut includes = Includes {
            base: parent,
            scripts: &mut scripts,
            include_names: &include_names,
            wrappers: &wrappers,
            error: None,
        };
        includes.visit_file(parsed);
        if let Some(error) = includes.error {
            return Err(format!("{relative}: {error}"));
        }
    }
    Ok(scripts)
}

fn load(root: &Path) -> Result<Manifest, String> {
    let source = regular_file(root, MANIFEST)?;
    let manifest: Manifest = toml::from_str(
        std::str::from_utf8(&source).map_err(|error| format!("{MANIFEST}: {error}"))?,
    )
    .map_err(|error| format!("{MANIFEST}: {error}"))?;
    if manifest.schema_version != 1 || manifest.asset.is_empty() {
        return Err("unsupported or empty vendored notice manifest".into());
    }
    Ok(manifest)
}

type ValidatedAsset<'a> = (&'a Asset, Vec<Vec<u8>>);

#[allow(clippy::too_many_lines)] // One fail-closed manifest validation boundary.
fn validate<'a>(
    root: &Path,
    manifest: &'a Manifest,
    provenance: &Provenance,
) -> Result<Vec<ValidatedAsset<'a>>, String> {
    if provenance.schema_version != 1 {
        return Err("unsupported upstream notice provenance schema".into());
    }
    let mut upstream_assets: BTreeMap<_, _> = provenance
        .assets
        .iter()
        .map(|asset| ((asset.name.as_str(), asset.version.as_str()), asset))
        .collect();
    if upstream_assets.len() != provenance.assets.len() {
        return Err("duplicate upstream notice provenance asset".into());
    }
    let mut expected_files = BTreeSet::new();
    let mut served_scripts = BTreeSet::new();
    let mut previous_first_party = None;
    for script in &manifest.first_party_script {
        if previous_first_party.is_some_and(|previous: &str| previous >= script.path.as_str())
            || script.path.starts_with(VENDOR_DIR)
        {
            return Err("first-party script entries must be uniquely sorted outside vendor".into());
        }
        previous_first_party = Some(&script.path);
        authenticated_file(root, &script.path, &script.sha256)?;
        served_scripts.insert(script.path.clone());
    }
    let mut previous_asset = None;
    let mut validated = Vec::new();
    for asset in &manifest.asset {
        if previous_asset.is_some_and(|previous: &str| previous >= asset.path.as_str()) {
            return Err("vendored asset entries must be uniquely sorted by path".into());
        }
        previous_asset = Some(&asset.path);
        if asset.name.is_empty()
            || asset.version.is_empty()
            || asset.spdx.is_empty()
            || asset.source.is_empty()
            || asset.tarball_member.is_empty()
            || asset.component.is_empty()
        {
            return Err(format!(
                "{}: incomplete vendored asset identity",
                asset.path
            ));
        }
        let upstream = upstream_assets
            .remove(&(asset.name.as_str(), asset.version.as_str()))
            .ok_or_else(|| {
                format!(
                    "{} {}: missing independent upstream provenance",
                    asset.name, asset.version
                )
            })?;
        if asset.spdx != upstream.spdx
            || asset.source != upstream.source
            || asset.path != upstream.path
            || asset.tarball_member != upstream.tarball_member
            || asset.sha256 != upstream.asset_sha256
        {
            return Err(format!(
                "{} {}: asset identity differs from independent upstream provenance",
                asset.name, asset.version
            ));
        }
        let mut upstream_components: BTreeMap<_, _> = upstream
            .components
            .iter()
            .map(|component| {
                (
                    (component.name.as_str(), component.version.as_str()),
                    component,
                )
            })
            .collect();
        if upstream_components.len() != upstream.components.len() {
            return Err(format!(
                "{}: duplicate upstream component provenance",
                asset.path
            ));
        }
        let asset_bytes = authenticated_file(root, &asset.path, &asset.sha256)?;
        expected_files.insert(asset.path.clone());
        served_scripts.insert(asset.path.clone());

        let mut previous_component: Option<(&str, &str)> = None;
        let mut licenses = Vec::new();
        let mut total_sources = 0_u32;
        let mut embedded_sources = 0_u32;
        let mut embedded_components = 0_usize;
        let mut root_sources = None;
        let mut root_tarball = None;
        let mut root_component = None;
        for component in &asset.component {
            let identity = (component.name.as_str(), component.version.as_str());
            if previous_component.is_some_and(|previous| previous >= identity) {
                return Err(format!(
                    "{}: components must be uniquely sorted by name and version",
                    asset.path
                ));
            }
            previous_component = Some(identity);
            if component.name.is_empty()
                || component.npm_name.as_deref().is_some_and(str::is_empty)
                || component.version.is_empty()
                || component.spdx.is_empty()
                || component.source.is_empty()
                || component.tarball.is_empty()
                || component.tarball_path.is_empty()
                || component.license_member.is_empty()
                || component.integrity.is_empty()
                || !valid_digest(&component.tarball_sha256)
            {
                return Err(format!(
                    "{} {}: incomplete component evidence",
                    component.name, component.version
                ));
            }
            let upstream_component = upstream_components
                .remove(&(component.name.as_str(), component.version.as_str()))
                .ok_or_else(|| {
                    format!(
                        "{} {}: identity differs from independent upstream provenance",
                        component.name, component.version
                    )
                })?;
            if component.package_name() != upstream_component.package_name()
                || component.spdx != upstream_component.spdx
                || component.source != upstream_component.source
                || component.tarball != upstream_component.tarball
                || component.tarball_sha256 != upstream_component.tarball_sha256
                || component.integrity != upstream_component.integrity
                || component.license_sha256 != upstream_component.license_sha256
            {
                return Err(format!(
                    "{} {}: declaration differs from independent upstream provenance",
                    component.name, component.version
                ));
            }
            let license =
                authenticated_file(root, &component.license_path, &component.license_sha256)?;
            if license.is_empty() || std::str::from_utf8(&license).is_err() {
                return Err(format!(
                    "{} {}: non-empty UTF-8 license text required",
                    component.name, component.version
                ));
            }
            let tarball =
                authenticated_file(root, &component.tarball_path, &component.tarball_sha256)?;
            if !integrity_matches(&tarball, &component.integrity) {
                return Err(format!(
                    "{} {}: npm integrity differs from authenticated tarball",
                    component.name, component.version
                ));
            }
            let members = tar_members(
                &tarball,
                &["package/package.json", component.license_member.as_str()],
            )?;
            let package: NpmPackage = crate::json::from_slice(&members["package/package.json"])?;
            if package.name != component.package_name()
                || package.version != component.version
                || package.license != component.spdx
                || package.repository.identity()?
                    != normalized_repository_identity(&component.source)?
                || members[&component.license_member] != license
            {
                return Err(format!(
                    "{} {}: package metadata, repository, or license differs from authenticated tarball",
                    component.name, component.version
                ));
            }
            expected_files.insert(component.license_path.clone());
            total_sources = total_sources
                .checked_add(component.source_count)
                .ok_or("source count overflow")?;
            let is_root = component.name == asset.name && component.version == asset.version;
            if is_root {
                if root_sources.replace(component.source_count).is_some()
                    || root_tarball.replace(tarball).is_some()
                    || root_component.replace(component).is_some()
                {
                    return Err(format!("{}: duplicate root component", asset.path));
                }
            } else {
                embedded_sources += component.source_count;
                embedded_components += 1;
            }
            licenses.push(license);
        }
        if !upstream_components.is_empty() {
            return Err(format!(
                "{}: upstream component provenance is unused",
                asset.path
            ));
        }
        let root_tarball = root_tarball.ok_or_else(|| {
            format!(
                "{}: missing authenticated root component tarball",
                asset.path
            )
        })?;
        let root_component = root_component
            .ok_or_else(|| format!("{}: missing authenticated root component", asset.path))?;
        let bundle = tar_members(&root_tarball, &[asset.tarball_member.as_str()])?;
        if bundle[&asset.tarball_member] != asset_bytes {
            return Err(format!(
                "{}: shipped asset differs from authenticated npm tarball",
                asset.path
            ));
        }
        match &asset.inventory {
            Some(inventory) => {
                if inventory.basis.is_empty()
                    || inventory.npm_tarball.is_empty()
                    || inventory.npm_tarball != root_component.tarball
                    || inventory.npm_tarball_sha256 != root_component.tarball_sha256
                    || inventory.npm_tarball_sha256 != upstream.npm_tarball_sha256
                    || inventory.source_map.is_empty()
                    || inventory.source_map_file.is_empty()
                    || inventory.source_map_member.is_empty()
                    || inventory.source_map_path.is_empty()
                    || upstream.source_map_sha256.as_deref()
                        != Some(inventory.source_map_sha256.as_str())
                    || inventory.lockfile_path.is_empty()
                    || upstream.lockfile_sha256.as_deref()
                        != Some(inventory.lockfile_sha256.as_str())
                    || !valid_digest(&inventory.npm_tarball_sha256)
                    || !valid_digest(&inventory.source_map_sha256)
                    || !valid_digest(&inventory.lockfile_sha256)
                    || inventory.source_count != total_sources
                    || root_sources != Some(inventory.root_source_count)
                    || inventory.root_source_count + inventory.embedded_source_count
                        != inventory.source_count
                    || inventory.embedded_source_count != embedded_sources
                    || inventory.embedded_component_count != embedded_components
                {
                    return Err(format!("{}: inconsistent source-map inventory", asset.path));
                }
                let source_map = authenticated_file(
                    root,
                    &inventory.source_map_path,
                    &inventory.source_map_sha256,
                )?;
                let tar_source_map =
                    tar_members(&root_tarball, &[inventory.source_map_member.as_str()])?;
                if tar_source_map[&inventory.source_map_member] != source_map {
                    return Err(format!(
                        "{}: source map differs from authenticated npm tarball",
                        asset.path
                    ));
                }
                let (mapped_root, mapped_packages, root_source_hash, package_source_hashes) =
                    source_map_inventory(&source_map, &inventory.source_map_file)?;
                let expected_packages: BTreeMap<_, _> = asset
                    .component
                    .iter()
                    .filter(|component| {
                        component.name != asset.name || component.version != asset.version
                    })
                    .map(|component| (component.name.clone(), component.source_count))
                    .collect();
                let expected_source_hashes: BTreeMap<_, _> = upstream
                    .components
                    .iter()
                    .filter(|component| {
                        component.name != asset.name || component.version != asset.version
                    })
                    .map(|component| {
                        (
                            component.name.clone(),
                            component.source_content_sha256.clone(),
                        )
                    })
                    .collect();
                let expected_root_hash = upstream
                    .components
                    .iter()
                    .find(|component| {
                        component.name == asset.name && component.version == asset.version
                    })
                    .map(|component| component.source_content_sha256.as_str());
                if mapped_root != inventory.root_source_count
                    || mapped_packages != expected_packages
                    || mapped_packages.len() != inventory.embedded_component_count
                    || package_source_hashes != expected_source_hashes
                    || Some(root_source_hash.as_str()) != expected_root_hash
                {
                    return Err(format!(
                        "{}: component inventory differs from authenticated source map",
                        asset.path
                    ));
                }
                let lockfile =
                    authenticated_file(root, &inventory.lockfile_path, &inventory.lockfile_sha256)?;
                let lock = yarn_lock_entries(&lockfile)?;
                for component in asset.component.iter().filter(|component| {
                    component.name != asset.name || component.version != asset.version
                }) {
                    let binding = lock_binding(&lock, component)?;
                    let expected = upstream
                        .components
                        .iter()
                        .find(|expected| {
                            expected.name == component.name && expected.version == component.version
                        })
                        .ok_or_else(|| {
                            format!(
                                "{} {}: absent from independent upstream provenance",
                                component.name, component.version
                            )
                        })?;
                    if binding.selectors != expected.selectors {
                        return Err(format!(
                            "{} {}: yarn.lock selectors differ from independent upstream provenance",
                            component.name, component.version
                        ));
                    }
                }
            }
            None if asset.component.len() != 1 || root_sources.is_none() => {
                return Err(format!(
                    "{}: single root component or bundled inventory required",
                    asset.path
                ));
            }
            None if upstream.npm_tarball_sha256 != root_component.tarball_sha256
                || upstream.source_map_sha256.is_some()
                || upstream.lockfile_sha256.is_some() =>
            {
                return Err(format!(
                    "{}: standalone asset provenance is inconsistent",
                    asset.path
                ));
            }
            None => {}
        }
        validated.push((asset, licenses));
    }
    if !upstream_assets.is_empty() {
        return Err("unused independent upstream notice provenance asset".into());
    }

    let embedded = served_javascript(root)?;
    if embedded != served_scripts {
        return Err(format!(
            "served JavaScript differs from classified notice inventory: embedded={embedded:?}, manifest={served_scripts:?}"
        ));
    }
    let actual_files = files_under(root, VENDOR_DIR)?;
    if actual_files != expected_files {
        return Err(format!(
            "vendored file inventory differs: actual={actual_files:?}, manifest={expected_files:?}"
        ));
    }
    Ok(validated)
}

fn normalize(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let text =
        std::str::from_utf8(bytes).map_err(|error| format!("cargo-about output: {error}"))?;
    let mut output = String::new();
    for line in text.lines() {
        output.push_str(line.trim_end_matches([' ', '\t']));
        output.push('\n');
    }
    if output.trim().is_empty() {
        return Err("cargo-about returned empty notices".into());
    }
    Ok(output.into_bytes())
}

fn authenticated_generator(
    cargo_about: &Path,
    expected_sha256: &str,
    expected_version: &str,
) -> Result<tempfile::TempPath, String> {
    let metadata = fs::symlink_metadata(cargo_about).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("cargo-about must be a regular file".into());
    }
    if !valid_digest(expected_sha256) {
        return Err("invalid pinned cargo-about SHA-256".into());
    }
    let bytes = fs::read(cargo_about).map_err(|error| error.to_string())?;
    let actual = digest(&bytes);
    if actual != expected_sha256 {
        return Err(format!(
            "cargo-about SHA-256 mismatch (pinned {expected_sha256}, actual {actual})"
        ));
    }
    let mut executable = tempfile::Builder::new()
        .prefix("pgokf-cargo-about-")
        .tempfile()
        .map_err(|error| error.to_string())?;
    std::io::Write::write_all(executable.as_file_mut(), &bytes)
        .map_err(|error| error.to_string())?;
    executable
        .as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        executable
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o500))
            .map_err(|error| error.to_string())?;
    }
    #[cfg(not(unix))]
    return Err("cargo-about execution is supported only on Unix".into());

    let executable = executable.into_temp_path();
    let output = Command::new(&executable)
        .arg("--version")
        .env_clear()
        .output()
        .map_err(|error| format!("cargo-about version check failed to start: {error}"))?;
    if !output.status.success()
        || String::from_utf8_lossy(&output.stdout).trim() != expected_version
        || !output.stderr.is_empty()
    {
        return Err("cargo-about version identity mismatch".into());
    }
    Ok(executable)
}

struct CargoEnvironment {
    temporary: tempfile::TempDir,
    cargo_home: PathBuf,
    path: std::ffi::OsString,
    rustup_home: Option<std::ffi::OsString>,
    rustup_toolchain: Option<std::ffi::OsString>,
}

fn isolated_cargo_environment() -> Result<CargoEnvironment, String> {
    #[cfg(not(unix))]
    return Err("isolated cargo execution is supported only on Unix".into());

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::Builder::new()
            .prefix("pgokf-cargo-home-")
            .tempdir()
            .map_err(|error| error.to_string())?;
        let cargo_home = temporary.path().join("cargo");
        fs::create_dir(&cargo_home).map_err(|error| error.to_string())?;
        let original = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
            .ok_or("CARGO_HOME or HOME is required for frozen notice generation")?;
        for directory in ["registry", "git"] {
            let source = original.join(directory);
            if source.exists() {
                symlink(&source, cargo_home.join(directory)).map_err(|error| error.to_string())?;
            }
        }
        Ok(CargoEnvironment {
            temporary,
            cargo_home,
            path: std::env::var_os("PATH").ok_or("PATH is required for cargo-about")?,
            rustup_home: std::env::var_os("RUSTUP_HOME"),
            rustup_toolchain: std::env::var_os("RUSTUP_TOOLCHAIN"),
        })
    }
}

fn cargo_notices(
    root: &Path,
    cargo_about: &Path,
    environment: &CargoEnvironment,
) -> Result<Vec<u8>, String> {
    let mut command = Command::new(cargo_about);
    command
        .args([
            "generate",
            "--frozen",
            "--fail",
            "--workspace",
            "--all-features",
            "-o",
            "/dev/stdout",
            "about.hbs",
        ])
        .current_dir(root)
        .env_clear()
        .env("HOME", environment.temporary.path())
        .env("CARGO_HOME", &environment.cargo_home)
        .env("PATH", &environment.path)
        .env("LC_ALL", "C")
        .env("TZ", "UTC");
    if let Some(value) = &environment.rustup_home {
        command.env("RUSTUP_HOME", value);
    }
    if let Some(value) = &environment.rustup_toolchain {
        command.env("RUSTUP_TOOLCHAIN", value);
    }
    let output = command
        .output()
        .map_err(|error| format!("cargo-about failed to start: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo-about failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    normalize(&output.stdout)
}

/// Render the exact vendored-JavaScript appendix after authenticating every byte.
/// # Errors
/// Malformed identities, missing files, symlinks, hash drift, incomplete component
/// evidence, source-map count drift, or active-asset inventory drift fail closed.
#[allow(clippy::too_many_lines)] // The appendix format is one reviewable deterministic contract.
pub fn render_vendored(root: &Path) -> Result<Vec<u8>, String> {
    render_vendored_with_provenance(root, UPSTREAM_PROVENANCE)
}

/// Test-only renderer with an explicitly supplied independent provenance contract.
/// # Errors
/// Malformed provenance or any notice evidence drift fails closed.
#[doc(hidden)]
#[allow(clippy::too_many_lines)] // The appendix format is one reviewable deterministic contract.
pub fn render_vendored_with_provenance(
    root: &Path,
    provenance_json: &str,
) -> Result<Vec<u8>, String> {
    let manifest = load(root)?;
    let provenance: Provenance = crate::json::from_slice(provenance_json.as_bytes())?;
    let validated = validate(root, &manifest, &provenance)?;
    let mut output = String::new();
    writeln!(
        output,
        "================================================================================"
    )
    .unwrap();
    writeln!(
        output,
        "Vendored JavaScript (shipped in the pgokf-web companion, not a Cargo crate)"
    )
    .unwrap();
    writeln!(
        output,
        "================================================================================\n"
    )
    .unwrap();
    writeln!(
        output,
        "This appendix is rendered from {MANIFEST}. Asset, package-tarball, source-map,"
    )
    .unwrap();
    writeln!(
        output,
        "and license-file SHA-256 identities are part of the deterministic notice contract.\n"
    )
    .unwrap();
    for (asset, licenses) in validated {
        writeln!(
            output,
            "--------------------------------------------------------------------------------"
        )
        .unwrap();
        writeln!(output, "{} {} ({})", asset.name, asset.version, asset.spdx).unwrap();
        writeln!(
            output,
            "--------------------------------------------------------------------------------"
        )
        .unwrap();
        writeln!(output, "Source: {}", asset.source).unwrap();
        writeln!(output, "Asset: {}", asset.path).unwrap();
        writeln!(output, "npm bundle member: {}", asset.tarball_member).unwrap();
        writeln!(output, "Bundle SHA-256: {}", asset.sha256).unwrap();
        if let Some(inventory) = &asset.inventory {
            writeln!(output, "npm tarball: {}", inventory.npm_tarball).unwrap();
            writeln!(
                output,
                "npm tarball SHA-256: {}",
                inventory.npm_tarball_sha256
            )
            .unwrap();
            writeln!(output, "Source map: {}", inventory.source_map).unwrap();
            writeln!(output, "Source-map file: {}", inventory.source_map_file).unwrap();
            writeln!(output, "Source-map evidence: {}", inventory.source_map_path).unwrap();
            writeln!(
                output,
                "Source-map SHA-256: {}",
                inventory.source_map_sha256
            )
            .unwrap();
            writeln!(
                output,
                "Upstream yarn.lock evidence: {}",
                inventory.lockfile_path
            )
            .unwrap();
            writeln!(
                output,
                "Upstream yarn.lock SHA-256: {}",
                inventory.lockfile_sha256
            )
            .unwrap();
            writeln!(output, "Inventory basis: {}", inventory.basis).unwrap();
            writeln!(
                output,
                "Embedded component inventory ({} packages, {} mapped sources)",
                inventory.embedded_component_count, inventory.embedded_source_count
            )
            .unwrap();
        }
        writeln!(output).unwrap();
        for (component, license) in asset.component.iter().zip(licenses) {
            writeln!(
                output,
                "  Component: {} {} ({}; {} mapped sources)",
                component.name, component.version, component.spdx, component.source_count
            )
            .unwrap();
            writeln!(output, "  Source: {}", component.source).unwrap();
            writeln!(output, "  Tarball: {}", component.tarball).unwrap();
            writeln!(output, "  Tarball SHA-256: {}", component.tarball_sha256).unwrap();
            writeln!(output, "  Tarball evidence: {}", component.tarball_path).unwrap();
            writeln!(output, "  npm integrity: {}", component.integrity).unwrap();
            writeln!(output, "  License file: {}", component.license_path).unwrap();
            writeln!(
                output,
                "  License archive member: {}",
                component.license_member
            )
            .unwrap();
            writeln!(output, "  License SHA-256: {}\n", component.license_sha256).unwrap();
            output.push_str(
                std::str::from_utf8(&license)
                    .map_err(|error| format!("validated license became invalid UTF-8: {error}"))?,
            );
            if !output.ends_with('\n') {
                output.push('\n');
            }
            output.push('\n');
        }
    }
    while output.ends_with('\n') {
        output.pop();
    }
    output.push('\n');
    Ok(output.into_bytes())
}

/// Render cargo-about twice, require byte-identical normalized output, then append
/// the authenticated vendored-JavaScript inventory.
/// # Errors
/// Generator failure, nondeterminism, or vendored evidence drift fails closed.
pub fn render(root: &Path, cargo_about: &Path) -> Result<Vec<u8>, String> {
    render_with_identity(
        root,
        cargo_about,
        production_generator_sha256()?,
        CARGO_ABOUT_VERSION,
    )
}

/// Lower-level deterministic renderer used by tests with an authenticated fixture generator.
/// Production callers must use [`render`] so the release generator identity stays pinned.
/// # Errors
/// Generator identity, execution, nondeterminism, or vendored evidence drift fails closed.
#[doc(hidden)]
pub fn render_with_identity(
    root: &Path,
    cargo_about: &Path,
    expected_sha256: &str,
    expected_version: &str,
) -> Result<Vec<u8>, String> {
    render_with_identity_and_provenance(
        root,
        cargo_about,
        expected_sha256,
        expected_version,
        UPSTREAM_PROVENANCE,
    )
}

/// Test-only identity/provenance-injected deterministic renderer.
/// # Errors
/// Generator identity, execution, provenance, or vendored evidence drift fails closed.
#[doc(hidden)]
pub fn render_with_identity_and_provenance(
    root: &Path,
    cargo_about: &Path,
    expected_sha256: &str,
    expected_version: &str,
    provenance_json: &str,
) -> Result<Vec<u8>, String> {
    for required in ["Cargo.lock", "about.toml", "about.hbs"] {
        regular_file(root, required)?;
    }
    let executable = authenticated_generator(cargo_about, expected_sha256, expected_version)?;
    let environment = isolated_cargo_environment()?;
    let first = cargo_notices(root, &executable, &environment)?;
    let second = cargo_notices(root, &executable, &environment)?;
    if first != second {
        return Err("cargo-about output is not deterministic".into());
    }
    let mut output = first;
    output.extend(render_vendored_with_provenance(root, provenance_json)?);
    Ok(output)
}

/// Compare a fresh deterministic rendering with the committed release notice.
/// # Errors
/// Missing, unreadable, nondeterministic, or stale notices fail closed.
pub fn verify(root: &Path, cargo_about: &Path) -> Result<(), String> {
    let expected = regular_file(root, NOTICE)?;
    let actual = render(root, cargo_about)?;
    if expected != actual {
        return Err(
            "THIRD-PARTY-NOTICES.txt is stale; run notices-render with the pinned cargo-about"
                .into(),
        );
    }
    Ok(())
}

/// Test-only identity-injected counterpart to [`verify`].
/// # Errors
/// Generator identity, rendering, or committed notice drift fails closed.
#[doc(hidden)]
pub fn verify_with_identity(
    root: &Path,
    cargo_about: &Path,
    expected_sha256: &str,
    expected_version: &str,
) -> Result<(), String> {
    let expected = regular_file(root, NOTICE)?;
    let actual = render_with_identity(root, cargo_about, expected_sha256, expected_version)?;
    if expected != actual {
        return Err(
            "THIRD-PARTY-NOTICES.txt is stale; run notices-render with the pinned cargo-about"
                .into(),
        );
    }
    Ok(())
}
