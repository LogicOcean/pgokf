// SPDX-License-Identifier: AGPL-3.0-only
//! Assembly of a workspace tree from resolved concept records: pure
//! functions over plain data, so every layout is unit-tested and a build
//! against an unchanged catalog is byte-identical.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use anyhow::{Result, anyhow};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::profile::{Profile, Shape, Target};
use crate::selection::{ConceptRecord, Selection, Snapshot, yaml_string};

/// Agent Skills limits for `SKILL.md` frontmatter.
const NAME_MAX: usize = 64;
const DESCRIPTION_MAX: usize = 1024;
/// How much concept text a prompt bundle inlines into the system prompt.
const PROMPT_BUDGET: usize = 24_000;
/// The manifest and lockfile names of spec §21.
pub const MANIFEST_FILE: &str = "okf-workspace.yaml";
pub const LOCK_FILE: &str = "okf-workspace.lock";

/// What to build.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub target: Target,
    /// The package name (slugged to Agent Skills rules).
    pub name: String,
    /// A display title; defaults to the name.
    pub title: Option<String>,
    /// The catalog's display name, for the index.
    pub catalog_name: String,
    /// `FROM` line of the Ollama Modelfile.
    pub base_model: Option<String>,
}

/// One file of the tree, path relative to the workspace root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginFile {
    pub path: String,
    #[serde(skip)]
    pub bytes: Vec<u8>,
    pub sha256: String,
}

/// A built workspace tree.
#[derive(Debug, Clone, Serialize)]
pub struct Plugin {
    pub target: String,
    pub name: String,
    /// The directory the harness reads (`.claude/skills/<name>`, ...).
    pub root: String,
    pub files: Vec<PluginFile>,
    pub concept_count: usize,
    /// Concepts the tree contains, in index order.
    pub concepts: Vec<ConceptRecord>,
}

impl Plugin {
    /// Total size of the tree in bytes.
    #[must_use]
    pub fn size(&self) -> usize {
        self.files.iter().map(|f| f.bytes.len()).sum()
    }
}

/// An Agent Skills `name`: lowercase letters, digits, and single hyphens,
/// 1 to 64 characters, no leading or trailing hyphen.
#[must_use]
pub fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending_hyphen = false;
    for ch in name.trim().chars() {
        let lower: Vec<char> = ch.to_lowercase().collect();
        for c in lower {
            if c.is_ascii_alphanumeric() {
                if pending_hyphen && !out.is_empty() {
                    out.push('-');
                }
                pending_hyphen = false;
                out.push(c);
            } else {
                pending_hyphen = true;
            }
        }
    }
    out.truncate(NAME_MAX);
    let trimmed = out.trim_end_matches('-').to_owned();
    if trimmed.is_empty() {
        DEFAULT_NAME.to_owned()
    } else {
        trimmed
    }
}

/// Where a shape puts its index and its content files.
struct Layout {
    root: String,
    index_path: String,
    content_dir: String,
}

fn layout(profile: &Profile, name: &str) -> Layout {
    match profile.shape {
        Shape::Skills => {
            let root = format!("{}/{name}", profile.root);
            Layout {
                index_path: format!("{root}/SKILL.md"),
                content_dir: format!("{root}/references"),
                root,
            }
        }
        Shape::InstructionFile => Layout {
            root: String::new(),
            index_path: "AGENTS.md".to_owned(),
            content_dir: "knowledge".to_owned(),
        },
        Shape::PromptBundle => Layout {
            root: profile.root.to_owned(),
            index_path: format!("{}/INDEX.md", profile.root),
            content_dir: format!("{}/knowledge", profile.root),
        },
        Shape::Generic => Layout {
            root: profile.root.to_owned(),
            index_path: format!("{}/INDEX.md", profile.root),
            content_dir: format!("{}/concepts", profile.root),
        },
    }
}

