// SPDX-License-Identifier: AGPL-3.0-only
//! Agent Skills `SKILL.md` manifests as virtual OKF concepts.
//!
//! A skill is authored in the portable Agent Skills package format
//! (<https://agentskills.io/>): a directory with a `SKILL.md` whose YAML
//! frontmatter carries `name` and `description` (plus optional `license`,
//! `compatibility`, `metadata`, and `allowed-tools`), followed by the skill's
//! Markdown instructions. That file is never rewritten with OKF-only fields;
//! instead [`parse_skill_manifest`] projects it in memory onto the same
//! [`ParsedConcept`] an OKF document produces:
//!
//! | Agent Skills field | virtual OKF value |
//! | --- | --- |
//! | `name` | `title` |
//! | `description` | `description` |
//! | `tags` (an extension the standard lets through as an unknown field) | `tags` |
//! | `license` | `metadata.license` |
//! | complete original frontmatter | `metadata.agent_skill` |
//!
//! The concept `type` is [`SKILL_TYPE`], the `id` is path-derived exactly as
//! for a document (`skills/deploy/SKILL.md` → `skills/deploy/SKILL`), and the
//! body is the Markdown after the frontmatter. Links to the package's own
//! resources (`scripts/`, `references/`, `assets/`) resolve to those files
//! with their full path as the target concept ID, which is how the catalog
//! identifies a package resource.
//!
//! The standard's structural rules (name shape, description length, name
//! equal to the directory) are not parse errors: a portable skill that bends
//! them still indexes. [`validate_skill`] reports them as
//! [`SkillDiagnostic`]s for the caller to surface.

use std::path::Path;

use serde_json::{Map, Value};

use crate::model::ParsedConcept;
use crate::{Error, ParserLimits, Result, frontmatter, links, markdown, normalize};

/// The OKF concept type of a skill manifest.
pub const SKILL_TYPE: &str = "Skill";

/// The metadata key under which the complete original frontmatter is kept.
pub const AGENT_SKILL_KEY: &str = "agent_skill";

/// The longest skill name the Agent Skills standard allows.
pub const NAME_MAX_LEN: usize = 64;

/// The longest description the Agent Skills standard allows.
pub const DESCRIPTION_MAX_LEN: usize = 1024;

/// The package-relative directories a skill may link to as resources.
const RESOURCE_DIRECTORIES: [&str; 3] = ["scripts", "references", "assets"];

/// A structural finding against the Agent Skills standard.
///
/// Reported by [`validate_skill`]; never a parse failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillDiagnostic {
    /// `name` is not lowercase letters, digits, and single hyphens, or is
    /// longer than [`NAME_MAX_LEN`].
    NameInvalid(String),
    /// `name` differs from the basename of the package directory.
    NameDirectoryMismatch {
        /// The declared name.
        name: String,
        /// The package directory's basename.
        directory: String,
    },
    /// `description` is longer than [`DESCRIPTION_MAX_LEN`].
    DescriptionTooLong(usize),
}

impl SkillDiagnostic {
    /// The stable diagnostic code (the specification's vocabulary).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NameInvalid(_) => "skill_name_invalid",
            Self::NameDirectoryMismatch { .. } => "skill_name_directory_mismatch",
            Self::DescriptionTooLong(_) => "skill_description_invalid",
        }
    }
}

/// How much of a declared value a diagnostic quotes back. The value comes
/// from a manifest's frontmatter, which is bounded only by
/// `max_frontmatter_bytes`, and the diagnostic reaches the client and the
/// server log once per sync per manifest.
const QUOTED_MAX: usize = 80;

/// A caller-supplied value, bounded, for a message that reaches a log.
fn quoted(value: &str) -> String {
    let mut shown: String = value.chars().take(QUOTED_MAX).collect();
    if value.chars().nth(QUOTED_MAX).is_some() {
        shown.push('\u{2026}');
    }
    format!("{shown:?}")
}

impl std::fmt::Display for SkillDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NameInvalid(name) => write!(
                f,
                "skill name {} must be 1-{NAME_MAX_LEN} lowercase letters, digits, and single \
                 hyphens (no leading or trailing hyphen)",
                quoted(name)
            ),
            Self::NameDirectoryMismatch { name, directory } => write!(
                f,
                "skill name {} differs from its package directory {}",
                quoted(name),
                quoted(directory)
            ),
            Self::DescriptionTooLong(length) => write!(
                f,
                "skill description is {length} characters, above the {DESCRIPTION_MAX_LEN} limit"
            ),
        }
    }
}

