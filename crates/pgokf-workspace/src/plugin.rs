// SPDX-License-Identifier: AGPL-3.0-only
//! Assembly of a workspace tree from resolved concept records: pure
//! functions over plain data, so every layout is unit-tested and a build
//! against an unchanged catalog is byte-identical.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use anyhow::{Result, anyhow};
use serde::Serialize;
use serde_json::{Value, json};
use unicode_normalization::UnicodeNormalization as _;

use crate::profile::{
    AGENT_PLUGIN_MCP_SCHEMA, AGENT_PLUGIN_SCHEMA, CustomHarness, EnvRef, McpFormat, McpSpec,
    Profile, Shape, TOKEN_ENV, Target, TokenRef,
};
use crate::selection::{
    BuildReport, ConceptRecord, Selection, Snapshot, StaleConcept, engaged, sha256_hex, yaml_string,
};

/// Agent Skills limits for `SKILL.md` frontmatter.
const NAME_MAX: usize = 64;
const DESCRIPTION_MAX: usize = 1024;
/// How much concept text a prompt bundle inlines into the system prompt.
const PROMPT_BUDGET: usize = 24_000;
/// The manifest and lockfile names of spec §21.
pub const MANIFEST_FILE: &str = "okf-workspace.yaml";
pub const LOCK_FILE: &str = "okf-workspace.lock";
/// The freshness report's name, written when a build kept or excluded stale
/// concepts (spec §4.8).
pub const FRESHNESS_FILE: &str = "FRESHNESS.md";
/// The suffix of the generated warning file placed beside an exact-bytes
/// file whose concept the catalog reported as stale: the file's own stem
/// plus `.stale-warning.md` (`SKILL.md` → `SKILL.stale-warning.md`), so the
/// byte-exact materialization promise is never broken.
pub const STALE_WARNING_SUFFIX: &str = ".stale-warning.md";

/// The optional parts of a plugin beyond the knowledge itself.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Component {
    /// The harness's MCP server configuration for `pgokf-mcp`, so the
    /// agent can query the catalog live (targets that have one).
    Mcp,
    /// A guide to the catalog: identities, trust tiers, the MCP tools and
    /// the JSON API, written for the agent.
    Guide,
    /// A small CLI helper (`okf.sh`) over the JSON API, for harnesses
    /// without MCP.
    Tools,
}

impl Component {
    /// Parse `mcp`, `guide`, or `tools`.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        match id.trim() {
            "mcp" => Some(Self::Mcp),
            "guide" | "docs" | "documentation" => Some(Self::Guide),
            "tools" | "scripts" => Some(Self::Tools),
            _ => None,
        }
    }

    /// The identifier used in manifests and arguments.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::Guide => "guide",
            Self::Tools => "tools",
        }
    }

    /// Every component, in the order the UI lists them.
    #[must_use]
    pub const fn all() -> [Self; 3] {
        [Self::Mcp, Self::Guide, Self::Tools]
    }
}

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
    /// The extra parts to include.
    pub components: Vec<Component>,
    /// How the harness starts the MCP server (default `pgokf-mcp`).
    /// Meaningless with [`BuildOptions::mcp_url`], which needs no command.
    pub mcp_command: Option<String>,
    /// The endpoint of a `pgokf-mcp --http` server. When set, the harness's
    /// MCP configuration describes that remote server instead of starting a
    /// local one, and the bearer token is referenced, never written.
    pub mcp_url: Option<String>,
    /// The tenant the agent's session should use, when the catalog is
    /// tenant-scoped (a name, never a secret).
    pub tenant: Option<String>,
    /// Base URL of the pgokf web UI and JSON API, for the guide and the
    /// helper script.
    pub web_url: Option<String>,
    /// The harness a [`Target::Custom`] build is for; ignored for registry
    /// targets.
    pub harness: Option<CustomHarness>,
}

impl BuildOptions {
    /// The HTTP endpoint this build points at, if it points at one.
    #[must_use]
    pub fn remote_endpoint(&self) -> Option<&str> {
        self.mcp_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
    }

    /// The profile the build is laid out with: the registry's for a known
    /// target, the described harness's for [`Target::Custom`].
    ///
    /// # Errors
    ///
    /// A custom target without a harness description.
    pub fn profile(&self) -> Result<Profile<'_>> {
        match self.target {
            Target::Custom => self.harness.as_ref().map(CustomHarness::profile).ok_or_else(|| {
                anyhow!(
                    "target custom needs a harness: the agent's name and, for a skills package, \
                     the directory it reads skills from"
                )
            }),
            target => Profile::of(target)
                .copied()
                .ok_or_else(|| anyhow!("unknown target {target}")),
        }
    }
}

/// The MCP server name written into harness configurations.
pub const MCP_SERVER_NAME: &str = "pgokf";

/// One file of the tree, path relative to the workspace root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginFile {
    pub path: String,
    #[serde(skip)]
    pub bytes: Vec<u8>,
    pub sha256: String,
    /// Written with the executable bit (the CLI helper).
    pub executable: bool,
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
    /// How many of the concepts are skill packages copied whole.
    pub package_count: usize,
    /// Concepts the tree contains, in index order.
    pub concepts: Vec<ConceptRecord>,
    /// Stale concepts the tree keeps, labelled (the `warn` policy).
    pub warnings: Vec<StaleConcept>,
    /// Concepts the `exclude` policy dropped.
    pub excluded: Vec<StaleConcept>,
    /// Whether the catalog offered the freshness surface at all.
    pub freshness_available: bool,
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

/// Where a shape puts its index, its content files, the skill packages it
/// copies whole, and the scripts selected on their own.
struct Layout {
    root: String,
    index_path: String,
    content_dir: String,
    /// Skill packages go to `<packages_dir>/<name>/`: beside the knowledge
    /// skill for a native consumer, under the knowledge tree otherwise.
    packages_dir: String,
    /// A package script selected without its package.
    scripts_dir: String,
    /// Where `okf-workspace.yaml` and `okf-workspace.lock` go: the workspace
    /// root, or inside the plugin directory when the plugin is the unit.
    meta_dir: String,
}

impl Layout {
    fn meta_path(&self, file: &str) -> String {
        if self.meta_dir.is_empty() {
            file.to_owned()
        } else {
            format!("{}/{file}", self.meta_dir)
        }
    }
}

fn layout(profile: &Profile, name: &str) -> Layout {
    match profile.shape {
        // The plugin directory is the unit: everything, the manifest and
        // lockfile included, lives under `<name>/`, and the knowledge skill
        // is one of its skills.
        Shape::AgentPlugin => {
            let root = name.to_owned();
            let skill = format!("{root}/skills/{name}");
            Layout {
                index_path: format!("{skill}/SKILL.md"),
                content_dir: format!("{skill}/references"),
                packages_dir: format!("{root}/skills"),
                scripts_dir: format!("{skill}/scripts"),
                meta_dir: root.clone(),
                root,
            }
        }
        Shape::Skills => {
            let root = format!("{}/{name}", profile.root);
            Layout {
                index_path: format!("{root}/SKILL.md"),
                content_dir: format!("{root}/references"),
                packages_dir: profile.root.to_owned(),
                scripts_dir: format!("{root}/scripts"),
                meta_dir: String::new(),
                root,
            }
        }
        Shape::InstructionFile => Layout {
            root: String::new(),
            index_path: "AGENTS.md".to_owned(),
            content_dir: "knowledge".to_owned(),
            packages_dir: "knowledge/skills".to_owned(),
            scripts_dir: "knowledge".to_owned(),
            meta_dir: String::new(),
        },
        Shape::PromptBundle => Layout {
            root: profile.root.to_owned(),
            index_path: format!("{}/INDEX.md", profile.root),
            content_dir: format!("{}/knowledge", profile.root),
            packages_dir: format!("{}/skills", profile.root),
            scripts_dir: format!("{}/knowledge", profile.root),
            meta_dir: String::new(),
        },
        Shape::Generic => Layout {
            root: profile.root.to_owned(),
            index_path: format!("{}/INDEX.md", profile.root),
            content_dir: format!("{}/concepts", profile.root),
            packages_dir: format!("{}/skills", profile.root),
            scripts_dir: format!("{}/concepts", profile.root),
            meta_dir: String::new(),
        },
    }
}

/// Add the lockfile, last of all, so its `files` can hash every other file
/// of the tree - the index, the MCP entry, the guide, and `okf.sh`, which is
/// the only executable one and was the only one nothing covered.
#[allow(clippy::too_many_arguments)]
fn append_lockfile(
    files: &mut Vec<PluginFile>,
    layout: &Layout,
    name: &str,
    options: &BuildOptions,
    selection: &Selection,
    snapshot: &Snapshot,
    entries: &[serde_json::Value],
    report: &BuildReport,
    engaged: bool,
) {
    let lock = lockfile(
        name,
        options.target,
        options.harness.as_ref(),
        &layout.root,
        selection,
        snapshot,
        entries,
        files,
        report,
        engaged,
    );
    files.push(file(layout.meta_path(LOCK_FILE), lock.into_bytes()));
}

/// The records a build was handed: there must be some, and each must carry
/// the content it stands for.
///
/// # Errors
///
/// An empty selection, or a document whose source did not load.
fn validate_records(records: &[ConceptRecord]) -> Result<()> {
    if records.is_empty() {
        return Err(anyhow!("the selection matched no visible concept"));
    }
    // A resource may legitimately be empty - an `assets/.gitkeep`, an empty
    // script - and `load_sources` keeps it for that reason. Only a document
    // that loaded nothing is a fault; refusing an empty resource here made
    // one `.gitkeep` in a package poison every build that selected it.
    if let Some(empty) = records
        .iter()
        .find(|r| r.bytes.is_empty() && r.resource.is_none())
    {
        return Err(anyhow!(
            "concept {}:{} has no content loaded",
            empty.bundle_id,
            empty.concept_id
        ));
    }
    Ok(())
}

/// Everything about a request that can be judged before any content is
/// read: the URLs it carries, and whether the harness can do what it asks.
///
/// # Errors
///
/// A malformed web or MCP URL, both ways of reaching the MCP server at
/// once, or an HTTP endpoint for a harness whose remote form is unknown.
fn validate_options(options: &BuildOptions, profile: &Profile) -> Result<()> {
    validated_web_url(options.web_url.as_deref())?;
    validated_mcp_url(options.mcp_url.as_deref())?;
    if options.remote_endpoint().is_none() {
        return Ok(());
    }
    if options
        .mcp_command
        .as_deref()
        .is_some_and(|c| !c.trim().is_empty())
    {
        return Err(anyhow!(
            "mcp_url and mcp_command are alternatives: an HTTP endpoint is reached, not started"
        ));
    }
    if let Some(spec) = profile.mcp
        && spec.remote.is_none()
    {
        return Err(anyhow!(
            "{} documents no remote MCP server form, so this build cannot point at an HTTP \
             endpoint; leave mcp_url out to configure the stdio server",
            profile.label
        ));
    }
    Ok(())
}

/// Assemble the tree for a target from resolved records (with content).
///
/// # Errors
///
/// No records, a record without content, or a request
/// [`validate_options`] refuses.
pub fn assemble(
    options: &BuildOptions,
    selection: &Selection,
    snapshot: &Snapshot,
    records: &[ConceptRecord],
) -> Result<Plugin> {
    assemble_with_report(
        options,
        selection,
        snapshot,
        records,
        &BuildReport::default(),
    )
}

/// [`assemble`] with the resolution stage's report: the closure summary and
/// the excluded concepts the manifest and lockfile record, and the freshness
/// availability that decides whether states are claimed at all.
///
/// # Errors
///
/// No records, a record without content, or a request
/// [`validate_options`] refuses.
#[allow(clippy::too_many_lines)]
pub fn assemble_with_report(
    options: &BuildOptions,
    selection: &Selection,
    snapshot: &Snapshot,
    records: &[ConceptRecord],
    report: &BuildReport,
) -> Result<Plugin> {
    validate_records(records)?;
    let profile = options.profile()?;
    let profile = &profile;
    let name = package_name(&options.name)?;
    let base_model = validated_base_model(options.base_model.as_deref())?;
    validate_options(options, profile)?;
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

    let discovers_by_directory = matches!(profile.shape, Shape::AgentPlugin | Shape::Skills);
    let engaged = engaged(selection, records);
    let materialized = content_files(
        &layout,
        &name,
        records,
        multi_bundle,
        discovers_by_directory,
        engaged,
    )?;
    let Materialized {
        listed,
        packages,
        files: content,
        entries,
    } = materialized;

    // The freshness report, when the build kept or excluded stale concepts
    // (a stale package member counts). A build with nothing stale writes
    // none, so it stays byte-identical to one from before the freshness
    // surface existed.
    let stale: Vec<&ConceptRecord> = records.iter().filter(|r| r.freshness.is_stale()).collect();
    let member_stale = records
        .iter()
        .filter_map(|r| r.package.as_ref())
        .flat_map(|p| p.member_freshness.values())
        .filter(|f| f.is_stale())
        .count();
    let freshness_doc = if stale.is_empty() && member_stale == 0 && report.excluded.is_empty() {
        None
    } else {
        Some(freshness_md(records, &report.excluded, selection))
    };
    let index_dir = layout.index_path.rsplit_once('/').map_or("", |(d, _)| d);
    let freshness_link = freshness_doc.as_ref().map(|_| {
        format!(
            "{}{FRESHNESS_FILE}",
            relative_prefix(index_dir, &layout.meta_dir)
        )
    });
    let package_prefix = relative_prefix(index_dir, &layout.packages_dir);
    let index = render_index(&IndexInputs {
        profile,
        name: &name,
        title: &title,
        options,
        selection,
        records,
        listed: &listed,
        packages: &packages,
        package_prefix: &package_prefix,
        freshness_link: freshness_link.as_deref(),
        stale_count: stale.len() + member_stale + report.excluded.len(),
    });
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
    if let Some(doc) = freshness_doc {
        files.push(file(layout.meta_path(FRESHNESS_FILE), doc.into_bytes()));
    }
    files.extend(extras(profile, &layout, options)?);
    let wrote_an_entry = profile.mcp.is_some() && options.components.contains(&Component::Mcp);
    files.extend(meta_files(
        &layout,
        &name,
        options,
        selection,
        snapshot,
        &entries,
        wrote_an_entry,
        engaged,
    ));
    if profile.shape == Shape::AgentPlugin {
        // Last, so its version digest covers every other file of the plugin.
        let manifest = plugin_json(&name, options, selection, records, snapshot, &files);
        files.insert(
            1,
            file(
                format!("{}/plugin.json", layout.root),
                manifest.into_bytes(),
            ),
        );
    }
    append_lockfile(
        &mut files, &layout, &name, options, selection, snapshot, &entries, report, engaged,
    );
    ensure_unique_paths(&files)?;

    let mut warnings: Vec<StaleConcept> = stale
        .iter()
        .map(|r| StaleConcept::of(r, selection.require_closure))
        .collect();
    for record in records {
        let Some(package) = &record.package else {
            continue;
        };
        for (member, freshness) in &package.member_freshness {
            if freshness.is_stale() {
                warnings.push(StaleConcept::member(
                    record,
                    member,
                    freshness,
                    selection.require_closure,
                ));
            }
        }
    }

    Ok(Plugin {
        target: options.target.id().to_owned(),
        name,
        root: layout.root,
        files,
        concept_count: records.len(),
        package_count: packages.len(),
        concepts: records.to_vec(),
        warnings,
        excluded: report.excluded.clone(),
        freshness_available: report.freshness_available,
    })
}

/// Everything the index file of a shape is rendered from.
struct IndexInputs<'a> {
    profile: &'a Profile<'a>,
    name: &'a str,
    title: &'a str,
    options: &'a BuildOptions,
    selection: &'a Selection,
    records: &'a [ConceptRecord],
    listed: &'a [(String, &'a ConceptRecord)],
    packages: &'a [(String, &'a ConceptRecord)],
    package_prefix: &'a str,
    /// The `FRESHNESS.md` link relative to the index, when the build wrote
    /// one, with how many concepts it names (stale kept plus excluded).
    freshness_link: Option<&'a str>,
    stale_count: usize,
}

/// The index file in the shape's own form: a `SKILL.md`, an `AGENTS.md`, or
/// an `INDEX.md`.
fn render_index(inputs: &IndexInputs<'_>) -> String {
    let IndexInputs {
        profile,
        name,
        title,
        options,
        selection,
        records,
        listed,
        packages,
        package_prefix,
        freshness_link,
        stale_count,
    } = *inputs;
    match profile.shape {
        Shape::AgentPlugin | Shape::Skills => skill_md(
            name,
            title,
            options,
            selection,
            records,
            listed,
            packages,
            package_prefix,
            freshness_link,
            stale_count,
        ),
        Shape::InstructionFile => agents_md(
            title,
            options,
            selection,
            records,
            listed,
            packages,
            package_prefix,
            freshness_link,
            stale_count,
        ),
        Shape::PromptBundle | Shape::Generic => index_md(
            title,
            options,
            selection,
            records,
            listed,
            packages,
            package_prefix,
            freshness_link,
            stale_count,
        ),
    }
}