/// Assemble the tree for a target from resolved records (with content).
///
/// # Errors
///
/// No records, or a record without content.
pub fn assemble(
    options: &BuildOptions,
    selection: &Selection,
    snapshot: &Snapshot,
    records: &[ConceptRecord],
) -> Result<Plugin> {
    if records.is_empty() {
        return Err(anyhow!("the selection matched no visible concept"));
    }
    if let Some(empty) = records.iter().find(|r| r.bytes.is_empty()) {
        return Err(anyhow!(
            "concept {}:{} has no content loaded",
            empty.bundle_id,
            empty.concept_id
        ));
    }
    let profile = Profile::of(options.target);
    let name = package_name(&options.name)?;
    let base_model = validated_base_model(options.base_model.as_deref())?;
    let title = options
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| name.clone());
    let layout = layout(profile, &name);
    let multi_bundle = records
        .iter()
        .map(|r| r.bundle_id)
        .collect::<BTreeSet<_>>()
        .len()
        > 1;

    let (listed, content, entries) = content_files(&layout, records, multi_bundle)?;

    let index_dir = layout.index_path.rsplit_once('/').map_or("", |(d, _)| d);
    let index = match profile.shape {
        Shape::Skills => skill_md(&name, &title, options, selection, records, &listed),
        Shape::InstructionFile => agents_md(&title, options, selection, records, &listed),
        Shape::PromptBundle | Shape::Generic => index_md(
            &title,
            options,
            selection,
            records,
            &listed,
            index_dir,
            &layout.content_dir,
        ),
    };
    let mut files = vec![file(layout.index_path.clone(), index.into_bytes())];
    if profile.shape == Shape::PromptBundle {
        let prompt = system_prompt(&title, options, records);
        files.push(file(
            format!("{}/Modelfile", layout.root),
            modelfile(&base_model, &prompt).into_bytes(),
        ));
        files.push(file(
            format!("{}/system-prompt.md", layout.root),
            prompt.into_bytes(),
        ));
    }
    files.extend(content);
    files.push(file(
        MANIFEST_FILE.to_owned(),
        manifest_yaml(&name, options.target, selection).into_bytes(),
    ));
    files.push(file(
        LOCK_FILE.to_owned(),
        lockfile(&name, options.target, &layout.root, snapshot, &entries).into_bytes(),
    ));
    ensure_unique_paths(&files)?;

    Ok(Plugin {
        target: options.target.id().to_owned(),
        name,
        root: layout.root,
        files,
        concept_count: records.len(),
        concepts: records.to_vec(),
    })
}

/// The listing the index renders, the content files, and the lockfile
/// entries, in one order.
type ContentFiles<'a> = (
    Vec<(String, &'a ConceptRecord)>,
    Vec<PluginFile>,
    Vec<serde_json::Value>,
);

/// One file per concept (namespaced by bundle when several are mixed), the
/// listing the index renders, and the lockfile entries.
fn content_files<'a>(
    layout: &Layout,
    records: &'a [ConceptRecord],
    multi_bundle: bool,
) -> Result<ContentFiles<'a>> {
    let bundle_dirs = bundle_directories(records);
    let listed: Vec<(String, &ConceptRecord)> = records
        .iter()
        .map(|record| {
            let path = tree_path(&record.path)?;
            let relative = if multi_bundle {
                format!("{}/{path}", bundle_dirs[&record.bundle_id])
            } else {
                path
            };
            Ok((relative, record))
        })
        .collect::<Result<Vec<_>>>()?;
    let content: Vec<PluginFile> = listed
        .iter()
        .map(|(relative, record)| {
            file(
                format!("{}/{relative}", layout.content_dir),
                record.bytes.clone(),
            )
        })
        .collect();
    let entries: Vec<serde_json::Value> = listed
        .iter()
        .zip(&content)
        .map(|((_, record), f)| {
            json!({
                "bundle_id": record.bundle_id,
                "concept_id": record.concept_id,
                "path": record.path,
                "file": f.path,
                "file_hash": record.file_hash,
                "content_sha256": f.sha256,
                "exact": record.exact,
            })
        })
        .collect();
    Ok((listed, content, entries))
}

/// A directory name per bundle: its slugged name, with the id appended when
/// two bundles slug alike, so files of different bundles never collide.
fn bundle_directories(records: &[ConceptRecord]) -> BTreeMap<i64, String> {
    let mut names: BTreeMap<i64, &str> = BTreeMap::new();
    for r in records {
        names.entry(r.bundle_id).or_insert(&r.bundle_name);
    }
    let mut taken: BTreeSet<String> = BTreeSet::new();
    let mut dirs = BTreeMap::new();
    for (id, name) in names {
        let base = slug(name);
        let dir = if taken.contains(&base) {
            format!("{base}-{id}")
        } else {
            base
        };
        taken.insert(dir.clone());
        dirs.insert(id, dir);
    }
    dirs
}