/// Parse one UTF-8 `SKILL.md` into a virtual `type: Skill` concept.
///
/// `relative_path` must name the manifest itself; its directory is the
/// package root. Every original frontmatter key, known or not, survives under
/// `metadata.agent_skill` with its original scalar/array/object value.
///
/// # Errors
/// Returns an error when limits are exceeded, the path is unsafe, the input
/// is not UTF-8, the frontmatter is missing or invalid YAML, the frontmatter
/// is not a mapping, or `name`/`description` are absent or not non-empty
/// strings ([`Error::InvalidSkillManifest`]).
pub fn parse_skill_manifest(
    source: &[u8],
    relative_path: impl AsRef<Path>,
    limits: ParserLimits,
) -> Result<ParsedConcept> {
    let path = normalize::normalize_path(relative_path.as_ref())?;
    if source.len() > limits.max_file_bytes {
        return Err(Error::FileTooLarge {
            path,
            actual: source.len(),
            limit: limits.max_file_bytes,
        });
    }
    let source = std::str::from_utf8(source).map_err(|source| Error::InvalidUtf8 {
        path: path.clone(),
        source,
    })?;
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let (yaml, body) = frontmatter::split(source, &path, limits.max_frontmatter_bytes)?;
    let frontmatter = parse_frontmatter(yaml, &path)?;

    let name = required_string(&frontmatter, "name", &path)?;
    let description = required_string(&frontmatter, "description", &path)?;
    let tags = frontmatter
        .get("tags")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    let mut metadata = Map::new();
    if let Some(license) = frontmatter.get("license").and_then(Value::as_str) {
        metadata.insert("license".to_owned(), Value::String(license.to_owned()));
    }
    metadata.insert(AGENT_SKILL_KEY.to_owned(), Value::Object(frontmatter));

    let package_root = normalize::parent_directory(&path).to_owned();
    let mut extracted = links::extract(body, &path);
    for link in &mut extracted {
        resolve_resource_link(link, &path, &package_root);
    }
    let body_text = markdown::plain_text(body);
    let id = normalize::concept_id(&path);

    Ok(ParsedConcept {
        id,
        declared_id: None,
        path,
        r#type: SKILL_TYPE.to_owned(),
        title: name,
        description: Some(description),
        tags,
        resource: None,
        body_text,
        links: extracted,
        metadata,
    })
}

/// Check a parsed manifest against the Agent Skills structural rules.
///
/// `package_directory` is the bundle-relative package root (`""` for a
/// bundle that is itself one package, which has no basename to compare).
#[must_use]
pub fn validate_skill(concept: &ParsedConcept, package_directory: &str) -> Vec<SkillDiagnostic> {
    let mut findings = Vec::new();
    let name = concept.title.as_str();
    if !is_valid_name(name) {
        findings.push(SkillDiagnostic::NameInvalid(name.to_owned()));
    }
    let basename = package_directory
        .rsplit_once('/')
        .map_or(package_directory, |(_, base)| base);
    if !basename.is_empty() && basename != name {
        findings.push(SkillDiagnostic::NameDirectoryMismatch {
            name: name.to_owned(),
            directory: basename.to_owned(),
        });
    }
    let description_len = concept
        .description
        .as_deref()
        .map_or(0, |d| d.chars().count());
    if description_len > DESCRIPTION_MAX_LEN {
        findings.push(SkillDiagnostic::DescriptionTooLong(description_len));
    }
    findings
}

/// Whether a name satisfies the standard: 1-64 lowercase ASCII letters,
/// digits, and hyphens, with no leading, trailing, or doubled hyphen.
#[must_use]
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX_LEN
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Deserialize the YAML block as a JSON object, preserving every key.
fn parse_frontmatter(yaml: &str, path: &str) -> Result<Map<String, Value>> {
    let value: serde_yaml::Value =
        serde_yaml::from_str(yaml).map_err(|source| Error::InvalidFrontmatter {
            path: path.to_owned(),
            source,
        })?;
    let json = serde_json::to_value(value).map_err(|source| Error::InvalidMetadata {
        path: path.to_owned(),
        source,
    })?;
    match json {
        Value::Object(map) => Ok(map),
        _ => Err(Error::InvalidSkillManifest {
            path: path.to_owned(),
            reason: "frontmatter must be a YAML mapping with name and description".to_owned(),
        }),
    }
}

/// A required, non-blank string field of the manifest.
fn required_string(frontmatter: &Map<String, Value>, key: &str, path: &str) -> Result<String> {
    match frontmatter.get(key) {
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text.clone()),
        Some(Value::String(_)) => Err(Error::InvalidSkillManifest {
            path: path.to_owned(),
            reason: format!("`{key}` must not be empty"),
        }),
        Some(_) => Err(Error::InvalidSkillManifest {
            path: path.to_owned(),
            reason: format!("`{key}` must be a string"),
        }),
        None => Err(Error::InvalidSkillManifest {
            path: path.to_owned(),
            reason: format!("`{key}` is required"),
        }),
    }
}