/// What `content_files` produces: the document listing the index renders,
/// the packages (directory name and record), every content file, and the
/// lockfile entries.
struct Materialized<'a> {
    listed: Vec<(String, &'a ConceptRecord)>,
    packages: Vec<(String, &'a ConceptRecord)>,
    files: Vec<PluginFile>,
    entries: Vec<serde_json::Value>,
}

/// One file per document concept (namespaced by bundle when several are
/// mixed), one directory per skill package copied byte for byte (its
/// `SKILL.md` and every script, reference, and asset), and a resource
/// selected on its own at its catalog path (scripts under the scripts
/// directory, executable); plus the listing the index renders and the
/// lockfile entries.
#[allow(clippy::too_many_lines)]
fn content_files<'a>(
    layout: &Layout,
    plugin_name: &str,
    records: &'a [ConceptRecord],
    multi_bundle: bool,
    discovers_by_directory: bool,
    engaged: bool,
) -> Result<Materialized<'a>> {
    let bundle_dirs = bundle_directories(records);
    let package_dirs = package_directories(records, plugin_name);
    if discovers_by_directory {
        // A harness that discovers skills by directory name skips a package
        // whose SKILL.md name differs from its directory, so a suffixed
        // directory would ship a package the client never loads.
        for record in records {
            let Some(package) = &record.package else {
                continue;
            };
            let dir = &package_dirs[&(record.bundle_id, record.concept_id.as_str())];
            if *dir != slug(&package.name) {
                return Err(anyhow!(
                    "skill package {} (bundle {}) cannot be written as {dir}: its name collides \
                     with {}; rename the plugin or drop the duplicate from the selection",
                    package.name,
                    record.bundle_id,
                    if *dir == format!("{}-{}", slug(&package.name), record.bundle_id) {
                        "the plugin's own directory or another package of that name"
                    } else {
                        "another package of that name"
                    }
                ));
            }
        }
    }
    let index_dir = layout.index_path.rsplit_once('/').map_or("", |(d, _)| d);
    let mut listed = Vec::new();
    let mut packages = Vec::new();
    let mut files = Vec::new();
    let mut entries = Vec::new();

    for record in records {
        if let Some(package) = &record.package {
            let dir = &package_dirs[&(record.bundle_id, record.concept_id.as_str())];
            let base = format!("{}/{dir}", layout.packages_dir);
            let manifest = file(format!("{base}/SKILL.md"), record.bytes.clone());
            entries.push(lock_entry(
                record,
                &manifest,
                None,
                None,
                Some((&package.hash, dir)),
                engaged,
            ));
            files.push(manifest);
            for resource in &package.resources {
                let member_freshness = package.member_freshness.get(&resource.concept_id);
                let mut f = file(
                    format!("{base}/{}", tree_path(&resource.path)?),
                    resource.bytes.clone(),
                );
                f.executable = resource.is_script();
                entries.push(lock_entry(
                    record,
                    &f,
                    Some(resource),
                    member_freshness,
                    Some((&package.hash, dir)),
                    engaged,
                ));
                // A stale member keeps its exact bytes like the rest of the
                // package; its own warning sits beside the member file, so a
                // fresh package no longer ships a stale member silently.
                if member_freshness.is_some_and(crate::selection::Freshness::is_stale) {
                    let warning_path = adjacent_warning_path(&f.path);
                    files.push(file(
                        warning_path.clone(),
                        stale_member_warning_doc(record, resource, &warning_path).into_bytes(),
                    ));
                }
                files.push(f);
            }
            // The package's bytes are exact and stay exact: a stale package
            // gets a generated warning beside its SKILL.md, never an edit.
            if record.freshness.is_stale() {
                let warning_path = adjacent_warning_path(&format!("{base}/SKILL.md"));
                files.push(file(
                    warning_path.clone(),
                    stale_warning_doc(record, &warning_path).into_bytes(),
                ));
            }
            packages.push((dir.clone(), record));
            continue;
        }
        let path = tree_path(&record.path)?;
        let relative = if multi_bundle {
            format!("{}/{path}", bundle_dirs[&record.bundle_id])
        } else {
            path
        };
        let is_script = record
            .resource
            .as_ref()
            .is_some_and(|r| r.class == "script");
        let dir = if is_script {
            &layout.scripts_dir
        } else {
            &layout.content_dir
        };
        let file_path = format!("{dir}/{relative}");
        if record.freshness.is_stale() && !record.exact {
            // A reconstructed document is generated bytes: the banner goes
            // in, after any frontmatter.
            let bytes = with_stale_banner(&record.bytes, &stale_banner(record));
            let mut f = file(file_path, bytes);
            f.executable = is_script;
            entries.push(lock_entry(record, &f, None, None, None, engaged));
            files.push(f);
        } else {
            let mut f = file(file_path.clone(), record.bytes.clone());
            f.executable = is_script;
            entries.push(lock_entry(record, &f, None, None, None, engaged));
            files.push(f);
            if record.freshness.is_stale() {
                // Stored exact bytes stay untouched; the warning is adjacent.
                let warning_path = adjacent_warning_path(&file_path);
                files.push(file(
                    warning_path.clone(),
                    stale_warning_doc(record, &warning_path).into_bytes(),
                ));
            }
        }
        // The index links to the file relative to its own directory.
        listed.push((
            format!("{}{relative}", relative_prefix(index_dir, dir)),
            record,
        ));
    }
    Ok(Materialized {
        listed,
        packages,
        files,
        entries,
    })
}

/// One lockfile entry per written file: the concept it came from (a package
/// member under its own id and file hash), its catalog hashes, and for a
/// package file the package hash and the directory the package was written
/// to (which differs from its name only when two packages collided). When
/// the build engaged the freshness surface, the entry also carries the
/// concept's effective state, reasons, revisions, and catalog generation -
/// a member's own state, never its package's - and how it entered the
/// selection.
fn lock_entry(
    record: &ConceptRecord,
    f: &PluginFile,
    member: Option<&crate::selection::ResourceFile>,
    member_freshness: Option<&crate::selection::Freshness>,
    package: Option<(&str, &str)>,
    engaged: bool,
) -> serde_json::Value {
    let mut entry = json!({
        "bundle_id": record.bundle_id,
        "concept_id": member.map_or(record.concept_id.as_str(), |m| m.concept_id.as_str()),
        "path": member.map_or(record.path.as_str(), |m| m.concept_id.as_str()),
        "file": f.path,
        "file_hash": member.map_or(record.file_hash.as_str(), |m| m.file_hash.as_str()),
        "content_sha256": f.sha256,
        "exact": record.exact,
    });
    if member.is_some() {
        entry["package_concept_id"] = json!(record.concept_id);
    } else if let Some(resource) = &record.resource {
        entry["package_concept_id"] = json!(resource.package_concept_id);
    }
    if let Some((hash, dir)) = package {
        entry["package_hash"] = json!(hash);
        entry["package_directory"] = json!(dir);
    }
    if engaged {
        let freshness = member_freshness.unwrap_or(&record.freshness);
        entry["freshness"] = json!({
            "state": freshness.state,
            "reasons": freshness.reasons,
            "scope": freshness.scope,
            "stale_since": freshness.stale_since,
            "observed_revision": freshness.observed_revision,
            "indexed_revision": freshness.indexed_revision,
            "published_revision": freshness.published_revision,
            "catalog_generation": freshness.catalog_generation,
            "last_reconciled_at": freshness.last_reconciled_at,
        });
        let origin = &record.origin;
        if origin.pick || origin.seed || origin.closure {
            entry["origin"] = json!({
                "pick": origin.pick,
                "seed": origin.seed,
                "closure": origin.closure,
            });
        }
    }
    entry
}

/// The deterministic warning file beside an exact-bytes file whose concept
/// is stale: the file's own stem plus [`STALE_WARNING_SUFFIX`], in the same
/// directory (`…/SKILL.md` → `…/SKILL.stale-warning.md`).
fn adjacent_warning_path(path: &str) -> String {
    let (dir, name) = path.rsplit_once('/').map_or(("", path), |(d, n)| (d, n));
    // The stem is the name before its last dot; a dotfile like `.gitkeep`
    // keeps its whole name.
    let stem = match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    };
    if dir.is_empty() {
        format!("{stem}{STALE_WARNING_SUFFIX}")
    } else {
        format!("{dir}/{stem}{STALE_WARNING_SUFFIX}")
    }
}

/// The machine-readable freshness line of a warning or banner: one JSON
/// object in an HTML comment, so a generated file states the catalog's
/// evidence without pretending to be prose.
fn freshness_json_line(
    bundle_id: i64,
    concept_id: &str,
    freshness: &crate::selection::Freshness,
) -> String {
    let evidence = json!({
        "bundle_id": bundle_id,
        "concept_id": concept_id,
        "state": freshness.state,
        "reasons": freshness.reasons,
        "scope": freshness.scope,
        "observed_revision": freshness.observed_revision,
        "indexed_revision": freshness.indexed_revision,
        "published_revision": freshness.published_revision,
        "catalog_generation": freshness.catalog_generation,
        "last_reconciled_at": freshness.last_reconciled_at,
    });
    format!(
        "<!-- pgokf-stale {} -->",
        serde_json::to_string(&evidence).unwrap_or_default()
    )
}

/// The revisions sentence shared by the banner and the warning file.
fn revision_sentence(freshness: &crate::selection::Freshness) -> String {
    let mut out = format!("state `{}`", freshness.state);
    if !freshness.reasons.is_empty() {
        let _ = write!(out, ", reasons: {}", freshness.reasons.join(", "));
    }
    let revision = |label: &str, value: &Option<String>| {
        value
            .as_deref()
            .map_or_else(|| format!("{label} unknown"), |v| format!("{label} `{v}`"))
    };
    let _ = write!(
        out,
        "; {}, {}, {}",
        revision("observed", &freshness.observed_revision),
        revision("indexed", &freshness.indexed_revision),
        revision("published", &freshness.published_revision),
    );
    if let Some(generation) = freshness.catalog_generation {
        let _ = write!(out, ", catalog generation {generation}");
    }
    out
}

/// The banner prepended to a stale reconstructed document (generated bytes,
/// so injecting a clearly-generated notice is safe).
fn stale_banner(record: &ConceptRecord) -> String {
    format!(
        "> [!WARNING]\n> **Stale catalog content** — when this workspace was built, the catalog \
         reported this concept as not fresh ({}). Rebuild the plugin to pick up the reconciled \
         version; see `FRESHNESS.md` for the full report.\n\n{}\n\n",
        revision_sentence(&record.freshness),
        freshness_json_line(record.bundle_id, &record.concept_id, &record.freshness),
    )
}

/// The standalone warning file placed beside a stale exact-bytes file (the
/// byte-exact materialization promise is never broken).
fn stale_warning_doc(record: &ConceptRecord, warning_path: &str) -> String {
    warning_doc(
        record.bundle_id,
        &record.concept_id,
        record.title.as_deref(),
        &record.path,
        &record.freshness,
        warning_path,
    )
}

/// The warning file beside one stale package member: the member's own
/// concept id and the member's own freshness evidence, never the package's.
fn stale_member_warning_doc(
    record: &ConceptRecord,
    member: &crate::selection::ResourceFile,
    warning_path: &str,
) -> String {
    let freshness = record
        .package
        .as_ref()
        .and_then(|p| p.member_freshness.get(&member.concept_id));
    let Some(freshness) = freshness else {
        return String::new();
    };
    warning_doc(
        record.bundle_id,
        &member.concept_id,
        None,
        &member.path,
        freshness,
        warning_path,
    )
}

/// The shared text of the adjacent warning files.
fn warning_doc(
    bundle_id: i64,
    concept_id: &str,
    title: Option<&str>,
    fallback_path: &str,
    freshness: &crate::selection::Freshness,
    warning_path: &str,
) -> String {
    let title = title.unwrap_or(concept_id);
    let referred = warning_path
        .strip_suffix(STALE_WARNING_SUFFIX)
        .and_then(|stem| {
            // The warning sits beside the file it refers to; recover the
            // file's name for the prose.
            stem.rsplit_once('/').map(|(_, name)| name.to_owned())
        })
        .unwrap_or_else(|| fallback_path.to_owned());
    format!(
        "# Stale content warning: {}\n\nGenerated by pgokf-workspace. When this workspace was \
         built, the catalog reported concept `({}:{})` as not fresh ({}). The document beside \
         this file (`{}` and, for a skill package, its whole directory) was materialized byte \
         for byte and is **not** modified; treat its content as out of date and rebuild the \
         plugin once the catalog reconciles. See `FRESHNESS.md` at the workspace root for every \
         stale entry.\n\n{}\n",
        title.replace('[', "\\[").replace(']', "\\]"),
        bundle_id,
        concept_id,
        revision_sentence(freshness),
        referred,
        freshness_json_line(bundle_id, concept_id, freshness),
    )
}

/// Prepend a banner to a generated document, keeping a leading frontmatter
/// block first so the file still parses.
fn with_stale_banner(bytes: &[u8], banner: &str) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return bytes.to_vec();
    };
    let body = strip_frontmatter(text);
    if body.len() < text.len()
        && let Some(head) = text.strip_suffix(body)
    {
        let mut out = head.to_owned();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(banner);
        out.push_str(body);
        return out.into_bytes();
    }
    let mut out = banner.to_owned();
    out.push_str(text);
    out.into_bytes()
}

/// The top-level freshness report: every stale concept the build kept
/// (labelled) or excluded - a stale package member under its own id and
/// evidence - with the catalog's evidence, in selection order.
fn freshness_md(
    records: &[ConceptRecord],
    excluded: &[StaleConcept],
    selection: &Selection,
) -> String {
    let mut out = String::from("# Freshness report\n\n");
    let _ = writeln!(
        out,
        "Generated by pgokf-workspace. This build ran under one catalog snapshot with stale \
         policy `{}`. The states below are what the catalog reported at build time; rebuild the \
         plugin to re-check, or call the `check_workspace_plugin_freshness` MCP tool with this \
         workspace's `{LOCK_FILE}`.",
        selection.stale_policy.id()
    );
    // Every stale thing the tree keeps: the stale records, and each stale
    // package member under its own concept id and freshness.
    let mut kept: Vec<(i64, &str, &crate::selection::Freshness)> = Vec::new();
    for record in records {
        if record.freshness.is_stale() {
            kept.push((record.bundle_id, &record.concept_id, &record.freshness));
        }
        if let Some(package) = &record.package {
            for (member, freshness) in &package.member_freshness {
                if freshness.is_stale() {
                    kept.push((record.bundle_id, member, freshness));
                }
            }
        }
    }
    if !kept.is_empty() {
        let _ = write!(
            out,
            "\n## Stale content ({})\n\n| concept | state | reasons | observed revision | indexed \
             revision | published revision | catalog generation |\n|---|---|---|---|---|---|---|\n",
            kept.len()
        );
        for (bundle_id, concept_id, freshness) in &kept {
            let cell = |value: &Option<String>| value.as_deref().unwrap_or("—").to_owned();
            let _ = writeln!(
                out,
                "| `({}:{})` | {} | {} | {} | {} | {} | {} |",
                bundle_id,
                concept_id,
                freshness.state,
                if freshness.reasons.is_empty() {
                    "—".to_owned()
                } else {
                    freshness.reasons.join(", ")
                },
                cell(&freshness.observed_revision),
                cell(&freshness.indexed_revision),
                cell(&freshness.published_revision),
                freshness
                    .catalog_generation
                    .map_or_else(|| "—".to_owned(), |g| g.to_string()),
            );
        }
        out.push_str(
            "\nExact packages and stored-source documents keep their bytes untouched; each stale \
             one - a stale package member included - has a generated `*.stale-warning.md` file \
             beside it. Reconstructed documents carry a banner at the top.\n",
        );
    }
    if !excluded.is_empty() {
        let _ = write!(
            out,
            "\n## Excluded by the `exclude` policy ({})\n\n| concept | role | state | reasons |\n|---|---|---|---|\n",
            excluded.len()
        );
        for concept in excluded {
            let _ = writeln!(
                out,
                "| `({}:{})` | {} | {} | {} |",
                concept.bundle_id,
                concept.concept_id,
                concept.role,
                concept.state,
                if concept.reasons.is_empty() {
                    "—".to_owned()
                } else {
                    concept.reasons.join(", ")
                },
            );
        }
    }
    out
}

/// A directory per skill package: its Agent Skills name (slugged when the
/// catalog's copy bends the rules), with the bundle id appended when two
/// packages share a name or one collides with the plugin's own directory.
fn package_directories<'a>(
    records: &'a [ConceptRecord],
    plugin_name: &str,
) -> BTreeMap<(i64, &'a str), String> {
    let mut taken: BTreeSet<String> = BTreeSet::from([plugin_name.to_owned()]);
    let mut dirs = BTreeMap::new();
    for r in records {
        let Some(package) = &r.package else {
            continue;
        };
        let base = slug(&package.name);
        let mut dir = base.clone();
        let mut n = 0;
        while taken.contains(&dir) {
            let suffix = if n == 0 {
                format!("-{}", r.bundle_id)
            } else {
                format!("-{}-{n}", r.bundle_id)
            };
            // Keep the suffixed directory a valid Agent Skills name: at most
            // NAME_MAX characters, no trailing hyphen.
            let keep = NAME_MAX.saturating_sub(suffix.len());
            let head = base
                .chars()
                .take(keep)
                .collect::<String>()
                .trim_end_matches('-')
                .to_owned();
            dir = format!("{head}{suffix}");
            n += 1;
        }
        taken.insert(dir.clone());
        dirs.insert((r.bundle_id, r.concept_id.as_str()), dir);
    }
    dirs
}