/// A concept path as written into the tree: the parser already refuses
/// `..` and absolute forms, and any remaining separator-like or control
/// character (a backslash is a separator to Windows extractors) becomes an
/// underscore, so the tree is safe on every platform. The lockfile keeps
/// the catalog path beside the file name.
fn tree_path(path: &str) -> Result<String> {
    let cleaned: String = path
        .chars()
        .map(|c| if c == '\\' || c.is_control() { '_' } else { c })
        .collect();
    let segments: Vec<&str> = cleaned.split('/').collect();
    if segments
        .iter()
        .any(|seg| seg.is_empty() || *seg == "." || *seg == "..")
    {
        return Err(anyhow!("concept path {path:?} cannot be written as a file"));
    }
    Ok(cleaned)
}

/// Two files at one path would make the archive invalid or silently drop
/// one of them; refuse the build instead.
fn ensure_unique_paths(files: &[PluginFile]) -> Result<()> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for f in files {
        if !seen.insert(f.path.as_str()) {
            return Err(anyhow!(
                "two files would be written at {}; rename the package or narrow the selection",
                f.path
            ));
        }
    }
    Ok(())
}

/// The package name as an Agent Skills `name`, refusing names that slug to
/// nothing rather than silently substituting the default.
fn package_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_NAME.to_owned());
    }
    let slugged = slug(trimmed);
    if slugged == DEFAULT_NAME && !trimmed.eq_ignore_ascii_case(DEFAULT_NAME) {
        return Err(anyhow!(
            "package name {trimmed:?} needs at least one letter or digit (a-z, 0-9)"
        ));
    }
    Ok(slugged)
}

const DEFAULT_NAME: &str = "okf-knowledge";

/// An Ollama model reference (`llama3.1`, `hf.co/org/model:Q8_0`): the only
/// characters such names use, so nothing else can reach the Modelfile.
fn validated_base_model(raw: Option<&str>) -> Result<String> {
    let model = raw
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("llama3.1");
    if model
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'))
    {
        Ok(model.to_owned())
    } else {
        Err(anyhow!("base model {model:?} is not a model reference"))
    }
}

/// A Markdown link destination: the plain form when it is safe, else the
/// `CommonMark` `<...>` form (spaces, parentheses, quotes).
fn link_destination(path: &str) -> String {
    if path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '~'))
    {
        path.to_owned()
    } else {
        format!("<{}>", path.replace('<', "%3C").replace('>', "%3E"))
    }
}

fn lockfile(
    name: &str,
    target: Target,
    root: &str,
    snapshot: &Snapshot,
    entries: &[serde_json::Value],
) -> String {
    let lock = json!({
        "version": 1,
        "name": name,
        "target": target.id(),
        "root": root,
        "catalog": {
            "version": snapshot.version,
            "sql_version": snapshot.sql_version,
            "bundles": snapshot.bundles,
        },
        "entries": entries,
    });
    serde_json::to_string_pretty(&lock).unwrap_or_default() + "\n"
}

fn file(path: String, bytes: Vec<u8>) -> PluginFile {
    let sha256 = sha256_hex(&bytes);
    PluginFile {
        path,
        bytes,
        sha256,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Distinct values in first-seen order, capped.
fn distinct<'a>(items: impl Iterator<Item = &'a str>, cap: usize) -> Vec<&'a str> {
    let mut seen: Vec<&str> = Vec::new();
    for item in items {
        if !seen.contains(&item) {
            seen.push(item);
            if seen.len() == cap {
                break;
            }
        }
    }
    seen
}

/// The one-paragraph description the harness reads at startup.
fn description(options: &BuildOptions, selection: &Selection, records: &[ConceptRecord]) -> String {
    let types = distinct(records.iter().filter_map(|r| r.concept_type.as_deref()), 4);
    let mut tag_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for r in records {
        for t in &r.tags {
            *tag_counts.entry(t.as_str()).or_default() += 1;
        }
    }
    let mut tags: Vec<(&str, usize)> = tag_counts.into_iter().collect();
    tags.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let tags: Vec<&str> = tags.iter().take(8).map(|(t, _)| *t).collect();
    let titles = distinct(
        records
            .iter()
            .filter_map(|r| r.title.as_deref())
            .filter(|t| !t.is_empty()),
        5,
    );
    let mut text = format!(
        "Knowledge from {} (a pgokf catalog): {} concept{}",
        options.catalog_name,
        records.len(),
        if records.len() == 1 { "" } else { "s" }
    );
    if !types.is_empty() {
        let _ = write!(text, " ({})", types.join(", "));
    }
    let _ = write!(text, " selected as {}.", selection.describe());
    if !tags.is_empty() {
        let _ = write!(text, " Use when the task involves {}.", tags.join(", "));
    }
    if !titles.is_empty() {
        let _ = write!(text, " Covers {}.", titles.join("; "));
    }
    text.push_str(" Read the reference file for a concept before relying on it.");
    truncate_chars(&text, DESCRIPTION_MAX)
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('\u{2026}');
    out
}