/// Point a link at a package resource when its destination is one.
///
/// The ordinary extractor only resolves Markdown destinations; a link to
/// `scripts/check.sh` or `references/guide.md` from `SKILL.md` names a
/// resource whose concept ID is its full path (a Markdown reference included,
/// because resources are identified by path, not by document stem).
fn resolve_resource_link(link: &mut links::Link, source_path: &str, package_root: &str) {
    if link.is_external {
        return;
    }
    let Some(resolved) = normalize::resolve_link_path(&link.target, source_path) else {
        return;
    };
    if is_package_resource(&resolved, package_root) {
        link.target_id = Some(resolved.clone());
        link.target_path = Some(resolved);
    }
}

/// Whether a normalized bundle-relative path lies below one of the package's
/// resource directories.
fn is_package_resource(path: &str, package_root: &str) -> bool {
    let relative = if package_root.is_empty() {
        Some(path)
    } else {
        path.strip_prefix(package_root)
            .and_then(|rest| rest.strip_prefix('/'))
    };
    relative
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(directory, file)| {
            RESOURCE_DIRECTORIES.contains(&directory) && !file.is_empty()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"---
name: postgres-failover
description: Diagnose and recover a PostgreSQL primary failure safely.
license: Apache-2.0
compatibility: Requires PostgreSQL 15+ and a POSIX shell.
metadata:
  supported_clients: claude-code, codex
  portable: "true"
allowed-tools: [Bash, Read]
tags: [postgresql, operations]
---

# When to Use

Run [the check](scripts/check.sh) and read [the guide](references/guide.md).
See also [the diagram](./assets/topology.png), [a sibling doc](../notes.md),
and [upstream](https://example.invalid/docs).
"#;

    fn parse(source: &str, path: &str) -> ParsedConcept {
        parse_skill_manifest(source.as_bytes(), path, ParserLimits::default())
            .expect("manifest parses")
    }

    #[test]
    fn parse_projects_name_and_description_onto_the_concept() {
        // Arrange / Act
        let concept = parse(MANIFEST, "skills/postgres-failover/SKILL.md");

        // Assert
        assert_eq!(concept.id, "skills/postgres-failover/SKILL");
        assert_eq!(concept.path, "skills/postgres-failover/SKILL.md");
        assert_eq!(concept.r#type, SKILL_TYPE);
        assert_eq!(concept.title, "postgres-failover");
        assert_eq!(
            concept.description.as_deref(),
            Some("Diagnose and recover a PostgreSQL primary failure safely.")
        );
        assert_eq!(concept.tags, ["postgresql", "operations"]);
        assert!(concept.body_text.starts_with("When to Use"));
        assert!(concept.declared_id.is_none());
        assert!(concept.resource.is_none());
    }

    #[test]
    fn parse_preserves_the_complete_frontmatter_under_agent_skill() {
        // Arrange / Act
        let concept = parse(MANIFEST, "skills/postgres-failover/SKILL.md");

        // Assert
        let skill = concept.metadata[AGENT_SKILL_KEY]
            .as_object()
            .expect("agent_skill is an object");
        assert_eq!(skill["name"], "postgres-failover");
        assert_eq!(skill["license"], "Apache-2.0");
        assert_eq!(
            skill["compatibility"],
            "Requires PostgreSQL 15+ and a POSIX shell."
        );
        assert_eq!(skill["metadata"]["portable"], "true");
        assert_eq!(skill["allowed-tools"], serde_json::json!(["Bash", "Read"]));
        assert_eq!(
            skill["tags"],
            serde_json::json!(["postgresql", "operations"])
        );
        assert_eq!(concept.metadata["license"], "Apache-2.0");
        assert_eq!(concept.metadata.len(), 2);
    }

    #[test]
    fn parse_resolves_links_to_package_resources_by_full_path() {
        // Arrange / Act
        let concept = parse(MANIFEST, "skills/postgres-failover/SKILL.md");

        // Assert
        let targets: Vec<(Option<&str>, Option<&str>)> = concept
            .links
            .iter()
            .map(|l| (l.target_path.as_deref(), l.target_id.as_deref()))
            .collect();
        assert_eq!(
            targets,
            [
                (
                    Some("skills/postgres-failover/scripts/check.sh"),
                    Some("skills/postgres-failover/scripts/check.sh")
                ),
                (
                    Some("skills/postgres-failover/references/guide.md"),
                    Some("skills/postgres-failover/references/guide.md")
                ),
                (
                    Some("skills/postgres-failover/assets/topology.png"),
                    Some("skills/postgres-failover/assets/topology.png")
                ),
                (Some("skills/notes.md"), Some("skills/notes")),
                (None, None),
            ]
        );
        assert!(concept.links[4].is_external);
    }

    #[test]
    fn parse_resolves_root_package_resources() {
        // Arrange: the bundle root is itself the package.
        let source = "---\nname: root\ndescription: d\n---\n[x](scripts/x.sh)\n";

        // Act
        let concept = parse(source, "SKILL.md");

        // Assert
        assert_eq!(concept.id, "SKILL");
        assert_eq!(concept.links[0].target_id.as_deref(), Some("scripts/x.sh"));
    }

    #[test]
    fn parse_rejects_a_missing_name() {
        // Arrange
        let source = "---\ndescription: d\n---\nbody\n";

        // Act
        let error = parse_skill_manifest(source.as_bytes(), "a/SKILL.md", ParserLimits::default())
            .expect_err("missing name is rejected");

        // Assert
        assert!(
            matches!(error, Error::InvalidSkillManifest { ref reason, .. } if reason == "`name` is required")
        );
        assert_eq!(error.path(), "a/SKILL.md");
    }

    #[test]
    fn parse_rejects_a_blank_description_and_a_non_string_name() {
        // Arrange
        let blank = "---\nname: a\ndescription: \"  \"\n---\n";
        let numeric = "---\nname: 7\ndescription: d\n---\n";

        // Act
        let blank_error =
            parse_skill_manifest(blank.as_bytes(), "a/SKILL.md", ParserLimits::default())
                .expect_err("blank description is rejected");
        let numeric_error =
            parse_skill_manifest(numeric.as_bytes(), "a/SKILL.md", ParserLimits::default())
                .expect_err("numeric name is rejected");

        // Assert
        assert!(
            blank_error
                .to_string()
                .contains("`description` must not be empty")
        );
        assert!(
            numeric_error
                .to_string()
                .contains("`name` must be a string")
        );
    }

    #[test]
    fn parse_rejects_a_non_mapping_frontmatter() {
        // Arrange
        let source = "---\n- just\n- a list\n---\n";

        // Act
        let error = parse_skill_manifest(source.as_bytes(), "a/SKILL.md", ParserLimits::default())
            .expect_err("a list is not a manifest");

        // Assert
        assert!(matches!(error, Error::InvalidSkillManifest { .. }));
    }

    #[test]
    fn parse_requires_frontmatter_and_honors_limits() {
        // Arrange
        let bare = "# No frontmatter\n";
        let limits = ParserLimits {
            max_file_bytes: 4,
            ..ParserLimits::default()
        };

        // Act
        let missing = parse_skill_manifest(bare.as_bytes(), "a/SKILL.md", ParserLimits::default())
            .expect_err("frontmatter is required");
        let too_large = parse_skill_manifest(MANIFEST.as_bytes(), "a/SKILL.md", limits)
            .expect_err("limit applies");

        // Assert
        assert!(matches!(missing, Error::MissingFrontmatter { .. }));
        assert!(matches!(too_large, Error::FileTooLarge { .. }));
    }

    #[test]
    fn validate_reports_name_shape_directory_mismatch_and_description_length() {
        // Arrange
        let mut concept = parse(MANIFEST, "skills/postgres-failover/SKILL.md");
        assert!(validate_skill(&concept, "skills/postgres-failover").is_empty());
        concept.title = "Bad_Name".to_owned();
        concept.description = Some("x".repeat(DESCRIPTION_MAX_LEN + 1));

        // Act
        let findings = validate_skill(&concept, "skills/postgres-failover");

        // Assert
        let codes: Vec<&str> = findings.iter().map(SkillDiagnostic::code).collect();
        assert_eq!(
            codes,
            [
                "skill_name_invalid",
                "skill_name_directory_mismatch",
                "skill_description_invalid"
            ]
        );
    }

    #[test]
    fn validate_skips_the_directory_rule_for_a_root_package() {
        // Arrange
        let concept = parse("---\nname: anything\ndescription: d\n---\n", "SKILL.md");

        // Act
        let findings = validate_skill(&concept, "");

        // Assert
        assert!(findings.is_empty());
    }

    #[test]
    fn is_valid_name_applies_the_standard_rules() {
        // Arrange / Act / Assert
        assert!(is_valid_name("a"));
        assert!(is_valid_name("postgres-failover-2"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("-lead"));
        assert!(!is_valid_name("trail-"));
        assert!(!is_valid_name("dou--ble"));
        assert!(!is_valid_name("Upper"));
        assert!(!is_valid_name("under_score"));
        assert!(!is_valid_name(&"a".repeat(NAME_MAX_LEN + 1)));
    }
}