/// The link prefix from the directory of the index file to `dir`: climb
/// out of what the two paths do not share, then descend into `dir`.
fn relative_prefix(index_dir: &str, dir: &str) -> String {
    let from: Vec<&str> = index_dir.split('/').filter(|s| !s.is_empty()).collect();
    let to: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut prefix = "../".repeat(from.len() - common);
    for segment in &to[common..] {
        prefix.push_str(segment);
        prefix.push('/');
    }
    prefix
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
/// character (a backslash is a separator to Windows extractors, a colon a
/// drive letter) becomes an underscore, so the tree is safe on every
/// platform and the directory writer accepts every path the zip carries. The lockfile keeps
/// the catalog path beside the file name.
fn tree_path(path: &str) -> Result<String> {
    let cleaned: String = path
        .chars()
        .map(|c| {
            if c == '\\' || c == ':' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let segments: Vec<&str> = cleaned.split('/').collect();
    if segments.iter().any(|seg| !safe_segment(seg)) {
        return Err(anyhow!("concept path {path:?} cannot be written as a file"));
    }
    Ok(cleaned)
}

/// Whether one path segment is safe to write on every platform.
///
/// Windows strips trailing spaces and dots from each component, so a
/// segment of `".. "` becomes `..` there and climbs a directory - on the
/// extractor's machine, not ours. Format characters are excluded too: they
/// are not `char::is_control`, and a right-to-left override in the name of
/// a file the tree marks executable is a plain deception.
fn safe_segment(segment: &str) -> bool {
    let squared = segment.trim_end_matches([' ', '.']);
    !segment.is_empty()
        && !squared.is_empty()
        && squared != ".."
        && segment != "."
        && !segment
            .chars()
            .any(|c| c.is_control() || matches!(c, '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
}

/// Two files at one path would make the archive invalid or silently drop
/// one of them; refuse the build instead.
///
/// "One path" is judged the way a filesystem judges it, not the way a byte
/// comparison does: `A.md` and `a.md` are one file on macOS and Windows,
/// and `café.md` written two Unicode ways is one file on macOS. Comparing
/// exact strings let such a pair through, and the tree then either lost a
/// file on extraction or half-wrote itself. A path that is a directory
/// prefix of another is a collision too - one cannot be both.
fn ensure_unique_paths(files: &[PluginFile]) -> Result<()> {
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    for f in files {
        if let Some(first) = seen.insert(collision_key(&f.path), f.path.as_str()) {
            return Err(if first == f.path {
                anyhow!(
                    "two files would be written at {}; rename the package or narrow the selection",
                    f.path
                )
            } else {
                anyhow!(
                    "{first} and {} are one file on a case-insensitive or Unicode-normalizing \
                     filesystem; rename the package or narrow the selection",
                    f.path
                )
            });
        }
    }
    // Every directory a file implies is a directory, so nothing else may be
    // a file at that path.
    let directories: BTreeSet<String> = files
        .iter()
        .flat_map(|f| directories_of(&f.path))
        .map(|d| collision_key(&d))
        .collect();
    for f in files {
        if directories.contains(&collision_key(&f.path)) {
            return Err(anyhow!(
                "{} is both a file and a directory in this tree; narrow the selection",
                f.path
            ));
        }
    }
    Ok(())
}

/// Every directory a path implies, outermost first.
fn directories_of(path: &str) -> Vec<String> {
    let segments: Vec<&str> = path.split('/').collect();
    let mut out = Vec::with_capacity(segments.len().saturating_sub(1));
    let mut prefix = String::new();
    for segment in &segments[..segments.len().saturating_sub(1)] {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(segment);
        out.push(prefix.clone());
    }
    out
}

/// The key two paths collide on: case folded, and Unicode-composed, because
/// that is what a case-insensitive or normalizing filesystem compares.
fn collision_key(path: &str) -> String {
    path.nfc().collect::<String>().to_lowercase()
}

/// The package name as an Agent Skills `name`, refusing names that slug to
/// nothing rather than silently substituting the default.
fn package_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_NAME.to_owned());
    }
    if !trimmed.chars().any(|c| c.is_ascii_alphanumeric()) {
        return Err(anyhow!(
            "package name {trimmed:?} needs at least one letter or digit (a-z, 0-9)"
        ));
    }
    Ok(slug(trimmed))
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

/// The optional components: MCP configuration, the catalog guide, and the
/// CLI helper, placed where each shape keeps its resources.
fn extras(profile: &Profile, layout: &Layout, options: &BuildOptions) -> Result<Vec<PluginFile>> {
    let mut out = Vec::new();
    let mut components = options.components.clone();
    components.sort_unstable();
    components.dedup();
    let (docs_dir, tools_dir) = match profile.shape {
        Shape::Skills => (
            layout.content_dir.clone(),
            format!("{}/scripts", layout.root),
        ),
        Shape::AgentPlugin => (layout.content_dir.clone(), layout.scripts_dir.clone()),
        Shape::InstructionFile => ("knowledge".to_owned(), "tools".to_owned()),
        Shape::PromptBundle | Shape::Generic => (
            format!("{}/knowledge", layout.root),
            format!("{}/tools", layout.root),
        ),
    };
    for component in components {
        match component {
            Component::Mcp => {
                if let Some(spec) = profile.mcp {
                    let (name, _) = spec.location(options.remote_endpoint().is_some());
                    // A plugin's mcp.json sits in the plugin directory; every
                    // other harness reads a workspace-rooted path.
                    let path = if profile.shape == Shape::AgentPlugin {
                        format!("{}/{name}", layout.root)
                    } else {
                        name.to_owned()
                    };
                    out.push(file(path, mcp_config(&spec, options)?.into_bytes()));
                }
            }
            Component::Guide => {
                out.push(file(
                    format!("{docs_dir}/USING-THE-CATALOG.md"),
                    guide_md(profile, options).into_bytes(),
                ));
            }
            Component::Tools => {
                let mut f = file(
                    format!("{tools_dir}/okf.sh"),
                    tools_script(options).into_bytes(),
                );
                f.executable = true;
                out.push(f);
            }
        }
    }
    Ok(out)
}

/// The MCP server entry in the harness's own format: a remote endpoint when
/// one was given, otherwise a local stdio server. Neither the connection
/// string nor the bearer token enters the file - the harness expands it
/// from the environment, names the variable to read it from, or the user
/// fills a placeholder in a file outside the repository.
fn mcp_config(spec: &McpSpec, options: &BuildOptions) -> Result<String> {
    match options.remote_endpoint() {
        Some(_) => remote_mcp_config(spec, options),
        None => local_mcp_config(spec, options),
    }
}

/// What a header carries when the harness cannot reference a secret: a word
/// nobody mistakes for a token, in a file the guide says to merge.
const TOKEN_PLACEHOLDER: &str = "TOKEN";

/// The entry for a `pgokf-mcp --http` endpoint, in the harness's own remote
/// form. The token is referenced, named, or left as a placeholder - it is
/// never written.
fn remote_mcp_config(spec: &McpSpec, options: &BuildOptions) -> Result<String> {
    let url = validated_mcp_url(options.mcp_url.as_deref())?
        .ok_or_else(|| anyhow!("no MCP endpoint to configure"))?;
    let remote = spec
        .remote
        .ok_or_else(|| anyhow!("this harness documents no remote MCP server form"))?;
    // The header the harness writes, if it writes one at all.
    let authorization = match remote.token_ref {
        TokenRef::Dollar => Some(format!("Bearer ${{{TOKEN_ENV}}}")),
        TokenRef::DollarEnv => Some(format!("Bearer ${{env:{TOKEN_ENV}}}")),
        TokenRef::Merge => Some(format!("Bearer {TOKEN_PLACEHOLDER}")),
        TokenRef::NamedEnvVar | TokenRef::Forbidden => None,
    };
    let json_entry = || {
        let mut server = serde_json::Map::new();
        if let Some(word) = remote.type_word {
            server.insert("type".to_owned(), json!(word));
        }
        server.insert(remote.url_key.to_owned(), json!(url));
        if let Some(header) = &authorization {
            server.insert("headers".to_owned(), json!({ "Authorization": header }));
        }
        server
    };
    Ok(match spec.format {
        McpFormat::AgentPluginJson => {
            // Agent Plugins 1.0.0 §7.2.1: "Non-loopback endpoints MUST use
            // HTTPS." A package declaring the schema must obey it.
            if url.starts_with("http://") && !is_loopback_endpoint(&url) {
                return Err(anyhow!(
                    "the Agent Plugins specification requires HTTPS for an endpoint that is not \
                     on the loopback interface, and this package declares that specification"
                ));
            }
            let doc = json!({
                "$schema": AGENT_PLUGIN_MCP_SCHEMA,
                "mcpServers": { MCP_SERVER_NAME: Value::Object(json_entry()) }
            });
            serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n"
        }
        McpFormat::McpServersJson => {
            let doc = json!({ "mcpServers": { MCP_SERVER_NAME: Value::Object(json_entry()) } });
            serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n"
        }
        McpFormat::CopilotJson => {
            let mut server = json_entry();
            server.insert("tools".to_owned(), json!(["*"]));
            let doc = json!({ "mcpServers": { MCP_SERVER_NAME: Value::Object(server) } });
            serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n"
        }
        McpFormat::CodexToml => format!(
            "# pgokf catalog MCP server over HTTP (generated by pgokf-workspace).\n\
             # The bearer token is read from {TOKEN_ENV} in your environment,\n\
             # never written here.\n\
             [mcp_servers.{MCP_SERVER_NAME}]\n{} = {}\nbearer_token_env_var = {}\n",
            remote.url_key,
            yaml_string(&url),
            yaml_string(TOKEN_ENV)
        ),
        McpFormat::HermesYaml => {
            let mut yaml = format!(
                "# Merge under the `mcp_servers:` key of ~/.hermes/config.yaml (Hermes reads no\n\
                 # project-level MCP file). The token is a secret and is not written here.\n\
                 mcp_servers:\n  {MCP_SERVER_NAME}:\n    {}: {}\n",
                remote.url_key,
                yaml_string(&url)
            );
            if let Some(header) = &authorization {
                let _ = write!(
                    yaml,
                    "    headers:\n      Authorization: {}\n",
                    yaml_string(header)
                );
            }
            yaml
        }
    })
}

/// The entry for a local stdio server the harness starts itself.
fn local_mcp_config(spec: &McpSpec, options: &BuildOptions) -> Result<String> {
    let command = validated_command(options.mcp_command.as_deref())?;
    let url_ref = match spec.env_ref {
        EnvRef::Dollar => "${OKF_PG_URL}".to_owned(),
        EnvRef::DollarEnv => "${env:OKF_PG_URL}".to_owned(),
        EnvRef::Forward | EnvRef::Placeholder | EnvRef::PluginData | EnvRef::Inherit => {
            "postgresql://okf_reader:PASSWORD@HOST:5432/okf".to_owned()
        }
    };
    let tenant = options.tenant.as_deref().filter(|t| !t.trim().is_empty());
    Ok(match spec.format {
        McpFormat::AgentPluginJson => {
            // Agent Plugins expand only ${PLUGIN_ROOT} and ${PLUGIN_DATA}, and
            // a conformant plugin depends on no ambient variable and embeds no
            // secret: the server reads its connection string from an env file
            // the user creates once under the client-managed data directory.
            let command = validated_plugin_command(&command)?;
            let mut server = serde_json::Map::new();
            server.insert("type".to_owned(), json!("stdio"));
            server.insert("command".to_owned(), json!(command));
            server.insert(
                "args".to_owned(),
                json!(["--env-file", format!("${{PLUGIN_DATA}}/{PLUGIN_ENV_FILE}")]),
            );
            if let Some(t) = tenant {
                server.insert("env".to_owned(), json!({ "OKF_TENANT": t }));
            }
            let doc = json!({
                "$schema": AGENT_PLUGIN_MCP_SCHEMA,
                "mcpServers": { MCP_SERVER_NAME: server }
            });
            serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n"
        }
        McpFormat::McpServersJson => {
            let mut env = serde_json::Map::new();
            env.insert("OKF_PG_URL".to_owned(), json!(url_ref));
            if let Some(t) = tenant {
                env.insert("OKF_TENANT".to_owned(), json!(t));
            }
            let doc = json!({
                "mcpServers": {
                    MCP_SERVER_NAME: { "command": command, "args": [], "env": env }
                }
            });
            serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n"
        }
        McpFormat::CopilotJson => {
            // Copilot documents no expansion form for env values, and a
            // stdio server inherits the environment Copilot starts with, so
            // no OKF_PG_URL entry is written at all.
            let mut server = serde_json::Map::new();
            server.insert("type".to_owned(), json!("local"));
            server.insert("command".to_owned(), json!(command));
            server.insert("args".to_owned(), json!([]));
            if let Some(t) = tenant {
                server.insert("env".to_owned(), json!({ "OKF_TENANT": t }));
            }
            server.insert("tools".to_owned(), json!(["*"]));
            let doc = json!({ "mcpServers": { MCP_SERVER_NAME: server } });
            serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n"
        }
        McpFormat::CodexToml => {
            let mut toml = format!(
                "# pgokf catalog MCP server (generated by pgokf-workspace).\n\
                 # OKF_PG_URL is forwarded from your environment, never written here.\n\
                 [mcp_servers.{MCP_SERVER_NAME}]\ncommand = {}\nargs = []\nenv_vars = [\"OKF_PG_URL\"]\n",
                yaml_string(&command)
            );
            if let Some(t) = tenant {
                let _ = writeln!(toml, "env = {{ OKF_TENANT = {} }}", yaml_string(t));
            }
            toml
        }
        McpFormat::HermesYaml => {
            let mut yaml = format!(
                "# Merge under the `mcp_servers:` key of ~/.hermes/config.yaml (Hermes reads no\n\
                 # project-level MCP file). Replace the placeholder connection string there;\n\
                 # Hermes passes only the env values you list to the server.\n\
                 mcp_servers:\n  {MCP_SERVER_NAME}:\n    command: {}\n    args: []\n    env:\n      OKF_PG_URL: {}\n",
                yaml_string(&command),
                yaml_string(&url_ref)
            );
            if let Some(t) = tenant {
                let _ = writeln!(yaml, "      OKF_TENANT: {}", yaml_string(t));
            }
            yaml
        }
    })
}

/// The env file a plugin's MCP server reads from `${PLUGIN_DATA}`.
pub const PLUGIN_ENV_FILE: &str = "pgokf.env";

/// The Agent Plugins manifest (`plugin.json`, Agent Plugins 1.0.0): the
/// closed set of portable fields, nothing else.
///
/// `version` is `1.<yyyymmdd>.<seconds of day>+<digest>`: the ordered part
/// comes from the newest sync among the included bundles, so a rebuilt
/// plugin compares newer under `SemVer` once the catalog changed, and the
/// build metadata is a digest over every other file of the plugin and the
/// manifest's own portable fields, so it changes exactly when anything the
/// plugin ships does.
fn plugin_json(
    name: &str,
    options: &BuildOptions,
    selection: &Selection,
    records: &[ConceptRecord],
    snapshot: &Snapshot,
    files: &[PluginFile],
) -> String {
    let mut keywords: Vec<&str> = distinct(
        records
            .iter()
            .flat_map(|r| r.tags.iter().map(String::as_str))
            .chain(records.iter().filter_map(|r| r.concept_type.as_deref())),
        20,
    );
    keywords.sort_unstable();
    let mut doc = serde_json::Map::new();
    doc.insert("$schema".to_owned(), json!(AGENT_PLUGIN_SCHEMA));
    doc.insert("name".to_owned(), json!(name));
    doc.insert(
        "description".to_owned(),
        json!(description(options, selection, records)),
    );
    if let Some(url) = options.web_url.as_deref().filter(|u| !u.trim().is_empty()) {
        doc.insert("homepage".to_owned(), json!(url.trim()));
    }
    if !keywords.is_empty() {
        doc.insert("keywords".to_owned(), json!(keywords));
    }
    let mut digested = files.iter().fold(String::new(), |mut acc, f| {
        let _ = writeln!(acc, "{}\n{}", f.path, f.sha256);
        acc
    });
    digested.push_str(&serde_json::Value::Object(doc.clone()).to_string());
    let digest = sha256_hex(digested.as_bytes());
    doc.insert(
        "version".to_owned(),
        json!(format!("{}+{}", ordered_version(snapshot), &digest[..12])),
    );
    serde_json::to_string_pretty(&serde_json::Value::Object(doc)).unwrap_or_default() + "\n"
}

/// `1.<yyyymmdd>.<seconds of day>` from the newest `last_synced_at` of the
/// snapshot's bundles (an RFC 3339 instant), or `1.0.0` when the snapshot
/// carries none (a preview).
fn ordered_version(snapshot: &Snapshot) -> String {
    let newest = snapshot
        .bundles
        .iter()
        .filter_map(|b| b.last_synced_at.as_deref())
        .max();
    let Some(stamp) = newest else {
        return "1.0.0".to_owned();
    };
    // 2026-09-07T00:05:12Z (or with an offset / fraction): digits only.
    let digits: Vec<u32> = stamp
        .chars()
        .take(19)
        .filter_map(|c| c.to_digit(10))
        .collect();
    if digits.len() < 14 {
        return "1.0.0".to_owned();
    }
    let number = |range: std::ops::Range<usize>| -> u32 {
        digits[range].iter().fold(0, |acc, d| acc * 10 + d)
    };
    let date = number(0..8);
    let seconds = number(8..10) * 3600 + number(10..12) * 60 + number(12..14);
    format!("1.{date}.{seconds}")
}

/// An Agent Plugins `command`: one executable token that is either a bare
/// name (resolved on the platform's search path) or a plugin-relative path
/// beginning with `./` that stays inside the plugin (spec §7.2.1, §4.1);
/// anything else makes the server entry invalid for a conformant client.
fn validated_plugin_command(command: &str) -> Result<String> {
    let bare = !command.contains(['/', '\\', ':']);
    let relative = command.starts_with("./")
        && !command.contains('\\')
        && !command.contains(':')
        && !command.split('/').any(|seg| seg == "..");
    if bare || relative {
        Ok(command.to_owned())
    } else {
        Err(anyhow!(
            "MCP command {command:?} is not valid for an Agent Plugin: use a bare program name \
             (resolved on PATH) or a plugin-relative path beginning with ./"
        ))
    }
}

/// A command name or path: no whitespace or shell metacharacters, so it
/// can only name a program.
fn validated_command(raw: Option<&str>) -> Result<String> {
    let command = raw
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .unwrap_or("pgokf-mcp");
    if command
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-' | '\\' | ':'))
    {
        Ok(command.to_owned())
    } else {
        Err(anyhow!(
            "MCP command {command:?} is not a program name or path"
        ))
    }
}

/// Characters a URL never needs and a shell reads as syntax. The web URL is
/// written into `okf.sh`, which ships executable and which the generated
/// guide tells the agent to run, so it is held to the same standard as a
/// command: quoting is the fix, this is the second lock.
const SHELL_METACHARACTERS: [char; 14] = [
    '\'', '"', '`', '$', '\\', '(', ')', '{', '}', ';', '&', '|', '<', '>',
];

/// A base URL for the JSON API: http(s), no whitespace, no trailing slash,
/// and nothing a shell would read as syntax.
fn validated_web_url(raw: Option<&str>) -> Result<Option<String>> {
    let Some(url) = raw.map(str::trim).filter(|u| !u.is_empty()) else {
        return Ok(None);
    };
    if (url.starts_with("http://") || url.starts_with("https://"))
        && url
            .chars()
            .all(|c| c.is_ascii_graphic() && !SHELL_METACHARACTERS.contains(&c))
    {
        Ok(Some(url.trim_end_matches('/').to_owned()))
    } else {
        Err(anyhow!(
            "web URL {url:?} is not an http(s) URL, or holds a character a shell would read as \
             syntax"
        ))
    }
}

/// The longest an endpoint may be. Every other free-text option is bounded;
/// this one is written into three files and a query string.
const MCP_URL_MAX: usize = 512;

/// The endpoint of a `pgokf-mcp --http` server: http(s), carrying no secret
/// of any kind, and nothing that could break out of the file it is written
/// into.
///
/// A `pgokf-mcp --http` endpoint is a path (`POST /mcp`) and nothing more,
/// so a query string or a fragment is refused outright rather than
/// inspected: both are places a token gets put by mistake, and both would
/// then be written into files meant to be committed. The Agent Plugins
/// specification independently forbids user information and a fragment in
/// a server URL.
///
/// # Errors
///
/// The URL is not http(s), is too long, names no host, carries credentials,
/// a query or a fragment, or holds a character that does not belong in one.
fn validated_mcp_url(raw: Option<&str>) -> Result<Option<String>> {
    let Some(url) = raw.map(str::trim).filter(|u| !u.is_empty()) else {
        return Ok(None);
    };
    if url.len() > MCP_URL_MAX {
        return Err(anyhow!(
            "MCP endpoint is {} characters; the most is {MCP_URL_MAX}",
            url.len()
        ));
    }
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return Err(anyhow!("MCP endpoint {url:?} is not an http(s) URL"));
    };
    if !url
        .chars()
        .all(|c| c.is_ascii_graphic() && c != '\'' && c != '"' && c != '`')
    {
        return Err(anyhow!("MCP endpoint {url:?} is not a URL"));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if authority.is_empty() {
        return Err(anyhow!("MCP endpoint {url:?} names no host"));
    }
    if authority.contains('@') {
        return Err(anyhow!(
            "MCP endpoint {url:?} carries credentials; the token goes in the Authorization \
             header, which this build references rather than writes"
        ));
    }
    if rest.contains(['?', '#']) {
        return Err(anyhow!(
            "MCP endpoint {url:?} carries a query or a fragment; the endpoint is a path, and a \
             token put in one would be written into files meant to be committed"
        ));
    }
    Ok(Some(url.to_owned()))
}

/// Whether an endpoint's host is one only this machine can reach, which is
/// where the Agent Plugins specification allows plain HTTP.
fn is_loopback_endpoint(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = match authority.rsplit_once(':') {
        // A port only when digits follow and what precedes them is either a
        // plain host or a bracketed IPv6 literal - the colons inside `::1`
        // are not port separators.
        Some((head, port))
            if !port.is_empty()
                && port.chars().all(|c| c.is_ascii_digit())
                && (head.ends_with(']') || !head.contains(':')) =>
        {
            head
        }
        _ => authority,
    };
    host.eq_ignore_ascii_case("localhost")
        || host == "[::1]"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|address| address.is_loopback())
}

/// The guide the agent reads to use the catalog beyond the packaged files.
fn guide_md(profile: &Profile, options: &BuildOptions) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Using the catalog ({})", options.catalog_name);
    out.push_str(
        "\nThis package holds a selection of concepts from a pgokf catalog: a PostgreSQL-backed \
         catalog of Open Knowledge Format (OKF) documents. Each concept has a stable identity \
         `(bundle_id, concept_id)`, a type, tags, a lifecycle status, and a derived trust tier \
         (`human-reviewed` > `machine-confirmed` > `unverified`). Prefer higher tiers when \
         documents disagree, and cite the identity when you rely on one.\n",
    );
    if options.components.contains(&Component::Mcp) {
        out.push_str("\n## Live access through MCP\n\n");
        match (profile.mcp, options.remote_endpoint()) {
            (Some(spec), Some(url)) => out.push_str(&remote_mcp_guide(&spec, url)),
            // A target with no MCP configuration has none whether or not an
            // endpoint was named; the local branch says so.
            (mcp, _) => guide_local_mcp(&mut out, mcp),
        }
        out.push_str(
            "\nTools: `concept_search` (full-text query with `type`, `tags`, `status`, `trust_tier` filters), \
             `find_similar` (more like a concept), `concept_neighbors` (walk resolved links), \
             `get_concept` (a concept's fields and text), `get_skill` (a stored Agent Skills \
             package), `list_plugin_targets` and `build_workspace_plugin` (rebuild a package \
             like this one). Example:\n\n\
             ```json\n{\"name\": \"concept_search\", \"arguments\": {\"query\": \"failover\", \"limit\": 5}}\n```\n",
        );
        if options.remote_endpoint().is_some() && profile.mcp.is_some() {
            out.push_str(
                "\nOver HTTP the token's role decides which of those you see: a `reader` token is \
                 offered the five reading tools, a `builder` token those and the two plugin tools.\n",
            );
        }
    }
    out.push_str("\n## The JSON API\n\n");
    let base = options
        .web_url
        .as_deref()
        .map_or("http://<pgokf-web host>:8080", |u| u.trim_end_matches('/'));
    let _ = writeln!(
        out,
        "The pgokf web UI serves the same catalog as JSON under `{base}/api/`:\n\n\
         - `GET {base}/api/search?q=<query>&limit=20` (add `type=`, `tags=`, `status=`, `trust=`, `bundle=`; with filters and no `q` it browses)\n\
         - `GET {base}/api/concepts/<bundle_id>/<concept_id>`\n\
         - `GET {base}/api/graph/<bundle_id>/<concept_id>?hops=2` and `GET {base}/api/graph?bundle=<id>&limit=300`\n\
         - `GET {base}/api/bundles`, `GET {base}/api/health`"
    );
    if options.components.contains(&Component::Tools) {
        out.push_str(
            "\n`okf.sh` in this package wraps those endpoints (`okf.sh search <query>`, `okf.sh get <bundle_id> <concept_id>`, `okf.sh graph <bundle_id> <concept_id> [hops]`, `okf.sh bundles`); it needs `curl` and reads `OKF_WEB_URL`.\n",
        );
    }
    let _ = write!(
        out,
        "\n## Rebuilding this package\n\n`okf-workspace.yaml` {} records the selection and `okf-workspace.lock` the catalog snapshot with a hash per file; rebuild from the web UI's Plugins page or with the `build_workspace_plugin` MCP tool.\n",
        if profile.shape == Shape::AgentPlugin {
            "in the plugin directory"
        } else {
            "at the workspace root"
        }
    );
    out
}

/// What the guide says about an endpoint this package points at: where the
/// entry is, and how the token reaches it without being in the tree.
fn remote_mcp_guide(spec: &McpSpec, url: &str) -> String {
    let (path, auto_loaded) = spec.location(true);
    let Some(remote) = spec.remote else {
        return String::new();
    };
    let mut out = String::new();
    let _ = write!(
        out,
        "`{path}` points the `{MCP_SERVER_NAME}` MCP server at `{url}`, a `pgokf-mcp --http` \
         endpoint. {}",
        if auto_loaded {
            "The harness loads it from the workspace.".to_owned()
        } else {
            merge_sentence(spec.merge_target(true))
        }
    );
    let _ = writeln!(
        out,
        " {}",
        match remote.token_ref {
            TokenRef::Dollar | TokenRef::DollarEnv => format!(
                "Every request carries a bearer token: set `{TOKEN_ENV}` in the environment that \
                 starts the harness, which expands it into the header. The token itself is not \
                 written into this tree."
            ),
            TokenRef::NamedEnvVar => format!(
                "Every request carries a bearer token: the entry names `{TOKEN_ENV}` for the \
                 harness to read it from, so set that in the environment that starts the \
                 harness. The token itself is not written into this tree."
            ),
            TokenRef::Merge => format!(
                "Every request carries a bearer token, and this harness documents no way to \
                 reference one from a configuration file - so the entry holds the placeholder \
                 `{TOKEN_PLACEHOLDER}` and lives here as a fragment rather than in a file of \
                 the workspace. Put your token in where you merge it, and keep that file out of \
                 version control."
            ),
            TokenRef::Forbidden =>
                "Every request carries a bearer token, and the Agent Plugins specification \
                 forbids a credential in a package and defines no way to reference one - so the \
                 entry names the endpoint only. Give your client the token itself."
                    .to_owned(),
        }
    );
    let _ = writeln!(
        out,
        "Mint one on the catalog's Admin page (pgokf-web), or with `pgokf-web mcp-token mint \
         --name <who> --role reader` on the server. Source for this layout: {}",
        remote.source
    );
    out
}

/// Where a fragment belongs, named when the harness's own file is known.
fn merge_sentence(target: Option<&str>) -> String {
    target.map_or_else(
        || "Merge it into the harness's own configuration.".to_owned(),
        |path| format!("Merge it into the harness's own configuration, `{path}`."),
    )
}

/// What the guide says about a server the harness starts itself.
fn guide_local_mcp(out: &mut String, mcp: Option<McpSpec>) {
    match mcp {
        Some(spec) if spec.auto_loaded && spec.env_ref == EnvRef::Inherit => {
            let _ = writeln!(
                out,
                "`{}` configures the `{MCP_SERVER_NAME}` MCP server (the `pgokf-mcp` companion); the harness loads it from the workspace and starts the server with its own environment, so set `OKF_PG_URL` to a reader connection string in the environment that starts the harness (no value is written into the file).",
                spec.path
            );
        }
        Some(spec) if spec.auto_loaded => {
            let _ = writeln!(
                out,
                "`{}` configures the `{MCP_SERVER_NAME}` MCP server (the `pgokf-mcp` companion); the harness loads it from the workspace. Set `OKF_PG_URL` to a reader connection string in the environment that starts the harness.",
                spec.path
            );
        }
        Some(spec) => {
            let _ = writeln!(
                out,
                "`{}` holds the `{MCP_SERVER_NAME}` MCP server entry. {} Set `OKF_PG_URL` (a reader connection string) where that file expects it.",
                spec.path,
                merge_sentence(spec.merge_target(false))
            );
        }
        None => {
            out.push_str("This target has no MCP configuration; use the JSON API below.\n");
        }
    }
}

/// A POSIX shell helper over the JSON API.
fn tools_script(options: &BuildOptions) -> String {
    let default_url = options
        .web_url
        .as_deref()
        .map_or("http://localhost:8080", |u| u.trim_end_matches('/'));
    OKF_SH.replace("__DEFAULT_URL__", default_url)
}

/// The helper script's text; `__DEFAULT_URL__` is the web UI base URL.
const OKF_SH: &str = r#"#!/bin/sh
# okf.sh - query the pgokf catalog through its JSON API (generated by pgokf-workspace).
# Usage: okf.sh search <query> [limit] | get <bundle_id> <concept_id> | graph <bundle_id> <concept_id> [hops] | bundles | health
# Needs curl; set OKF_WEB_URL to the pgokf-web base URL (default: __DEFAULT_URL__).
set -eu
# Single-quoted: the shell expands nothing inside, so the URL below is data
# whatever it contains.
OKF_DEFAULT_URL='__DEFAULT_URL__'
BASE="${OKF_WEB_URL:-$OKF_DEFAULT_URL}"
enc() { printf '%s' "$1" | od -An -tx1 -v | tr -d ' \n' | sed 's/\([0-9a-f][0-9a-f]\)/%\1/g'; }
case "${1:-}" in
  search) [ $# -ge 2 ] || { echo 'usage: okf.sh search <query> [limit]' >&2; exit 2; }
    curl -fsS "$BASE/api/search?q=$(enc "$2")&limit=${3:-20}" ;;
  get) [ $# -ge 3 ] || { echo 'usage: okf.sh get <bundle_id> <concept_id>' >&2; exit 2; }
    curl -fsS "$BASE/api/concepts/$2/$3" ;;
  graph) [ $# -ge 3 ] || { echo 'usage: okf.sh graph <bundle_id> <concept_id> [hops]' >&2; exit 2; }
    curl -fsS "$BASE/api/graph/$2/$3?hops=${4:-2}" ;;
  bundles) curl -fsS "$BASE/api/bundles" ;;
  health) curl -fsS "$BASE/api/health" ;;
  *) echo 'usage: okf.sh search|get|graph|bundles|health ...' >&2; exit 2 ;;