/// The listing every index shares: one line per concept, grouped by bundle
/// (in first-seen order, so a ranked selection keeps its order inside each
/// group).
fn listing(listed: &[(String, &ConceptRecord)], prefix: &str) -> String {
    let mut out = String::new();
    let mut bundles: Vec<i64> = Vec::new();
    for (_, r) in listed {
        if !bundles.contains(&r.bundle_id) {
            bundles.push(r.bundle_id);
        }
    }
    let multi = bundles.len() > 1;
    for bundle in bundles {
        if multi {
            let name = listed
                .iter()
                .find(|(_, r)| r.bundle_id == bundle)
                .map_or("", |(_, r)| r.bundle_name.as_str());
            let _ = writeln!(out, "\n### Bundle {name} (#{bundle})\n");
        }
        for (relative, r) in listed.iter().filter(|(_, r)| r.bundle_id == bundle) {
            listing_line(&mut out, relative, r, prefix);
        }
    }
    out
}

fn listing_line(out: &mut String, relative: &str, r: &ConceptRecord, prefix: &str) {
    let title = r.title.as_deref().unwrap_or(&r.concept_id);
    let mut meta: Vec<String> = Vec::new();
    if let Some(t) = &r.concept_type {
        meta.push(t.clone());
    }
    if !r.tags.is_empty() {
        meta.push(r.tags.join(", "));
    }
    meta.push(r.trust_tier.clone());
    let _ = write!(
        out,
        "- [{}]({})",
        title.replace('[', "\\[").replace(']', "\\]"),
        link_destination(&format!("{prefix}{relative}"))
    );
    let _ = write!(out, " \u{2014} {}", meta.join(" \u{00b7} "));
    if let Some(d) = r.description.as_deref().filter(|d| !d.is_empty()) {
        let _ = write!(out, ". {d}");
    }
    let _ = writeln!(out, " `({}:{})`", r.bundle_id, r.concept_id);
}

fn skill_md(
    name: &str,
    title: &str,
    options: &BuildOptions,
    selection: &Selection,
    records: &[ConceptRecord],
    listed: &[(String, &ConceptRecord)],
) -> String {
    let desc = description(options, selection, records);
    let bundles: Vec<String> = distinct(records.iter().map(|r| r.bundle_name.as_str()), 20)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut out = String::new();
    out.push_str("---\n");
    let _ = writeln!(out, "name: {name}");
    let _ = writeln!(out, "description: {}", yaml_string(&desc));
    out.push_str("metadata:\n");
    out.push_str("  okf-source: pgokf\n");
    let _ = writeln!(out, "  okf-catalog: {}", yaml_string(&options.catalog_name));
    let _ = writeln!(out, "  okf-bundles: {}", yaml_string(&bundles.join(", ")));
    let _ = writeln!(
        out,
        "  okf-selection: {}",
        yaml_string(&selection.describe())
    );
    out.push_str("---\n\n");
    let _ = writeln!(out, "# {title}\n");
    let _ = write!(
        out,
        "Catalog knowledge exported from **{}**: {} concept{}, {}. Each entry below is the \
         concept's own document under `references/`; open one when the task touches its subject, \
         and prefer `human-reviewed` entries when several overlap. Identities are `(bundle_id:concept_id)` \
         in the catalog, which the pgokf MCP server can query live (`get_concept`, `concept_search`, \
         `concept_neighbors`) for anything not included here.\n\n",
        options.catalog_name,
        records.len(),
        if records.len() == 1 { "" } else { "s" },
        selection.describe()
    );
    out.push_str("## Contents\n");
    out.push_str(&listing(listed, "references/"));
    out.push_str("\n## Provenance\n\n");
    let _ = writeln!(
        out,
        "Built by pgokf-workspace from the catalog. `{LOCK_FILE}` at the workspace root records the \
         catalog snapshot and a content hash per file; `{MANIFEST_FILE}` reproduces the selection. \
         Files marked reconstructed in the lockfile were rebuilt from indexed text because the bundle \
         was ingested without stored source."
    );
    out
}

fn agents_md(
    title: &str,
    options: &BuildOptions,
    selection: &Selection,
    records: &[ConceptRecord],
    listed: &[(String, &ConceptRecord)],
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# {title}\n");
    let _ = write!(
        out,
        "<!-- Generated by pgokf-workspace from the {} catalog ({}); merge into your own AGENTS.md as needed. -->\n\n",
        options.catalog_name,
        selection.describe()
    );
    let _ = write!(
        out,
        "## Catalog knowledge\n\nThe `knowledge/` directory holds {} concept{} from the **{}** catalog. \
         Consult the matching file before acting on its subject; prefer `human-reviewed` entries. \
         Identities are `(bundle_id:concept_id)` for the pgokf MCP server.\n\n",
        records.len(),
        if records.len() == 1 { "" } else { "s" },
        options.catalog_name
    );
    out.push_str("### Index\n");
    out.push_str(&listing(listed, "knowledge/"));
    out
}

fn index_md(
    title: &str,
    options: &BuildOptions,
    selection: &Selection,
    records: &[ConceptRecord],
    listed: &[(String, &ConceptRecord)],
    index_dir: &str,
    content_dir: &str,
) -> String {
    // Links relative to the index file's own directory.
    let prefix = content_dir
        .strip_prefix(index_dir)
        .map(|rest| rest.trim_start_matches('/'))
        .filter(|rest| !rest.is_empty())
        .map_or(String::new(), |rest| format!("{rest}/"));
    let mut out = String::new();
    let _ = writeln!(out, "# {title}\n");
    let _ = write!(
        out,
        "{} concept{} from the **{}** catalog ({}). Generated by pgokf-workspace.\n\n",
        records.len(),
        if records.len() == 1 { "" } else { "s" },
        options.catalog_name,
        selection.describe()
    );
    out.push_str("## Index\n");
    out.push_str(&listing(listed, &prefix));
    out
}

/// The bounded system prompt of a prompt bundle: the index, then as many
/// documents as fit the budget, most trusted first.
fn system_prompt(title: &str, options: &BuildOptions, records: &[ConceptRecord]) -> String {
    let mut out = format!(
        "You have curated knowledge from {} (a pgokf catalog), titled {title}. Use it when relevant; say so when it does not cover a question.\n\n",
        options.catalog_name
    );
    let mut ordered: Vec<&ConceptRecord> = records.iter().collect();
    ordered.sort_by_key(|r| match r.trust_tier.as_str() {
        "human-reviewed" => 0,
        "machine-confirmed" => 1,
        _ => 2,
    });
    let mut used = out.len();
    let mut included = 0;
    for r in &ordered {
        let text = String::from_utf8_lossy(&r.bytes);
        let body = strip_frontmatter(&text);
        let header = format!(
            "\n## {} ({}{})\n\n",
            r.title.as_deref().unwrap_or(&r.concept_id),
            r.concept_type.as_deref().unwrap_or("concept"),
            if r.tags.is_empty() {
                String::new()
            } else {
                format!("; {}", r.tags.join(", "))
            }
        );
        if used + header.len() + body.len() > PROMPT_BUDGET {
            continue;
        }
        used += header.len() + body.len();
        included += 1;
        out.push_str(&header);
        out.push_str(body.trim());
        out.push('\n');
    }
    if included < records.len() {
        let _ = write!(
            out,
            "\n({} of {} documents inlined; the rest are under knowledge/.)\n",
            included,
            records.len()
        );
    }
    out
}

fn modelfile(base: &str, prompt: &str) -> String {
    // Triple-quoted SYSTEM blocks end at the first """; the prompt is
    // Markdown that never needs that sequence, so it is neutralized.
    let safe = prompt.replace("\"\"\"", "'''");
    format!("FROM {base}\nPARAMETER temperature 0.2\nSYSTEM \"\"\"\n{safe}\n\"\"\"\n")
}

/// Drop a leading YAML frontmatter block.
fn strip_frontmatter(text: &str) -> &str {
    let rest = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(after) = rest
        .strip_prefix("---\n")
        .or_else(|| rest.strip_prefix("---\r\n"))
    else {
        return text;
    };
    let mut offset = 0;
    for line in after.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return &after[offset + line.len()..];
        }
        offset += line.len();
    }
    text
}