esac
echo
"#;

/// The two files that make a tree reproducible: the manifest (the
/// selection) and the lockfile (the catalog snapshot and a hash per file).
#[allow(clippy::too_many_arguments)]
fn meta_files(
    layout: &Layout,
    name: &str,
    options: &BuildOptions,
    selection: &Selection,
    snapshot: &Snapshot,
    entries: &[serde_json::Value],
    // Whether an MCP entry was written at all; without one the tree calls
    // no endpoint, whatever was asked for.
    wrote_an_entry: bool,
    // Whether the freshness/closure machinery had a say in this build.
    engaged: bool,
) -> Vec<PluginFile> {
    let _ = (snapshot, entries);
    vec![file(
        layout.meta_path(MANIFEST_FILE),
        manifest_yaml(
            name,
            options.target,
            options.harness.as_ref(),
            selection,
            &options.components,
            // Only an endpoint an entry was actually written for reaches
            // the catalog block; otherwise the tree calls nothing.
            options.remote_endpoint().filter(|_| wrote_an_entry),
            engaged,
        )
        .into_bytes(),
    )]
}

#[allow(clippy::too_many_arguments)]
fn lockfile(
    name: &str,
    target: Target,
    harness: Option<&CustomHarness>,
    root: &str,
    selection: &Selection,
    snapshot: &Snapshot,
    entries: &[serde_json::Value],
    files: &[PluginFile],
    report: &BuildReport,
    engaged: bool,
) -> String {
    let mut lock = json!({
        "version": 1,
        "name": name,
        "target": target.id(),
        "root": root,
        "catalog": {
            "version": snapshot.version,
            "sql_version": snapshot.sql_version,
            "bundles": snapshot.bundles,
        },
        // Every file of the tree, so what was received can be checked
        // against what was built. `entries` below is the catalog identity
        // of the content files; this is the tree itself, generated files
        // included. The lockfile cannot hash itself and is not listed.
        "files": files
            .iter()
            .map(|f| json!({
                "path": f.path,
                "sha256": f.sha256,
                "bytes": f.bytes.len(),
                "executable": f.executable,
            }))
            .collect::<Vec<_>>(),
        "entries": entries,
    });
    if let Some(harness) = harness.filter(|_| target == Target::Custom) {
        lock["harness"] = serde_json::to_value(harness).unwrap_or(serde_json::Value::Null);
    }
    // The policy blocks appear only when the freshness/closure machinery had
    // a say, so a build it never touched stays byte-identical.
    if engaged {
        lock["policy"] = json!({
            "stale": selection.stale_policy.id(),
            "freshness_available": report.freshness_available,
        });
        if let Some(closure) = &report.closure {
            lock["closure"] = serde_json::to_value(closure).unwrap_or(serde_json::Value::Null);
        }
        if !report.excluded.is_empty() {
            lock["excluded"] = serde_json::to_value(&report.excluded).unwrap_or_default();
        }
    }
    serde_json::to_string_pretty(&lock).unwrap_or_default() + "\n"
}

fn file(path: String, bytes: Vec<u8>) -> PluginFile {
    let sha256 = sha256_hex(&bytes);
    PluginFile {
        path,
        bytes,
        sha256,
        executable: false,
    }
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
fn listing(listed: &[(String, &ConceptRecord)]) -> String {
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
            listing_line(&mut out, relative, r);
        }
    }
    out
}

/// The skill packages of the tree, one line each, linking to the copied
/// `SKILL.md`.
fn package_listing(packages: &[(String, &ConceptRecord)], prefix: &str) -> String {
    let mut out = String::new();
    for (dir, r) in packages {
        let Some(package) = &r.package else {
            continue;
        };
        let scripts = package.resources.iter().filter(|x| x.is_script()).count();
        let others = package.resources.len() - scripts;
        let _ = write!(
            out,
            "- [{}]({}) \u{2014} {}",
            package.name,
            link_destination(&format!("{prefix}{dir}/SKILL.md")),
            r.trust_tier
        );
        if *dir != package.name {
            let _ = write!(
                out,
                " (installed as `{dir}`; the harness may warn that the directory differs from \
                 the skill's name)"
            );
        }
        if let Some(d) = r.description.as_deref().filter(|d| !d.is_empty()) {
            let _ = write!(out, ". {d}");
        }
        let _ = writeln!(
            out,
            " ({scripts} script{}, {others} reference{}) `({}:{})`",
            if scripts == 1 { "" } else { "s" },
            if others == 1 { "" } else { "s" },
            r.bundle_id,
            r.concept_id
        );
    }
    out
}