/// The manifest that reproduces this build (spec §21.1).
fn manifest_yaml(name: &str, target: Target, selection: &Selection) -> String {
    let mut out = String::new();
    out.push_str(
        "# okf-workspace.yaml - generated by pgokf. Rebuild the same tree from the web Plugins page\n\
         # or the MCP build_workspace_plugin tool; a `pgokf-workspace sync` command is planned for it.\n",
    );
    out.push_str("version: 1\n");
    let _ = writeln!(out, "name: {}", yaml_string(name));
    let _ = writeln!(out, "targets: [{}]", target.id());
    out.push_str("catalog:\n  url_env: OKF_PG_URL\n");
    let _ = write!(
        out,
        "policy:\n  trust: {}\n",
        if selection.verified_only {
            "verified-only"
        } else {
            "any"
        }
    );
    out.push_str("include:\n  - ");
    let mut fields: Vec<String> = Vec::new();
    if !selection.bundle_ids.is_empty() {
        let ids: Vec<String> = selection
            .bundle_ids
            .iter()
            .map(ToString::to_string)
            .collect();
        fields.push(format!("bundle_ids: [{}]", ids.join(", ")));
    }
    if !selection.types.is_empty() {
        fields.push(format!("types: [{}]", list(&selection.types)));
    }
    if !selection.tags.is_empty() {
        fields.push(format!("tags: [{}]", list(&selection.tags)));
    }
    if !selection.concept_ids.is_empty() {
        fields.push(format!("ids: [{}]", list(&selection.concept_ids)));
    }
    if let Some(q) = selection.query.as_deref().filter(|q| !q.trim().is_empty()) {
        fields.push(format!("query: {}", yaml_string(q.trim())));
    }
    fields.push(format!("limit: {}", selection.effective_limit()));
    let _ = writeln!(out, "{{ {} }}", fields.join(", "));
    out
}