fn listing_line(out: &mut String, link: &str, r: &ConceptRecord) {
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
        link_destination(link)
    );
    let _ = write!(out, " \u{2014} {}", meta.join(" \u{00b7} "));
    if let Some(d) = r.description.as_deref().filter(|d| !d.is_empty()) {
        let _ = write!(out, ". {d}");
    }
    let _ = writeln!(out, " `({}:{})`", r.bundle_id, r.concept_id);
}

/// The one sentence every index renders when the build wrote a freshness
/// report: how many concepts it names and where the report is.
fn freshness_note(link: Option<&str>, stale_count: usize) -> String {
    match link {
        Some(link) => format!(
            "\nFreshness: the catalog reported {stale_count} concept{} as not fresh (or excluded) \
             when this workspace was built; see [{FRESHNESS_FILE}]({}) for states, reasons, and \
             revisions.\n",
            if stale_count == 1 { "" } else { "s" },
            link_destination(link),
        ),
        None => String::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn skill_md(
    name: &str,
    title: &str,
    options: &BuildOptions,
    selection: &Selection,
    records: &[ConceptRecord],
    listed: &[(String, &ConceptRecord)],
    packages: &[(String, &ConceptRecord)],
    package_prefix: &str,
    freshness_link: Option<&str>,
    stale_count: usize,
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
    if !packages.is_empty() {
        let _ = write!(
            out,
            "## Skills\n\nThese Agent Skills packages from the catalog are installed beside this one, \
             byte for byte (their `SKILL.md`, `scripts/`, `references/`, and `assets/`); the harness \
             loads them like any other skill.\n\n"
        );
        out.push_str(&package_listing(packages, package_prefix));
        out.push('\n');
    }
    out.push_str("## Contents\n");
    if listed.is_empty() {
        out.push_str("(no documents besides the skills above)\n");
    }
    out.push_str(&listing(listed));
    if options.components.contains(&Component::Guide) {
        out.push_str("\nRead [references/USING-THE-CATALOG.md](references/USING-THE-CATALOG.md) for how to query the live catalog (MCP tools, JSON API) beyond these files.\n");
    }
    if options.components.contains(&Component::Tools) {
        out.push_str("\n`scripts/okf.sh` queries the catalog's JSON API (`scripts/okf.sh search <query>`).\n");
    }
    out.push_str("\n## Provenance\n\n");
    let _ = writeln!(
        out,
        "Built by pgokf-workspace from the catalog. `{LOCK_FILE}` {} records the \
         catalog snapshot and a content hash per file; `{MANIFEST_FILE}` reproduces the selection. \
         Files marked reconstructed in the lockfile were rebuilt from indexed text because the bundle \
         was ingested without stored source.",
        if options
            .profile()
            .is_ok_and(|p| p.shape == Shape::AgentPlugin)
        {
            "in the plugin directory"
        } else {
            "at the workspace root"
        }
    );
    out.push_str(&freshness_note(freshness_link, stale_count));
    out
}

#[allow(clippy::too_many_arguments)]
fn agents_md(
    title: &str,
    options: &BuildOptions,
    selection: &Selection,
    records: &[ConceptRecord],
    listed: &[(String, &ConceptRecord)],
    packages: &[(String, &ConceptRecord)],
    package_prefix: &str,
    freshness_link: Option<&str>,
    stale_count: usize,
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
    if !packages.is_empty() {
        out.push_str("### Skills\n\nAgent Skills packages copied whole from the catalog; read a package's `SKILL.md` before using its scripts.\n\n");
        out.push_str(&package_listing(packages, package_prefix));
        out.push('\n');
    }
    out.push_str("### Index\n");
    out.push_str(&listing(listed));
    out.push_str(&freshness_note(freshness_link, stale_count));
    out
}

#[allow(clippy::too_many_arguments)]
fn index_md(
    title: &str,
    options: &BuildOptions,
    selection: &Selection,
    records: &[ConceptRecord],
    listed: &[(String, &ConceptRecord)],
    packages: &[(String, &ConceptRecord)],
    package_prefix: &str,
    freshness_link: Option<&str>,
    stale_count: usize,
) -> String {
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
    if !packages.is_empty() {
        out.push_str("## Skills\n\nAgent Skills packages copied whole from the catalog (`SKILL.md`, `scripts/`, `references/`, `assets/`).\n\n");
        out.push_str(&package_listing(packages, package_prefix));
        out.push('\n');
    }
    out.push_str("## Index\n");
    out.push_str(&listing(listed));
    out.push_str(&freshness_note(freshness_link, stale_count));
    out
}

/// The bounded system prompt of a prompt bundle: the index, then as many
/// documents as fit the budget, most trusted first. A document the catalog
/// reported stale is inlined with its freshness warning - the same banner a
/// reconstructed document carries - and the warning counts against the
/// budget like the document itself; the exported copy of the document keeps
/// its exact bytes either way.
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
        // A binary asset has no prose to inline; it stays under knowledge/.
        let Ok(text) = std::str::from_utf8(&r.bytes) else {
            continue;
        };
        let body = strip_frontmatter(text);
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
        let notice = r.freshness.is_stale().then(|| stale_banner(r));
        let notice_len = notice.as_deref().map_or(0, str::len);
        if used + header.len() + notice_len + body.len() > PROMPT_BUDGET {
            continue;
        }
        used += header.len() + notice_len + body.len();
        included += 1;
        out.push_str(&header);
        if let Some(notice) = &notice {
            out.push_str(notice);
        }
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
#[allow(clippy::too_many_lines)]
fn manifest_yaml(
    name: &str,
    target: Target,
    harness: Option<&CustomHarness>,
    selection: &Selection,
    components: &[Component],
    mcp_url: Option<&str>,
    engaged: bool,
) -> String {
    let mut out = String::new();
    out.push_str(
        "# okf-workspace.yaml - generated by pgokf. Rebuild the same tree from the web Plugins page\n\
         # or the MCP build_workspace_plugin tool; a `pgokf-workspace sync` command is planned for it.\n",
    );
    out.push_str("version: 1\n");
    let _ = writeln!(out, "name: {}", yaml_string(name));
    let _ = writeln!(out, "targets: [{}]", target.id());
    if let Some(harness) = harness.filter(|_| target == Target::Custom) {
        let mut fields = vec![
            format!("label: {}", yaml_string(&harness.label)),
            format!("kind: {}", harness.shape.id()),
        ];
        if let Some(dir) = &harness.skills_dir {
            fields.push(format!("skills_dir: {}", yaml_string(dir)));
        }
        let _ = writeln!(out, "harness: {{ {} }}", fields.join(", "));
    }
    let mut ids: Vec<&str> = components.iter().map(|c| c.id()).collect();
    ids.sort_unstable();
    ids.dedup();
    let _ = writeln!(out, "components: [{}]", ids.join(", "));
    // How the built tree reaches the catalog: a server it starts, which
    // reads the connection string from the environment, or an endpoint it
    // calls, whose token it reads from the environment. Neither value is
    // ever written here.
    match mcp_url {
        Some(url) => {
            let _ = writeln!(
                out,
                "catalog:\n  mcp_url: {}\n  token_env: {TOKEN_ENV}",
                yaml_string(url)
            );
        }
        None => out.push_str("catalog:\n  url_env: OKF_PG_URL\n"),
    }
    let _ = write!(
        out,
        "policy:\n  trust: {}\n",
        if selection.verified_only {
            "verified-only"
        } else {
            "any"
        }
    );
    if engaged {
        let _ = writeln!(out, "  stale: {}", selection.stale_policy.id());
    }
    out.push_str("include:\n  - ");
    let mut fields: Vec<String> = Vec::new();
    if selection.all {
        fields.push("all: true".to_owned());
    }
    if !selection.picks.is_empty() {
        let picks: Vec<String> = selection
            .picks
            .iter()
            .map(|p| yaml_string(&p.to_string()))
            .collect();
        fields.push(format!("picks: [{}]", picks.join(", ")));
    }
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
    if engaged && !selection.seeds.is_empty() {
        let seeds: Vec<String> = selection
            .seeds
            .iter()
            .map(|s| yaml_string(&s.to_string()))
            .collect();
        fields.push(format!("seeds: [{}]", seeds.join(", ")));
        if !selection.relation_types.is_empty() {
            fields.push(format!(
                "relation_types: [{}]",
                list(&selection.relation_types)
            ));
        }
        if selection.direction != crate::selection::Direction::Outbound {
            fields.push(format!("direction: {}", selection.direction.id()));
        }
        fields.push(format!("hops: {}", selection.effective_hops()));
        if selection.require_closure {
            fields.push("require_closure: true".to_owned());
        }
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
            package: None,
            resource: None,
            freshness: crate::selection::Freshness::unknown(),
            origin: crate::selection::Origin::default(),
        }
    }

    /// A skill package record with one script and one reference.
    fn package_record(bundle_id: i64, name: &str) -> ConceptRecord {
        use crate::selection::{PackageRecord, ResourceFile};
        let mut r = record(
            bundle_id,
            &format!("skills/{name}/SKILL"),
            name,
            "human-reviewed",
            "Use it.",
        );
        r.path = format!("skills/{name}/SKILL.md");
        r.concept_type = Some("Skill".to_owned());
        r.bytes = format!("---\nname: {name}\ndescription: d\n---\n# Use\n").into_bytes();
        r.package = Some(PackageRecord {
            name: name.to_owned(),
            root: format!("skills/{name}"),
            hash: "p".repeat(64),
            member_freshness: std::collections::BTreeMap::new(),
            resources: vec![
                ResourceFile {
                    concept_id: format!("skills/{name}/scripts/run.sh"),
                    class: "script".to_owned(),
                    path: "scripts/run.sh".to_owned(),
                    sha256: "s".repeat(64),
                    file_hash: "fs".to_owned(),
                    bytes: b"#!/bin/sh\necho run\n".to_vec(),
                },
                ResourceFile {
                    concept_id: format!("skills/{name}/assets/logo.png"),
                    class: "asset".to_owned(),
                    path: "assets/logo.png".to_owned(),
                    sha256: "a".repeat(64),
                    file_hash: "fa".to_owned(),
                    bytes: b"\x89PNG\r\n\x1a\n".to_vec(),
                },
            ],
        });
        r
    }

    fn options(target: Target) -> BuildOptions {
        BuildOptions {
            target,
            harness: None,
            name: "Ops Runbooks!".to_owned(),
            title: None,
            catalog_name: "acme".to_owned(),
            base_model: None,
            components: Vec::new(),
            mcp_command: None,
            mcp_url: None,
            tenant: None,
            web_url: None,
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
                catalog_generation: None,
                freshness_state: None,
                freshness_reasons: Vec::new(),
                embedding_contract: None,
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
        assert_eq!(
            package_name("OKF Knowledge").expect("slugs"),
            "okf-knowledge"
        );
        assert!(package_name("---").is_err());
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
    fn components_add_mcp_config_guide_and_helper_in_the_documented_places() {
        // Arrange
        let records = vec![record(1, "a", "Alpha", "human-reviewed", "Do A.")];
        let with = |target: Target| BuildOptions {
            components: vec![Component::Mcp, Component::Guide, Component::Tools],
            tenant: Some("acme".to_owned()),
            web_url: Some("https://okf.example.test/".to_owned()),
            ..options(target)
        };

        // Act
        let claude = assemble(
            &with(Target::ClaudeCode),
            &selection(),
            &snapshot(),
            &records,
        )
        .expect("claude");
        let codex =
            assemble(&with(Target::Codex), &selection(), &snapshot(), &records).expect("codex");
        let hermes = assemble(
            &with(Target::HermesAgent),
            &selection(),
            &snapshot(),
            &records,
        )
        .expect("hermes");
        let cursor =
            assemble(&with(Target::Cursor), &selection(), &snapshot(), &records).expect("cursor");
        let ollama =
            assemble(&with(Target::Ollama), &selection(), &snapshot(), &records).expect("ollama");

        // Assert
        let paths = |p: &Plugin| p.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
        let text = |p: &Plugin, path: &str| {
            String::from_utf8_lossy(&p.files.iter().find(|f| f.path == path).expect(path).bytes)
                .into_owned()
        };
        assert!(paths(&claude).contains(&".mcp.json".to_owned()));
        assert!(
            paths(&claude).contains(
                &".claude/skills/ops-runbooks/references/USING-THE-CATALOG.md".to_owned()
            )
        );
        let helper = claude
            .files
            .iter()
            .find(|f| f.path == ".claude/skills/ops-runbooks/scripts/okf.sh")
            .expect("helper");
        assert!(helper.executable);
        assert!(
            String::from_utf8_lossy(&helper.bytes)
                .contains("OKF_DEFAULT_URL='https://okf.example.test'")
        );
        let doc: serde_json::Value =
            serde_json::from_str(&text(&claude, ".mcp.json")).expect("json");
        assert_eq!(doc["mcpServers"]["pgokf"]["command"], "pgokf-mcp");
        assert_eq!(
            doc["mcpServers"]["pgokf"]["env"]["OKF_PG_URL"],
            "${OKF_PG_URL}"
        );
        assert_eq!(doc["mcpServers"]["pgokf"]["env"]["OKF_TENANT"], "acme");
        let toml = text(&codex, ".codex/config.toml");
        assert!(
            toml.contains("[mcp_servers.pgokf]") && toml.contains("env_vars = [\"OKF_PG_URL\"]")
        );
        assert!(!toml.contains("PASSWORD"));
        let yaml = text(&hermes, "okf-hermes-mcp.yaml");
        assert!(
            yaml.contains("mcp_servers:\n  pgokf:") && yaml.contains("${OKF_PG_URL}"),
            "Hermes references the variable rather than holding a placeholder: {yaml}"
        );
        assert!(text(&cursor, ".cursor/mcp.json").contains("${env:OKF_PG_URL}"));
        assert!(
            !paths(&ollama).iter().any(|p| p.contains("mcp")),
            "ollama has no MCP"
        );
        assert!(paths(&ollama).contains(&"okf-prompt/tools/okf.sh".to_owned()));
        assert!(text(&claude, MANIFEST_FILE).contains("components: [guide, mcp, tools]"));
    }

    #[test]
    fn mcp_command_and_web_url_are_validated() {
        // Arrange & Act & Assert
        assert!(validated_command(Some("pgokf-mcp; rm -rf /")).is_err());
        assert_eq!(
            validated_command(Some("/usr/local/bin/pgokf-mcp")).expect("ok"),
            "/usr/local/bin/pgokf-mcp"
        );
        // An Agent Plugin's command is a bare token or plugin-relative.
        assert_eq!(
            validated_plugin_command("pgokf-mcp").expect("ok"),
            "pgokf-mcp"
        );
        assert_eq!(
            validated_plugin_command("./bin/pgokf-mcp").expect("ok"),
            "./bin/pgokf-mcp"
        );
        for bad in [
            "/usr/local/bin/pgokf-mcp",
            "./../x",
            "bin/x",
            "C:\\x",
            "./a\\b",
        ] {
            assert!(validated_plugin_command(bad).is_err(), "{bad}");
        }
        assert!(validated_web_url(Some("javascript:alert(1)")).is_err());
        assert!(validated_web_url(Some("http://x y")).is_err());
        // The web URL is written into okf.sh, which ships executable: a
        // character a shell reads as syntax never reaches it.
        for hostile in [
            "http://h.example/$(id)",
            "http://h.example/`id`",
            "http://h.example/${IFS}",
            "http://h.example/;id",
            "http://h.example/&id",
            "http://h.example/|id",
            "http://h.example/>out",
            "http://h.example/\\",
        ] {
            assert!(validated_web_url(Some(hostile)).is_err(), "{hostile}");
        }
        assert_eq!(
            validated_web_url(Some("https://okf.example.test/"))
                .expect("ok")
                .as_deref(),
            Some("https://okf.example.test")
        );
        assert_eq!(Component::parse("docs"), Some(Component::Guide));
        assert_eq!(Component::parse("nope"), None);
    }

    #[test]
    fn an_mcp_endpoint_is_validated_and_never_carries_credentials() {
        // Arrange / Act / Assert
        assert_eq!(
            validated_mcp_url(Some(" https://catalog.example/mcp "))
                .expect("ok")
                .as_deref(),
            Some("https://catalog.example/mcp")
        );
        assert_eq!(
            validated_mcp_url(Some("http://127.0.0.1:8081/mcp"))
                .expect("ok")
                .as_deref(),
            Some("http://127.0.0.1:8081/mcp")
        );
        assert!(validated_mcp_url(None).expect("none").is_none());
        assert!(validated_mcp_url(Some("  ")).expect("blank").is_none());
        assert!(validated_mcp_url(Some("ftp://catalog.example/mcp")).is_err());
        assert!(validated_mcp_url(Some("javascript:alert(1)")).is_err());
        assert!(validated_mcp_url(Some("https://a b/mcp")).is_err());
        assert!(validated_mcp_url(Some("https:///mcp")).is_err(), "no host");
        assert!(
            validated_mcp_url(Some("https://pgokf_tok@catalog.example/mcp")).is_err(),
            "a token belongs in a header, not in every access log on the way"
        );
        for carried in [
            "https://catalog.example/mcp?token=s3cr3t",
            "https://catalog.example/mcp#access_token=s3cr3t",
            "https://catalog.example/mcp?x=1",
        ] {
            assert!(
                validated_mcp_url(Some(carried)).is_err(),
                "a query or fragment is where a token gets put by mistake: {carried}"
            );
        }
        assert!(
            validated_mcp_url(Some(&format!(
                "https://catalog.example/{}",
                "p".repeat(MCP_URL_MAX)
            )))
            .is_err(),
            "bounded like every other free-text option"
        );
    }

    #[test]
    fn an_agent_plugin_refuses_a_plaintext_endpoint_it_could_not_declare() {
        // Arrange: Agent Plugins 1.0.0 §7.2.1 - "Non-loopback endpoints
        // MUST use HTTPS", and the package declares that specification.
        let records = vec![record(1, "a", "Alpha", "human-reviewed", "Do A.")];
        let with = |url: &str| BuildOptions {
            components: vec![Component::Mcp],
            mcp_url: Some(url.to_owned()),
            ..options(Target::AgentPlugin)
        };
        let build = |url: &str| assemble(&with(url), &selection(), &snapshot(), &records);

        // Act / Assert
        assert!(build("http://catalog.example/mcp").is_err());
        assert!(build("https://catalog.example/mcp").is_ok());
        assert!(build("http://localhost:8081/mcp").is_ok(), "loopback");
        assert!(build("http://127.0.0.1:8081/mcp").is_ok(), "loopback");
        assert!(build("http://[::1]:8081/mcp").is_ok(), "loopback");
        // Another target may talk plain HTTP on a private network; only the
        // portable package carries the specification's constraint.
        assert!(
            assemble(
                &BuildOptions {
                    components: vec![Component::Mcp],
                    mcp_url: Some("http://10.0.0.4:8081/mcp".to_owned()),
                    ..options(Target::ClaudeCode)
                },
                &selection(),
                &snapshot(),
                &records
            )
            .is_ok()
        );
    }

    #[test]
    fn a_target_without_an_mcp_configuration_says_so_however_it_was_asked() {
        // Arrange
        let records = vec![record(1, "a", "Alpha", "human-reviewed", "Do A.")];
        let built = BuildOptions {
            components: vec![Component::Mcp, Component::Guide],
            mcp_url: Some("https://catalog.example/mcp".to_owned()),
            ..options(Target::Ollama)
        };

        // Act
        let plugin = assemble(&built, &selection(), &snapshot(), &records).expect("builds");
        let text = |path: &str| {
            String::from_utf8_lossy(
                &plugin
                    .files
                    .iter()
                    .find(|f| f.path.ends_with(path))
                    .expect(path)
                    .bytes,
            )
            .into_owned()
        };

        // Assert: no entry, no token talk, and a manifest that does not
        // claim the tree calls anything.
        let guide = text("USING-THE-CATALOG.md");
        assert!(guide.contains("This target has no MCP configuration"));
        assert!(!guide.contains("OKF_MCP_TOKEN"));
        assert!(!guide.contains("reader` token is offered"));
        let manifest = text(MANIFEST_FILE);
        assert!(manifest.contains("url_env: OKF_PG_URL"));
        assert!(!manifest.contains("mcp_url"));
    }

    #[test]
    fn an_endpoint_reaches_the_manifest_only_when_an_entry_was_written() {
        // Arrange
        let records = vec![record(1, "a", "Alpha", "human-reviewed", "Do A.")];
        let without_the_component = BuildOptions {
            components: vec![Component::Guide],
            mcp_url: Some("https://catalog.example/mcp".to_owned()),
            ..options(Target::ClaudeCode)
        };

        // Act
        let plugin =
            assemble(&without_the_component, &selection(), &snapshot(), &records).expect("builds");
        let manifest = String::from_utf8_lossy(
            &plugin
                .files
                .iter()
                .find(|f| f.path.ends_with(MANIFEST_FILE))
                .expect("manifest")
                .bytes,
        )
        .into_owned();

        // Assert
        assert!(!plugin.files.iter().any(|f| f.path == ".mcp.json"));
        assert!(!manifest.contains("mcp_url"), "{manifest}");
    }

    #[test]
    fn an_endpoint_and_a_command_are_alternatives() {
        // Arrange
        let records = vec![record(1, "a", "Alpha", "human-reviewed", "Do A.")];
        let both = BuildOptions {
            components: vec![Component::Mcp],
            mcp_command: Some("pgokf-mcp".to_owned()),
            mcp_url: Some("https://catalog.example/mcp".to_owned()),
            ..options(Target::ClaudeCode)
        };
        let unsupported = BuildOptions {
            components: vec![Component::Mcp],
            mcp_url: Some("https://catalog.example/mcp".to_owned()),
            ..options(Target::Ollama)
        };

        // Act
        let refused = assemble(&both, &selection(), &snapshot(), &records);
        let prompt_bundle = assemble(&unsupported, &selection(), &snapshot(), &records);

        // Assert
        assert!(
            refused
                .as_ref()
                .is_err_and(|e| format!("{e:#}").contains("alternatives")),
            "{refused:?}"
        );
        // Ollama has no MCP configuration at all, so an endpoint is simply
        // unused rather than refused.
        assert!(prompt_bundle.is_ok());
    }

    #[test]
    fn a_remote_entry_names_the_endpoint_in_each_harness_documented_form() {
        // Arrange
        let records = vec![record(1, "a", "Alpha", "human-reviewed", "Do A.")];
        let url = "https://catalog.example/mcp";
        let with = |target: Target| BuildOptions {
            components: vec![Component::Mcp, Component::Guide],
            tenant: Some("acme".to_owned()),
            mcp_url: Some(url.to_owned()),
            ..options(target)
        };
        let built = |target: Target| {
            assemble(&with(target), &selection(), &snapshot(), &records).expect("builds")
        };
        let text = |p: &Plugin, path: &str| {
            String::from_utf8_lossy(&p.files.iter().find(|f| f.path == path).expect(path).bytes)
                .into_owned()
        };

        // Act
        let claude = built(Target::ClaudeCode);
        let cursor = built(Target::Cursor);
        let codex = built(Target::Codex);
        let gemini = built(Target::GeminiCli);
        let copilot = built(Target::Copilot);
        let hermes = built(Target::HermesAgent);
        let plugin = built(Target::AgentPlugin);

        // Assert: each names the endpoint the way its own documentation
        // does, and each references the token rather than holding one.
        let entry = |p: &Plugin, path: &str| -> serde_json::Value {
            serde_json::from_str::<serde_json::Value>(&text(p, path)).expect("json")["mcpServers"]
                ["pgokf"]
                .clone()
        };
        let claude_entry = entry(&claude, ".mcp.json");
        assert_eq!(claude_entry["type"], "http");
        assert_eq!(claude_entry["url"], url);
        assert_eq!(
            claude_entry["headers"]["Authorization"],
            "Bearer ${OKF_MCP_TOKEN}"
        );
        assert!(claude_entry.get("command").is_none(), "nothing to start");

        let cursor_entry = entry(&cursor, ".cursor/mcp.json");
        assert_eq!(cursor_entry["url"], url);
        assert_eq!(
            cursor_entry["headers"]["Authorization"],
            "Bearer ${env:OKF_MCP_TOKEN}"
        );

        let codex_toml = text(&codex, ".codex/config.toml");
        assert!(
            codex_toml.contains(&format!("url = \"{url}\"")),
            "{codex_toml}"
        );
        assert!(codex_toml.contains("bearer_token_env_var = \"OKF_MCP_TOKEN\""));

        // Gemini CLI expands ${VAR} in a header value (and `httpUrl` is
        // deprecated in favour of `url` with a type), so it keeps its own
        // file and references the token.
        let gemini_entry = entry(&gemini, ".gemini/settings.json");
        assert_eq!(gemini_entry["type"], "http");
        assert_eq!(gemini_entry["url"], url);
        assert_eq!(
            gemini_entry["headers"]["Authorization"],
            "Bearer ${OKF_MCP_TOKEN}"
        );
        assert!(gemini_entry.get("httpUrl").is_none(), "deprecated");

        // The Copilot CLI documents header values as literals, so its entry
        // is a fragment with a placeholder, not a file in the workspace.
        let copilot_entry = entry(&copilot, "okf-mcp.json");
        assert_eq!(copilot_entry["type"], "http");
        assert_eq!(copilot_entry["tools"], serde_json::json!(["*"]));
        assert_eq!(copilot_entry["headers"]["Authorization"], "Bearer TOKEN");

        let hermes_yaml = text(&hermes, "okf-hermes-mcp.yaml");
        assert!(
            hermes_yaml.contains(&format!("url: \"{url}\"")),
            "{hermes_yaml}"
        );
        assert!(hermes_yaml.contains("Authorization: \"Bearer ${OKF_MCP_TOKEN}\""));

        // The Agent Plugins specification forbids a credential in a package
        // and defines no way to reference one, so the entry names the
        // endpoint and nothing else.
        let plugin_entry = entry(&plugin, "ops-runbooks/mcp.json");
        assert_eq!(plugin_entry["type"], "streamable-http");
        assert_eq!(plugin_entry["url"], url);
        assert!(plugin_entry.get("headers").is_none());

        // Nothing anywhere carries a tenant or a connection string, and the
        // guide says how the token gets there.
        for (built, path) in [
            (&claude, ".mcp.json"),
            (&cursor, ".cursor/mcp.json"),
            (&gemini, ".gemini/settings.json"),
        ] {
            let written = text(built, path);
            assert!(!written.contains("OKF_PG_URL"), "{path}");
            assert!(!written.contains("acme"), "{path}: no tenant over HTTP");
        }
        let guide = text(
            &claude,
            ".claude/skills/ops-runbooks/references/USING-THE-CATALOG.md",
        );
        assert!(guide.contains(url));
        assert!(guide.contains("set `OKF_MCP_TOKEN` in the environment"));
        assert!(guide.contains("a `reader` token is offered the five reading tools"));
    }

    #[test]
    fn a_harness_that_cannot_reference_a_secret_gets_a_fragment_to_merge() {
        // Arrange
        let records = vec![record(1, "a", "Alpha", "human-reviewed", "Do A.")];
        let with = |target: Target, url: Option<&str>| BuildOptions {
            components: vec![Component::Mcp, Component::Guide],
            mcp_url: url.map(str::to_owned),
            ..options(target)
        };
        let paths = |p: &Plugin| p.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
        let build = |target: Target, url: Option<&str>| {
            assemble(&with(target, url), &selection(), &snapshot(), &records).expect("builds")
        };
        let endpoint = Some("https://catalog.example/mcp");

        // Act
        let local = build(Target::Copilot, None);
        let remote = build(Target::Copilot, endpoint);
        let referenced = build(Target::ClaudeCode, endpoint);

        // Assert: the harness's own file only holds an entry it can fill
        // from the environment; otherwise the entry is a fragment.
        assert!(paths(&local).contains(&".github/mcp.json".to_owned()));
        assert!(!paths(&remote).contains(&".github/mcp.json".to_owned()));
        assert!(paths(&remote).contains(&"okf-mcp.json".to_owned()));
        assert!(
            paths(&referenced).contains(&".mcp.json".to_owned()),
            "a harness that can reference the token keeps its own file"
        );
        let guide = String::from_utf8_lossy(
            &remote
                .files
                .iter()
                .find(|f| f.path.ends_with("USING-THE-CATALOG.md"))
                .expect("guide")
                .bytes,
        )
        .into_owned();
        assert!(guide.contains("keep that file out of version control"));
        assert!(
            guide.contains("`.github/mcp.json`"),
            "the guide names the file to merge into: {guide}"
        );
    }

    #[test]
    fn the_lockfile_hashes_every_file_of_the_tree() {
        // Arrange: a build with the generated extras, including okf.sh -
        // the only executable file, and the one nothing used to cover.
        let records = vec![record(1, "a", "Alpha", "human-reviewed", "Do A.")];
        let built = assemble(
            &BuildOptions {
                components: vec![Component::Mcp, Component::Guide, Component::Tools],
                web_url: Some("https://okf.example.test".to_owned()),
                ..options(Target::ClaudeCode)
            },
            &selection(),
            &snapshot(),
            &records,
        )
        .expect("builds");

        // Act
        let lock: serde_json::Value = serde_json::from_slice(
            &built
                .files
                .iter()
                .find(|f| f.path.ends_with(LOCK_FILE))
                .expect("lockfile")
                .bytes,
        )
        .expect("json");
        let listed: Vec<&str> = lock["files"]
            .as_array()
            .expect("files")
            .iter()
            .filter_map(|f| f["path"].as_str())
            .collect();

        // Assert: every file but the lockfile itself, with its hash.
        for f in &built.files {
            if f.path.ends_with(LOCK_FILE) {
                continue;
            }
            assert!(listed.contains(&f.path.as_str()), "{} is unlisted", f.path);
        }
        assert!(!listed.iter().any(|p| p.ends_with(LOCK_FILE)));
        let helper = lock["files"]
            .as_array()
            .expect("files")
            .iter()
            .find(|f| f["path"].as_str().is_some_and(|p| p.ends_with("okf.sh")))
            .expect("the helper is covered");
        assert_eq!(helper["executable"], true);
        assert_eq!(
            helper["sha256"],
            built
                .files
                .iter()
                .find(|f| f.path.ends_with("okf.sh"))
                .expect("helper")
                .sha256
                .as_str()
        );
    }

    #[test]
    fn the_manifest_records_the_endpoint_a_build_points_at() {
        // Arrange / Act
        let local = manifest_yaml("ops", Target::Cursor, None, &selection(), &[], None, false);
        let remote = manifest_yaml(
            "ops",
            Target::Cursor,
            None,
            &selection(),
            &[],
            Some("https://catalog.example/mcp"),
            false,
        );

        // Assert
        assert!(local.contains("catalog:\n  url_env: OKF_PG_URL"));
        assert!(remote.contains("mcp_url: \"https://catalog.example/mcp\""));
        assert!(remote.contains("token_env: OKF_MCP_TOKEN"));
        assert!(!remote.contains("url_env: OKF_PG_URL"));
    }

    #[test]
    fn the_helper_script_treats_its_default_url_as_data() {
        // Arrange: the URL is interpolated into a shell script that ships
        // executable, so it must land somewhere the shell expands nothing.
        let script = tools_script(&BuildOptions {
            web_url: Some("https://catalog.example".to_owned()),
            ..options(Target::Generic)
        });

        // Act
        let assignment = script
            .lines()
            .find(|line| line.starts_with("OKF_DEFAULT_URL="))
            .expect("the default is its own assignment");

        // Assert: single-quoted, and the validator keeps a quote out of it,
        // so nothing in the value can be read as syntax.
        assert_eq!(assignment, "OKF_DEFAULT_URL='https://catalog.example'");
        assert!(script.contains("BASE=\"${OKF_WEB_URL:-$OKF_DEFAULT_URL}\""));
        assert!(
            !script.contains("${OKF_WEB_URL:-https://"),
            "the URL must not sit in an expansion's default branch"
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
        let yaml = manifest_yaml(
            "ops",
            Target::Cursor,
            None,
            &selection,
            &[Component::Mcp],
            None,
            false,
        );

        // Assert
        assert!(yaml.contains("targets: [cursor]\ncomponents: [mcp]\n"));
        assert!(yaml.contains("  trust: verified-only\n"));
        assert!(yaml.contains("  - { bundle_ids: [2], types: [\"Runbook\"], tags: [\"ops\"], query: \"failover\", limit: 50 }\n"));
    }

    #[test]
    fn a_skill_package_is_copied_whole_beside_the_knowledge_skill() {
        // Arrange: one document and one package.
        let records = vec![
            record(1, "runbooks/a", "Alpha", "human-reviewed", "Do A."),
            package_record(1, "deploy"),
        ];

        // Act
        let plugin = assemble(
            &options(Target::ClaudeCode),
            &selection(),
            &snapshot(),
            &records,
        )
        .expect("assembles");

        // Assert: the package lands under its own name with every file
        // byte-identical, scripts executable, and the index links to it.
        let paths: Vec<(&str, bool)> = plugin
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.executable))
            .collect();
        assert_eq!(
            paths,
            vec![
                (".claude/skills/ops-runbooks/SKILL.md", false),
                (
                    ".claude/skills/ops-runbooks/references/runbooks/a.md",
                    false
                ),
                (".claude/skills/deploy/SKILL.md", false),
                (".claude/skills/deploy/scripts/run.sh", true),
                (".claude/skills/deploy/assets/logo.png", false),
                (MANIFEST_FILE, false),
                (LOCK_FILE, false),
            ]
        );
        assert_eq!(plugin.package_count, 1);
        assert_eq!(plugin.files[2].bytes, records[1].bytes);
        assert_eq!(plugin.files[3].bytes, b"#!/bin/sh\necho run\n");
        let index = String::from_utf8(plugin.files[0].bytes.clone()).expect("utf-8");
        assert!(index.contains("## Skills"));
        assert!(index.contains("- [deploy](../deploy/SKILL.md) \u{2014} human-reviewed. About deploy. (1 script, 1 reference) `(1:skills/deploy/SKILL)`"), "{index}");
        let lock: serde_json::Value = serde_json::from_slice(&plugin.files[6].bytes).expect("json");
        let entries = lock["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[1]["package_hash"], "p".repeat(64));
        assert_eq!(entries[1]["package_directory"], "deploy");
        assert_eq!(entries[1]["file_hash"], "ff", "the manifest's own hash");
        assert_eq!(entries[2]["concept_id"], "skills/deploy/scripts/run.sh");
        assert_eq!(entries[2]["package_concept_id"], "skills/deploy/SKILL");
        assert_eq!(
            entries[2]["file_hash"], "fs",
            "a member's own hash, not the manifest's"
        );
        assert_eq!(entries[2]["content_sha256"], plugin.files[3].sha256);
    }

    #[test]
    fn packages_go_under_the_knowledge_tree_for_other_shapes() {
        // Arrange
        let records = vec![package_record(1, "deploy")];

        // Act
        let agents = assemble(
            &options(Target::AgentsMd),
            &selection(),
            &snapshot(),
            &records,
        )
        .expect("assembles");
        let generic = assemble(
            &options(Target::Generic),
            &selection(),
            &snapshot(),
            &records,
        )
        .expect("assembles");

        // Assert
        let agents_paths: Vec<&str> = agents.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            agents_paths[..4],
            [
                "AGENTS.md",
                "knowledge/skills/deploy/SKILL.md",
                "knowledge/skills/deploy/scripts/run.sh",
                "knowledge/skills/deploy/assets/logo.png",
            ]
        );
        let agents_md = String::from_utf8(agents.files[0].bytes.clone()).expect("utf-8");
        assert!(
            agents_md.contains("[deploy](knowledge/skills/deploy/SKILL.md)"),
            "{agents_md}"
        );
        let generic_paths: Vec<&str> = generic.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(generic_paths[1], "okf-knowledge/skills/deploy/SKILL.md");
        let index = String::from_utf8(generic.files[0].bytes.clone()).expect("utf-8");
        assert!(
            index.contains("[deploy](skills/deploy/SKILL.md)"),
            "{index}"
        );
    }

    #[test]
    fn package_directories_never_collide_with_each_other_or_the_plugin() {
        // Arrange: two packages named alike in different bundles, and one
        // named like the plugin itself, for a shape that lists packages in
        // the index rather than discovering them by directory.
        let mut clash = package_record(2, "deploy");
        clash.bundle_name = "bundle-2".to_owned();
        let mut same_as_plugin = package_record(1, "ops-runbooks");
        same_as_plugin.concept_id = "skills/ops-runbooks/SKILL".to_owned();
        let records = vec![package_record(1, "deploy"), clash, same_as_plugin];

        // Act
        let plugin = assemble(
            &options(Target::Generic),
            &selection(),
            &snapshot(),
            &records,
        )
        .expect("assembles");

        // Assert
        let manifests: Vec<&str> = plugin
            .files
            .iter()
            .map(|f| f.path.as_str())
            .filter(|p| p.ends_with("/SKILL.md"))
            .collect();
        assert_eq!(
            manifests,
            [
                "okf-knowledge/skills/deploy/SKILL.md",
                "okf-knowledge/skills/deploy-2/SKILL.md",
                "okf-knowledge/skills/ops-runbooks-1/SKILL.md",
            ]
        );
    }

    #[test]
    fn directory_discovered_shapes_refuse_colliding_package_names() {
        // Arrange: a harness that discovers skills by directory would skip a
        // package whose SKILL.md name differs from its directory.
        let mut clash = package_record(2, "deploy");
        clash.bundle_name = "bundle-2".to_owned();
        let two_deploys = vec![package_record(1, "deploy"), clash];
        let mut same_as_plugin = package_record(1, "ops-runbooks");
        same_as_plugin.concept_id = "skills/ops-runbooks/SKILL".to_owned();
        let like_the_plugin = vec![same_as_plugin];

        // Act
        let skills = assemble(
            &options(Target::ClaudeCode),
            &selection(),
            &snapshot(),
            &two_deploys,
        );
        let portable = assemble(
            &options(Target::AgentPlugin),
            &selection(),
            &snapshot(),
            &like_the_plugin,
        );
        let fine = assemble(
            &options(Target::ClaudeCode),
            &selection(),
            &snapshot(),
            &[package_record(1, "deploy")],
        );

        // Assert
        let skills_error = skills.expect_err("two packages named deploy").to_string();
        assert!(skills_error.contains("deploy (bundle 2)"), "{skills_error}");
        let portable_error = portable
            .expect_err("package named like the plugin")
            .to_string();
        assert!(
            portable_error.contains("plugin's own directory"),
            "{portable_error}"
        );
        assert!(fine.is_ok());
    }

    #[test]
    fn a_standalone_script_lands_in_the_scripts_directory_executable() {
        // Arrange: a script selected without its package.
        use crate::selection::ResourceRecord;
        let mut script = record(
            1,
            "skills/deploy/scripts/run.sh",
            "run.sh",
            "unverified",
            "",
        );
        script.path = "skills/deploy/scripts/run.sh".to_owned();
        script.concept_type = Some("Script".to_owned());
        script.bytes = b"#!/bin/sh\necho run\n".to_vec();
        script.resource = Some(ResourceRecord {
            class: "script".to_owned(),
            source_path: "scripts/run.sh".to_owned(),
            package_concept_id: "skills/deploy/SKILL".to_owned(),
            sha256: "s".repeat(64),
        });
        let records = vec![
            record(1, "runbooks/a", "Alpha", "human-reviewed", "Do A."),
            script,
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
        let script_file = plugin
            .files
            .iter()
            .find(|f| f.path.ends_with("run.sh"))
            .expect("the script is written");
        assert_eq!(
            script_file.path,
            ".claude/skills/ops-runbooks/scripts/skills/deploy/scripts/run.sh"
        );
        assert!(script_file.executable);
        assert_eq!(plugin.package_count, 0);
        // The index links to where the script was written, not to references/.
        let index = String::from_utf8(plugin.files[0].bytes.clone()).expect("utf-8");
        assert!(
            index.contains("[run.sh](scripts/skills/deploy/scripts/run.sh)"),
            "{index}"
        );
        let lock: serde_json::Value =
            serde_json::from_slice(&plugin.files.last().unwrap().bytes).expect("json");
        let entry = lock["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["concept_id"] == "skills/deploy/scripts/run.sh")
            .expect("the script has a lock entry");
        assert_eq!(entry["package_concept_id"], "skills/deploy/SKILL");
    }

    #[test]
    fn same_bundle_same_name_packages_and_long_names_get_bounded_directories() {
        // Arrange: two packages in one bundle whose manifests declare the same
        // name (different directories), one of them 64 characters long, for
        // a shape that lists packages in its index.
        let long = "a".repeat(NAME_MAX);
        let mut first = package_record(1, &long);
        first.concept_id = "skills/first/SKILL".to_owned();
        let mut second = package_record(1, &long);
        second.concept_id = "skills/second/SKILL".to_owned();
        let records = vec![first, second];

        // Act
        let plugin = assemble(
            &options(Target::Generic),
            &selection(),
            &snapshot(),
            &records,
        )
        .expect("assembles");

        // Assert: the second directory is suffixed and still a valid name.
        let dirs: Vec<&str> = plugin
            .files
            .iter()
            .filter(|f| f.path.ends_with("/SKILL.md"))
            .filter_map(|f| f.path.strip_prefix("okf-knowledge/skills/"))
            .map(|rest| rest.split('/').next().unwrap())
            .collect();
        assert_eq!(dirs[0], long);
        assert_eq!(dirs[1], format!("{}-1", "a".repeat(NAME_MAX - 2)));
        assert!(dirs[1].len() <= NAME_MAX);
        let index = String::from_utf8(plugin.files[0].bytes.clone()).expect("utf-8");
        assert!(index.contains("(installed as `"), "{index}");
    }

    #[test]
    fn prompt_bundles_never_inline_binary_bytes() {
        // Arrange: a standalone binary asset beside a document.
        use crate::selection::ResourceRecord;
        let mut asset = record(
            1,
            "skills/deploy/assets/logo.png",
            "logo.png",
            "unverified",
            "",
        );
        asset.path = "skills/deploy/assets/logo.png".to_owned();
        asset.concept_type = Some("Reference".to_owned());
        asset.bytes = b"\x89PNG\r\n\x1a\n\xff\xfe".to_vec();
        asset.resource = Some(ResourceRecord {
            class: "asset".to_owned(),
            source_path: "assets/logo.png".to_owned(),
            package_concept_id: "skills/deploy/SKILL".to_owned(),
            sha256: String::new(),
        });
        let records = vec![
            record(1, "runbooks/a", "Alpha", "human-reviewed", "Do A."),
            asset,
        ];

        // Act
        let plugin = assemble(
            &options(Target::Ollama),
            &selection(),
            &snapshot(),
            &records,
        )
        .expect("assembles");

        // Assert
        let prompt = plugin
            .files
            .iter()
            .find(|f| f.path.ends_with("system-prompt.md"))
            .expect("prompt exists");
        let text = String::from_utf8(prompt.bytes.clone()).expect("the prompt is UTF-8");
        assert!(text.contains("## Alpha"));
        assert!(!text.contains("logo.png"), "{text}");
        assert!(text.contains("1 of 2 documents inlined"));
    }

    #[test]
    fn tree_paths_neutralise_colons() {
        // Arrange / Act / Assert
        assert_eq!(
            tree_path("references/a:b.txt").unwrap(),
            "references/a_b.txt"
        );
    }

    #[test]
    fn relative_prefix_climbs_out_of_the_index_directory() {
        // Arrange / Act / Assert
        assert_eq!(relative_prefix(".claude/skills/x", ".claude/skills"), "../");
        assert_eq!(relative_prefix("", "knowledge/skills"), "knowledge/skills/");
        assert_eq!(
            relative_prefix("okf-knowledge", "okf-knowledge/skills"),
            "skills/"
        );
        assert_eq!(relative_prefix("a/b", "a/b"), "");
        assert_eq!(relative_prefix("a/b", "c"), "../../c/");
    }

    #[test]
    fn an_agent_plugin_is_a_self_contained_directory_with_manifest_and_mcp() {
        // Arrange: a document and a stored package, all extras on.
        let records = vec![
            record(1, "runbooks/a", "Alpha", "human-reviewed", "Do A."),
            package_record(1, "deploy"),
        ];
        let mut opts = options(Target::AgentPlugin);
        opts.components = Component::all().to_vec();
        opts.tenant = Some("acme".to_owned());
        opts.web_url = Some("http://catalog.example:8080".to_owned());

        // Act
        let plugin = assemble(&opts, &selection(), &snapshot(), &records).expect("assembles");

        // Assert: the Agent Plugins layout, everything under the plugin dir.
        let paths: Vec<&str> = plugin.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(plugin.root, "ops-runbooks");
        assert_eq!(
            paths,
            vec![
                "ops-runbooks/skills/ops-runbooks/SKILL.md",
                "ops-runbooks/plugin.json",
                "ops-runbooks/skills/ops-runbooks/references/runbooks/a.md",
                "ops-runbooks/skills/deploy/SKILL.md",
                "ops-runbooks/skills/deploy/scripts/run.sh",
                "ops-runbooks/skills/deploy/assets/logo.png",
                "ops-runbooks/mcp.json",
                "ops-runbooks/skills/ops-runbooks/references/USING-THE-CATALOG.md",
                "ops-runbooks/skills/ops-runbooks/scripts/okf.sh",
                "ops-runbooks/okf-workspace.yaml",
                "ops-runbooks/okf-workspace.lock",
            ]
        );
        // plugin.json: the closed portable manifest, no other top-level key.
        let manifest: serde_json::Value =
            serde_json::from_slice(&plugin.files[1].bytes).expect("json");
        assert_eq!(manifest["$schema"], AGENT_PLUGIN_SCHEMA);
        assert_eq!(manifest["name"], "ops-runbooks");
        assert!(
            manifest["version"]
                .as_str()
                .unwrap()
                .starts_with("1.20260906.0+"),
            "{}",
            manifest["version"]
        );
        assert_eq!(manifest["homepage"], "http://catalog.example:8080");
        assert_eq!(
            manifest["keywords"],
            serde_json::json!(["Runbook", "Skill", "ops"])
        );
        let keys: Vec<&str> = manifest
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for key in &keys {
            assert!(
                [
                    "$schema",
                    "name",
                    "version",
                    "description",
                    "author",
                    "homepage",
                    "repository",
                    "license",
                    "keywords",
                    "extensions"
                ]
                .contains(key),
                "{key} is not a portable manifest field"
            );
        }
        // mcp.json: typed stdio entry, one-token command, ${PLUGIN_DATA} env file, no URL.
        let mcp: serde_json::Value = serde_json::from_slice(&plugin.files[6].bytes).expect("json");
        assert_eq!(mcp["$schema"], AGENT_PLUGIN_MCP_SCHEMA);
        let server = &mcp["mcpServers"]["pgokf"];
        assert_eq!(server["type"], "stdio");
        assert_eq!(server["command"], "pgokf-mcp");
        assert_eq!(
            server["args"],
            serde_json::json!(["--env-file", "${PLUGIN_DATA}/pgokf.env"])
        );
        assert_eq!(server["env"], serde_json::json!({ "OKF_TENANT": "acme" }));
        assert!(!String::from_utf8_lossy(&plugin.files[6].bytes).contains("postgresql://"));
        assert!(!String::from_utf8_lossy(&plugin.files[6].bytes).contains("OKF_PG_URL"));
    }

    #[test]
    fn the_plugin_version_follows_every_file_of_the_plugin() {
        // Arrange
        let same = vec![record(1, "runbooks/a", "Alpha", "human-reviewed", "Do A.")];
        let changed = vec![record(
            1,
            "runbooks/a",
            "Alpha",
            "human-reviewed",
            "Do A differently.",
        )];
        let mut titled = options(Target::AgentPlugin);
        titled.title = Some("Other title".to_owned());
        let mut with_mcp = options(Target::AgentPlugin);
        with_mcp.components = vec![Component::Mcp];
        let mut linked = options(Target::AgentPlugin);
        linked.web_url = Some("http://catalog.example:8080".to_owned());
        let version = |opts: &BuildOptions, records: &[ConceptRecord]| -> String {
            let plugin = assemble(opts, &selection(), &snapshot(), records).expect("assembles");
            let manifest: serde_json::Value = serde_json::from_slice(
                &plugin
                    .files
                    .iter()
                    .find(|f| f.path.ends_with("/plugin.json"))
                    .unwrap()
                    .bytes,
            )
            .expect("json");
            manifest["version"].as_str().unwrap().to_owned()
        };
        let base = options(Target::AgentPlugin);

        // Act / Assert: identical input, identical version; any other file
        // of the plugin changing changes the build metadata.
        assert_eq!(version(&base, &same), version(&base, &same));
        assert_ne!(version(&base, &same), version(&base, &changed));
        assert_ne!(version(&base, &same), version(&titled, &same));
        assert_ne!(version(&base, &same), version(&with_mcp, &same));
        assert_ne!(version(&base, &same), version(&linked, &same));
    }

    #[test]
    fn the_ordered_version_comes_from_the_newest_sync() {
        // Arrange
        let mut newer = snapshot();
        newer.bundles.push(BundleState {
            id: 2,
            name: "bundle-2".to_owned(),
            sync_hash: None,
            last_synced_at: Some("2026-09-07T01:02:03.5+00:00".to_owned()),
            catalog_generation: None,
            freshness_state: None,
            freshness_reasons: Vec::new(),
            embedding_contract: None,
        });
        let mut unsynced = snapshot();
        unsynced.bundles[0].last_synced_at = None;

        // Act / Assert
        assert_eq!(ordered_version(&snapshot()), "1.20260906.0");
        assert_eq!(ordered_version(&newer), "1.20260907.3723");
        assert_eq!(ordered_version(&unsynced), "1.0.0");
    }

    #[test]
    fn copilot_writes_a_typed_local_entry_without_a_connection_string() {
        // Arrange
        let records = vec![record(1, "runbooks/a", "Alpha", "human-reviewed", "Do A.")];
        let mut opts = options(Target::Copilot);
        opts.components = vec![Component::Mcp, Component::Guide];
        opts.tenant = Some("acme".to_owned());

        // Act
        let plugin = assemble(&opts, &selection(), &snapshot(), &records).expect("assembles");

        // Assert: the skill under .github/skills, the MCP entry Copilot's way.
        let paths: Vec<&str> = plugin.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            paths.contains(&".github/skills/ops-runbooks/SKILL.md"),
            "{paths:?}"
        );
        let mcp = plugin
            .files
            .iter()
            .find(|f| f.path == ".github/mcp.json")
            .expect("mcp file");
        let doc: serde_json::Value = serde_json::from_slice(&mcp.bytes).expect("json");
        let server = &doc["mcpServers"]["pgokf"];
        assert_eq!(server["type"], "local");
        assert_eq!(server["command"], "pgokf-mcp");
        assert_eq!(server["tools"], serde_json::json!(["*"]));
        assert_eq!(server["env"], serde_json::json!({ "OKF_TENANT": "acme" }));
        assert!(!String::from_utf8_lossy(&mcp.bytes).contains("OKF_PG_URL"));
        let guide = plugin
            .files
            .iter()
            .find(|f| f.path.ends_with("USING-THE-CATALOG.md"))
            .expect("guide");
        assert!(
            String::from_utf8_lossy(&guide.bytes)
                .contains("starts the server with its own environment"),
            "the guide says where OKF_PG_URL goes"
        );
    }

    #[test]
    fn a_custom_harness_builds_under_its_directory_and_is_recorded() {
        // Arrange
        let records = vec![record(1, "runbooks/a", "Alpha", "human-reviewed", "Do A.")];
        let mut opts = options(Target::Custom);
        opts.harness = Some(
            CustomHarness::new("Acme Agent", Shape::Skills, Some(".acme/skills")).expect("valid"),
        );
        opts.components = vec![Component::Mcp];

        // Act
        let plugin = assemble(&opts, &selection(), &snapshot(), &records).expect("assembles");

        // Assert
        let paths: Vec<&str> = plugin.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(plugin.root, ".acme/skills/ops-runbooks");
        assert!(
            paths.contains(&".acme/skills/ops-runbooks/SKILL.md"),
            "{paths:?}"
        );
        assert!(
            paths.contains(&"okf-mcp.json"),
            "the shared snippet: {paths:?}"
        );
        let manifest = plugin
            .files
            .iter()
            .find(|f| f.path == MANIFEST_FILE)
            .expect("manifest");
        let text = String::from_utf8_lossy(&manifest.bytes);
        assert!(text.contains("targets: [custom]\n"), "{text}");
        assert!(
            text.contains(
                "harness: { label: \"Acme Agent\", kind: skills, skills_dir: \".acme/skills\" }\n"
            ),
            "{text}"
        );
        let lock: serde_json::Value =
            serde_json::from_slice(&plugin.files.last().unwrap().bytes).expect("json");
        assert_eq!(lock["target"], "custom");
        assert_eq!(lock["harness"]["skills_dir"], ".acme/skills");
        assert_eq!(lock["harness"]["kind"], "skills");
    }

    #[test]
    fn a_custom_target_without_a_harness_is_refused() {
        // Arrange
        let records = vec![record(1, "runbooks/a", "Alpha", "human-reviewed", "Do A.")];

        // Act
        let error = assemble(
            &options(Target::Custom),
            &selection(),
            &snapshot(),
            &records,
        )
        .expect_err("no harness")
        .to_string();

        // Assert
        assert!(error.contains("needs a harness"), "{error}");
    }

    #[test]
    fn the_manifest_records_the_everything_scope() {
        // Arrange
        let selection = Selection {
            all: true,
            types: vec!["Runbook".to_owned()],
            ..Selection::default()
        };

        // Act
        let manifest = manifest_yaml("kit", Target::Generic, None, &selection, &[], None, false);

        // Assert
        assert!(
            manifest.contains("- { all: true, types: [\"Runbook\"], limit: 100 }"),
            "{manifest}"
        );
        assert!(!manifest.contains("harness:"));
    }

    #[test]
    fn the_manifest_records_picks() {
        // Arrange
        let selection = Selection {
            picks: vec![crate::selection::ConceptRef {
                bundle_id: 3,
                concept_id: "skills/deploy/SKILL".to_owned(),
            }],
            ..Selection::default()
        };

        // Act
        let manifest = manifest_yaml(
            "kit",
            Target::AgentPlugin,
            None,
            &selection,
            &[],
            None,
            false,
        );

        // Assert
        assert!(
            manifest.contains("picks: [\"3:skills/deploy/SKILL\"]"),
            "{manifest}"
        );
        assert!(manifest.contains("targets: [agent-plugin]"));
    }

    /// A record the catalog reported stale.
    fn stale_record(bundle_id: i64, id: &str, exact: bool) -> ConceptRecord {
        let mut r = record(bundle_id, id, id, "unverified", "body");
        r.exact = exact;
        r.freshness = crate::selection::Freshness {
            state: "stale".to_owned(),
            reasons: vec!["upstream_changed".to_owned()],
            scope: "bundle".to_owned(),
            stale_since: None,
            observed_revision: Some("r2".to_owned()),
            indexed_revision: Some("r1".to_owned()),
            published_revision: Some("41".to_owned()),
            catalog_generation: Some(42),
            last_reconciled_at: None,
        };
        r
    }

    fn report() -> BuildReport {
        BuildReport {
            freshness_available: true,
            ..BuildReport::default()
        }
    }

    #[test]
    fn the_adjacent_warning_path_swaps_the_last_extension() {
        // Arrange / Act / Assert
        assert_eq!(
            adjacent_warning_path("pkg/skills/deploy/SKILL.md"),
            "pkg/skills/deploy/SKILL.stale-warning.md"
        );
        assert_eq!(
            adjacent_warning_path("references/a/b.md"),
            "references/a/b.stale-warning.md"
        );
        assert_eq!(adjacent_warning_path("run.sh"), "run.stale-warning.md");
        assert_eq!(
            adjacent_warning_path("assets/.gitkeep"),
            "assets/.gitkeep.stale-warning.md"
        );
        assert_eq!(adjacent_warning_path("noext"), "noext.stale-warning.md");
    }

    #[test]
    fn a_stale_exact_package_keeps_its_bytes_and_gets_an_adjacent_warning() {
        // Arrange
        let mut package = package_record(1, "deploy");
        package.freshness = stale_record(1, "x", true).freshness;
        let original_manifest = package.bytes.clone();
        let original_script = package.package.as_ref().unwrap().resources[0].bytes.clone();

        // Act
        let plugin = assemble_with_report(
            &options(Target::ClaudeCode),
            &selection(),
            &snapshot(),
            &[package],
            &report(),
        )
        .expect("builds");

        // Assert: the package bytes are untouched, the warning is adjacent,
        // and the report and index exist.
        let bytes_of = |path: &str| {
            plugin
                .files
                .iter()
                .find(|f| f.path == path)
                .map(|f| f.bytes.clone())
        };
        assert_eq!(
            bytes_of(".claude/skills/deploy/SKILL.md"),
            Some(original_manifest)
        );
        assert_eq!(
            bytes_of(".claude/skills/deploy/scripts/run.sh"),
            Some(original_script)
        );
        let warning = String::from_utf8(
            bytes_of(".claude/skills/deploy/SKILL.stale-warning.md").expect("adjacent warning"),
        )
        .expect("text");
        assert!(warning.contains("state `stale`"), "{warning}");
        assert!(warning.contains("upstream_changed"), "{warning}");
        assert!(warning.contains("catalog generation 42"), "{warning}");
        assert!(
            warning.contains("<!-- pgokf-stale {"),
            "machine-readable evidence: {warning}"
        );
        assert!(bytes_of(FRESHNESS_FILE).is_some(), "top-level report");
        let index =
            String::from_utf8(bytes_of(".claude/skills/ops-runbooks/SKILL.md").expect("index"))
                .expect("text");
        assert!(
            index.contains("[FRESHNESS.md](../../../FRESHNESS.md)"),
            "{index}"
        );
        assert_eq!(plugin.warnings.len(), 1);
        assert_eq!(plugin.warnings[0].role, "selected");
    }

    #[test]
    fn a_stale_reconstructed_document_carries_a_banner_after_its_frontmatter() {
        // Arrange
        let stale = stale_record(1, "runbooks/a", false);
        let original = stale.bytes.clone();

        // Act
        let plugin = assemble_with_report(
            &options(Target::Generic),
            &selection(),
            &snapshot(),
            &[stale],
            &report(),
        )
        .expect("builds");

        // Assert: the banner follows the frontmatter, and there is no
        // adjacent warning for a reconstructed document.
        let text = String::from_utf8(
            plugin
                .files
                .iter()
                .find(|f| f.path == "okf-knowledge/concepts/runbooks/a.md")
                .expect("the document")
                .bytes
                .clone(),
        )
        .expect("text");
        let original_text = String::from_utf8(original).expect("text");
        assert!(text.starts_with("---\ntitle:"), "{text}");
        assert!(text.contains("> [!WARNING]"), "{text}");
        assert!(text.contains("<!-- pgokf-stale {"), "{text}");
        assert!(
            text.contains("catalog generation 42"),
            "the banner carries the evidence: {text}"
        );
        assert!(
            text.ends_with(&original_text) || text.contains("# runbooks/a"),
            "{text}"
        );
        assert!(
            !plugin
                .files
                .iter()
                .any(|f| f.path.contains("a.stale-warning.md")),
            "no adjacent warning for generated bytes"
        );
    }

    #[test]
    fn a_stale_package_member_gets_its_own_warning_lock_entry_and_report_row() {
        // Arrange: a fresh package whose script the catalog flagged stale.
        let mut package = package_record(1, "deploy");
        package.package.as_mut().unwrap().member_freshness = std::collections::BTreeMap::from([(
            "skills/deploy/scripts/run.sh".to_owned(),
            stale_record(1, "skills/deploy/scripts/run.sh", true).freshness,
        )]);
        let original_script = package.package.as_ref().unwrap().resources[0].bytes.clone();

        // Act
        let plugin = assemble_with_report(
            &options(Target::ClaudeCode),
            &selection(),
            &snapshot(),
            &[package],
            &report(),
        )
        .expect("builds");
        let bytes_of = |path: &str| {
            plugin
                .files
                .iter()
                .find(|f| f.path == path)
                .map(|f| f.bytes.clone())
        };

        // Assert: the member's bytes stay exact, its warning sits beside the
        // member file with the member's own evidence, the lock entry carries
        // the member's state, and the member is in the report and warnings.
        assert_eq!(
            bytes_of(".claude/skills/deploy/scripts/run.sh"),
            Some(original_script)
        );
        let warning = String::from_utf8(
            bytes_of(".claude/skills/deploy/scripts/run.stale-warning.md")
                .expect("the member gets its own adjacent warning"),
        )
        .expect("text");
        assert!(warning.contains("state `stale`"), "{warning}");
        assert!(
            warning.contains("skills/deploy/scripts/run.sh"),
            "{warning}"
        );
        assert!(warning.contains("<!-- pgokf-stale {"), "{warning}");
        assert!(
            bytes_of(".claude/skills/deploy/SKILL.stale-warning.md").is_none(),
            "the fresh package itself gets no warning"
        );
        let lock: serde_json::Value = serde_json::from_str(
            std::str::from_utf8(&bytes_of(LOCK_FILE).expect("lockfile")).expect("text"),
        )
        .expect("json");
        assert_eq!(lock["policy"]["stale"], "warn", "the build engaged");
        let entry = lock["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .find(|e| e["concept_id"] == "skills/deploy/scripts/run.sh")
            .expect("the member has a lock entry");
        assert_eq!(
            entry["freshness"]["state"], "stale",
            "the member's own state, not the fresh package's"
        );
        assert_eq!(entry["freshness"]["reasons"][0], "upstream_changed");
        let manifest_entry = lock["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .find(|e| e["concept_id"] == "skills/deploy/SKILL")
            .expect("the package has a lock entry");
        assert_eq!(manifest_entry["freshness"]["state"], "unknown");
        let report_text =
            String::from_utf8(bytes_of(FRESHNESS_FILE).expect("report")).expect("text");
        assert!(
            report_text.contains("skills/deploy/scripts/run.sh"),
            "{report_text}"
        );
        assert_eq!(plugin.warnings.len(), 1);
        assert_eq!(
            plugin.warnings[0].concept_id,
            "skills/deploy/scripts/run.sh"
        );
        assert_eq!(plugin.warnings[0].role, "selected");
    }

    #[test]
    fn a_prompt_bundle_marks_inlined_stale_documents_in_both_artifacts() {
        // Arrange: a stale stored-source document.
        let stale = stale_record(1, "runbooks/a", true);
        let original = stale.bytes.clone();

        // Act
        let plugin = assemble_with_report(
            &options(Target::Ollama),
            &selection(),
            &snapshot(),
            &[stale],
            &report(),
        )
        .expect("builds");
        let text_of = |path: &str| {
            String::from_utf8(
                plugin
                    .files
                    .iter()
                    .find(|f| f.path == path)
                    .unwrap_or_else(|| panic!("{path} exists"))
                    .bytes
                    .clone(),
            )
            .expect("text")
        };

        // Assert: the inlined knowledge carries its freshness warning in the
        // prompt and the Modelfile built from it, while the exported source
        // keeps its exact bytes (with its own adjacent warning).
        let prompt = text_of("okf-prompt/system-prompt.md");
        assert!(prompt.contains("<!-- pgokf-stale {"), "{prompt}");
        assert!(prompt.contains("Stale catalog content"), "{prompt}");
        let modelfile = text_of("okf-prompt/Modelfile");
        assert!(modelfile.contains("<!-- pgokf-stale {"), "{modelfile}");
        assert!(modelfile.contains("Stale catalog content"), "{modelfile}");
        let exported = plugin
            .files
            .iter()
            .find(|f| f.path == "okf-prompt/knowledge/runbooks/a.md")
            .expect("the exported source");
        assert_eq!(exported.bytes, original, "exported bytes stay exact");
    }

    #[test]
    fn the_stale_notice_counts_against_the_prompt_budget() {
        // Arrange: a document that fills the budget closely enough that its
        // stale notice no longer fits beside it.
        let body = "y".repeat(PROMPT_BUDGET - 400);
        let mut fresh = record(1, "big", "Big", "unverified", "");
        fresh.bytes = body.clone().into_bytes();
        let mut stale = stale_record(1, "big", true);
        stale.title = Some("Big".to_owned());
        stale.bytes = body.into_bytes();

        // Act
        let fresh_prompt = system_prompt("T", &options(Target::Ollama), &[fresh]);
        let stale_prompt = system_prompt("T", &options(Target::Ollama), &[stale]);

        // Assert
        assert!(
            !fresh_prompt.contains("documents inlined"),
            "the fresh document fits on its own: {}",
            &fresh_prompt[fresh_prompt.len().saturating_sub(120)..]
        );
        assert!(
            stale_prompt.contains("0 of 1 documents inlined"),
            "the notice spends budget like the document: {stale_prompt}"
        );
    }

    #[test]
    fn a_fresh_build_is_byte_identical_to_one_before_the_freshness_surface() {
        // Arrange: the same records through the plain and the
        // freshness-aware entry points, with nothing stale.
        let records = || vec![record(1, "runbooks/a", "A", "human-reviewed", "body")];

        // Act
        let plain = assemble(
            &options(Target::Generic),
            &selection(),
            &snapshot(),
            &records(),
        )
        .expect("plain");
        let aware = assemble_with_report(
            &options(Target::Generic),
            &selection(),
            &snapshot(),
            &records(),
            &report(),
        )
        .expect("aware");

        // Assert
        let paths_and_bytes = |p: &Plugin| {
            p.files
                .iter()
                .map(|f| (f.path.clone(), f.bytes.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(paths_and_bytes(&plain), paths_and_bytes(&aware));
        assert!(plain.warnings.is_empty() && plain.excluded.is_empty());
        assert!(
            !plain.files.iter().any(|f| f.path.ends_with(FRESHNESS_FILE)),
            "no report without stale concepts"
        );
    }

    #[test]
    fn the_lock_records_the_policy_closure_and_per_entry_freshness_when_engaged() {
        // Arrange: a stale pick with a closure report.
        let mut picked = stale_record(1, "runbooks/a", true);
        picked.origin.pick = true;
        let mut selection = selection();
        selection.stale_policy = crate::selection::StalePolicy::Warn;
        selection.picks = vec![crate::selection::ConceptRef {
            bundle_id: 1,
            concept_id: "runbooks/a".to_owned(),
        }];
        let mut report = report();
        report.closure = Some(crate::selection::ClosureReport {
            seeds: vec![crate::selection::ConceptRef {
                bundle_id: 1,
                concept_id: "runbooks/a".to_owned(),
            }],
            direction: crate::selection::Direction::Outbound,
            relation_types: vec!["ns:depends".to_owned()],
            hops: 2,
            required: true,
            nodes: vec![crate::selection::ClosureNode {
                bundle_id: 1,
                concept_id: "runbooks/b".to_owned(),
                hops: 1,
                via: Some(crate::selection::ConceptRef {
                    bundle_id: 1,
                    concept_id: "runbooks/a".to_owned(),
                }),
                relation_type: Some("ns:depends".to_owned()),
            }],
            unresolved: Vec::new(),
        });
        report.excluded = vec![StaleConcept {
            bundle_id: 1,
            concept_id: "runbooks/c".to_owned(),
            role: "selected".to_owned(),
            state: "stale".to_owned(),
            reasons: vec!["upstream_changed".to_owned()],
        }];

        // Act
        let plugin = assemble_with_report(
            &options(Target::Generic),
            &selection,
            &snapshot(),
            &[picked],
            &report,
        )
        .expect("builds");
        let lock: serde_json::Value = serde_json::from_str(
            std::str::from_utf8(
                &plugin
                    .files
                    .iter()
                    .find(|f| f.path == LOCK_FILE)
                    .expect("lock")
                    .bytes,
            )
            .expect("text"),
        )
        .expect("json");

        // Assert
        assert_eq!(lock["policy"]["stale"], "warn");
        assert_eq!(lock["policy"]["freshness_available"], true);
        assert_eq!(lock["closure"]["hops"], 2);
        assert_eq!(lock["closure"]["relation_types"][0], "ns:depends");
        assert_eq!(lock["closure"]["required"], true);
        assert_eq!(lock["closure"]["nodes"][0]["concept_id"], "runbooks/b");
        assert_eq!(lock["excluded"][0]["concept_id"], "runbooks/c");
        let entry = &lock["entries"][0];
        assert_eq!(entry["freshness"]["state"], "stale");
        assert_eq!(entry["freshness"]["observed_revision"], "r2");
        assert_eq!(entry["freshness"]["catalog_generation"], 42);
        assert_eq!(entry["origin"]["pick"], true);
    }

    #[test]
    fn the_manifest_records_the_stale_policy_and_closure_when_engaged() {
        // Arrange
        let selection = Selection {
            stale_policy: crate::selection::StalePolicy::Exclude,
            seeds: vec![crate::selection::ConceptRef {
                bundle_id: 3,
                concept_id: "a".to_owned(),
            }],
            relation_types: vec!["ns:depends".to_owned()],
            direction: crate::selection::Direction::Both,
            hops: Some(3),
            require_closure: true,
            ..Selection::default()
        };

        // Act
        let manifest = manifest_yaml("kit", Target::Generic, None, &selection, &[], None, true);

        // Assert
        assert!(manifest.contains("  stale: exclude\n"), "{manifest}");
        assert!(
            manifest.contains(
                "seeds: [\"3:a\"], relation_types: [\"ns:depends\"], direction: both, hops: 3, require_closure: true"
            ),
            "{manifest}"
        );
        // Unengaged manifests stay as they were.
        let plain = manifest_yaml("kit", Target::Generic, None, &selection, &[], None, false);
        assert!(!plain.contains("stale:"), "{plain}");
        assert!(!plain.contains("seeds:"), "{plain}");
    }
}