fn list(values: &[String]) -> String {
    values
        .iter()
        .map(|v| yaml_string(v))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::BundleState;

    fn record(bundle_id: i64, id: &str, title: &str, tier: &str, body: &str) -> ConceptRecord {
        ConceptRecord {
            bundle_id,
            bundle_name: format!("bundle-{bundle_id}"),
            concept_id: id.to_owned(),
            path: format!("{id}.md"),
            title: Some(title.to_owned()),
            description: Some(format!("About {title}.")),
            concept_type: Some("Runbook".to_owned()),
            tags: vec!["ops".to_owned()],
            file_hash: "ff".to_owned(),
            trust_tier: tier.to_owned(),
            status: "stable".to_owned(),
            exact: true,
            bytes: format!("---\ntitle: {title}\n---\n\n# {title}\n\n{body}\n").into_bytes(),
        }
    }

    fn options(target: Target) -> BuildOptions {
        BuildOptions {
            target,
            name: "Ops Runbooks!".to_owned(),
            title: None,
            catalog_name: "acme".to_owned(),
            base_model: None,
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            version: "0.1.16".to_owned(),
            sql_version: "0.1.16".to_owned(),
            bundles: vec![BundleState {
                id: 1,
                name: "bundle-1".to_owned(),
                sync_hash: Some("h".to_owned()),
                last_synced_at: Some("2026-09-06T00:00:00Z".to_owned()),
            }],
        }
    }

    fn selection() -> Selection {
        Selection {
            tags: vec!["ops".to_owned()],
            ..Selection::default()
        }
    }

    #[test]
    fn slug_follows_the_agent_skills_name_rules() {
        // Arrange & Act & Assert
        assert_eq!(slug("Ops Runbooks!"), "ops-runbooks");
        assert_eq!(slug("--Weird__name--"), "weird-name");
        assert_eq!(slug(""), "okf-knowledge");
        assert_eq!(slug(&"a".repeat(80)).len(), NAME_MAX);
    }

    #[test]
    fn a_skills_package_has_skill_md_references_manifest_and_lockfile() {
        // Arrange
        let records = vec![
            record(1, "runbooks/a", "Alpha", "human-reviewed", "Do A."),
            record(1, "runbooks/b", "Beta", "unverified", "Do B."),
        ];

        // Act
        let plugin = assemble(
            &options(Target::ClaudeCode),
            &selection(),
            &snapshot(),
            &records,
        )
        .expect("assembles");

        // Assert
        let paths: Vec<&str> = plugin.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(plugin.root, ".claude/skills/ops-runbooks");
        assert_eq!(
            paths,
            vec![
                ".claude/skills/ops-runbooks/SKILL.md",
                ".claude/skills/ops-runbooks/references/runbooks/a.md",
                ".claude/skills/ops-runbooks/references/runbooks/b.md",
                MANIFEST_FILE,
                LOCK_FILE,
            ]
        );
        let skill = String::from_utf8(plugin.files[0].bytes.clone()).expect("utf-8");
        assert!(skill.starts_with("---\nname: ops-runbooks\ndescription: \"Knowledge from acme (a pgokf catalog): 2 concepts (Runbook)"));
        assert!(skill.contains("- [Alpha](references/runbooks/a.md) \u{2014} Runbook \u{00b7} ops \u{00b7} human-reviewed. About Alpha. `(1:runbooks/a)`"));
        let lock: serde_json::Value = serde_json::from_slice(&plugin.files[4].bytes).expect("json");
        assert_eq!(lock["target"], "claude-code");
        assert_eq!(lock["entries"][0]["content_sha256"], plugin.files[1].sha256);
        assert_eq!(lock["catalog"]["bundles"][0]["sync_hash"], "h");
    }

    #[test]
    fn multi_bundle_trees_namespace_files_by_bundle() {
        // Arrange
        let records = vec![
            record(1, "a", "Alpha", "unverified", "A"),
            record(2, "a", "Alpha two", "unverified", "A2"),
        ];

        // Act
        let plugin = assemble(&options(Target::Codex), &selection(), &snapshot(), &records)
            .expect("assembles");

        // Assert
        assert!(
            plugin
                .files
                .iter()
                .any(|f| f.path == ".agents/skills/ops-runbooks/references/bundle-1/a.md")
        );
        assert!(
            plugin
                .files
                .iter()
                .any(|f| f.path == ".agents/skills/ops-runbooks/references/bundle-2/a.md")
        );
    }

    #[test]
    fn instruction_file_and_prompt_bundle_shapes_lay_out_as_documented() {
        // Arrange
        let records = vec![record(1, "a", "Alpha", "human-reviewed", "Do A.")];

        // Act
        let agents = assemble(
            &options(Target::AgentsMd),
            &selection(),
            &snapshot(),
            &records,
        )
        .expect("assembles");
        let ollama = assemble(
            &BuildOptions {
                base_model: Some("qwen3:8b".to_owned()),
                ..options(Target::Ollama)
            },
            &selection(),
            &snapshot(),
            &records,
        )
        .expect("assembles");

        // Assert
        assert_eq!(agents.files[0].path, "AGENTS.md");
        assert_eq!(agents.files[1].path, "knowledge/a.md");
        assert!(
            String::from_utf8_lossy(&agents.files[0].bytes).contains("[Alpha](knowledge/a.md)")
        );
        let paths: Vec<&str> = ollama.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths[..4],
            [
                "okf-prompt/INDEX.md",
                "okf-prompt/Modelfile",
                "okf-prompt/system-prompt.md",
                "okf-prompt/knowledge/a.md"
            ]
        );
        let modelfile = String::from_utf8_lossy(&ollama.files[1].bytes);
        assert!(
            String::from_utf8_lossy(&ollama.files[0].bytes).contains("[Alpha](knowledge/a.md)")
        );
        assert!(modelfile.starts_with("FROM qwen3:8b\n"));
        assert!(modelfile.contains("## Alpha (Runbook; ops)"));
        assert!(modelfile.contains("Do A."));
    }

    #[test]
    fn the_same_input_builds_byte_identical_output() {
        // Arrange
        let records = vec![record(1, "a", "Alpha", "unverified", "A")];

        // Act
        let first = assemble(
            &options(Target::Generic),
            &selection(),
            &snapshot(),
            &records,
        )
        .expect("a");
        let second = assemble(
            &options(Target::Generic),
            &selection(),
            &snapshot(),
            &records,
        )
        .expect("b");

        // Assert
        assert_eq!(first.files, second.files);
        assert_eq!(first.files[0].path, "okf-knowledge/INDEX.md");
        assert_eq!(first.files[1].path, "okf-knowledge/concepts/a.md");
        assert!(String::from_utf8_lossy(&first.files[0].bytes).contains("[Alpha](concepts/a.md)"));
    }

    #[test]
    fn assemble_refuses_empty_selections_and_unloaded_content() {
        // Arrange
        let mut unloaded = record(1, "a", "Alpha", "unverified", "A");
        unloaded.bytes.clear();

        // Act & Assert
        assert!(assemble(&options(Target::Generic), &selection(), &snapshot(), &[]).is_err());
        assert!(
            assemble(
                &options(Target::Generic),
                &selection(),
                &snapshot(),
                &[unloaded]
            )
            .is_err()
        );
    }

    #[test]
    fn tree_paths_are_made_safe_and_bundle_directories_never_collide() {
        // Arrange
        let mut odd = record(1, "notes/a b (x)", "Odd", "unverified", "A");
        odd.path = "notes\\..\\a b (x).md".to_owned();
        odd.bundle_name = "tmp3".to_owned();
        let mut lower = record(1, "a", "Lower", "unverified", "A");
        lower.bundle_name = "tmp3".to_owned();
        let mut upper = record(2, "a", "Upper", "unverified", "A");
        upper.bundle_name = "Tmp3".to_owned();

        // Act
        let plugin = assemble(
            &options(Target::Generic),
            &selection(),
            &snapshot(),
            &[odd, lower, upper],
        )
        .expect("assembles");

        // Assert
        let paths: Vec<&str> = plugin.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            paths.contains(&"okf-knowledge/concepts/tmp3/notes_.._a b (x).md"),
            "{paths:?}"
        );
        assert!(paths.contains(&"okf-knowledge/concepts/tmp3/a.md"));
        assert!(paths.contains(&"okf-knowledge/concepts/tmp3-2/a.md"));
        let index = String::from_utf8_lossy(&plugin.files[0].bytes);
        assert!(
            index.contains("(<concepts/tmp3/notes_.._a b (x).md>)"),
            "{index}"
        );
        assert!(tree_path("a/../b.md").is_err());
    }

    #[test]
    fn duplicate_paths_bad_names_and_bad_models_are_refused() {
        // Arrange
        let twice = vec![
            record(1, "a", "One", "unverified", "A"),
            record(1, "a", "Two", "unverified", "B"),
        ];
        let unicode = BuildOptions {
            name: "\u{65e5}\u{672c}".to_owned(),
            ..options(Target::Generic)
        };
        let injected = BuildOptions {
            base_model: Some("llama3.1\nADAPTER /etc/passwd".to_owned()),
            ..options(Target::Ollama)
        };
        let one = vec![record(1, "a", "One", "unverified", "A")];

        // Act & Assert
        assert!(assemble(&options(Target::Generic), &selection(), &snapshot(), &twice).is_err());
        assert!(assemble(&unicode, &selection(), &snapshot(), &one).is_err());
        assert!(assemble(&injected, &selection(), &snapshot(), &one).is_err());
        assert_eq!(
            validated_base_model(Some("hf.co/Qwen/Qwen3:Q8_0")).expect("ok"),
            "hf.co/Qwen/Qwen3:Q8_0"
        );
        assert_eq!(package_name("  ").expect("default"), DEFAULT_NAME);
    }

    #[test]
    fn descriptions_stay_within_the_agent_skills_bound() {
        // Arrange: many long titles.
        let records: Vec<ConceptRecord> = (0..40)
            .map(|i| {
                record(
                    1,
                    &format!("c{i}"),
                    &"Very long title ".repeat(6),
                    "unverified",
                    "x",
                )
            })
            .collect();

        // Act
        let text = description(&options(Target::ClaudeCode), &selection(), &records);

        // Assert
        assert!(text.chars().count() <= DESCRIPTION_MAX);
    }

    #[test]
    fn ranked_multi_bundle_listings_group_each_bundle_once() {
        // Arrange: bundles interleave as a ranked query would order them.
        let records = vec![
            record(1, "a", "A", "unverified", "x"),
            record(2, "b", "B", "unverified", "x"),
            record(1, "c", "C", "unverified", "x"),
        ];

        // Act
        let plugin = assemble(
            &options(Target::AgentsMd),
            &selection(),
            &snapshot(),
            &records,
        )
        .expect("assembles");
        let index = String::from_utf8_lossy(&plugin.files[0].bytes);

        // Assert
        assert_eq!(index.matches("### Bundle").count(), 2);
        let first = index.find("### Bundle bundle-1").expect("b1");
        let second = index.find("### Bundle bundle-2").expect("b2");
        assert!(first < second);
        assert!(
            index.find("[C]").expect("c") < second,
            "C stays in bundle 1's group"
        );
    }

    #[test]
    fn manifest_reproduces_the_selection() {
        // Arrange
        let selection = Selection {
            bundle_ids: vec![2],
            types: vec!["Runbook".to_owned()],
            tags: vec!["ops".to_owned()],
            query: Some("failover".to_owned()),
            verified_only: true,
            limit: Some(50),
            ..Selection::default()
        };

        // Act
        let yaml = manifest_yaml("ops", Target::Cursor, &selection);

        // Assert
        assert!(yaml.contains("targets: [cursor]\n"));
        assert!(yaml.contains("  trust: verified-only\n"));
        assert!(yaml.contains("  - { bundle_ids: [2], types: [\"Runbook\"], tags: [\"ops\"], query: \"failover\", limit: 50 }\n"));
    }
}
