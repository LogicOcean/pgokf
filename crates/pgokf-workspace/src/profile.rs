// SPDX-License-Identifier: AGPL-3.0-only
//! The registry of target profiles: where each agent harness looks for what
//! this crate writes, and which of the three shapes it takes.
//!
//! Every layout here was read from the client's own documentation when the
//! adapter was written (the `verified` field records the date and source);
//! the builder refuses a target it does not know rather than guess one.

use std::fmt;

/// One of the three shapes a workspace tree can take (spec §21.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// A directory the harness scans for Agent Skills packages
    /// (`<root>/<name>/SKILL.md` plus `references/`).
    Skills,
    /// An `AGENTS.md`-style instruction file with a compact discovery index
    /// and the full content under a `knowledge/` tree beside it.
    InstructionFile,
    /// A bounded system prompt (and a Modelfile) for a bare model server,
    /// plus the file tree.
    PromptBundle,
    /// An index plus files, for any harness without a known layout.
    Generic,
}

/// A target harness the builder can write for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    ClaudeCode,
    Codex,
    HermesAgent,
    Kimi,
    GeminiCli,
    Cursor,
    /// The cross-tool `.agents/skills/` directory read by Codex, Cursor,
    /// Gemini CLI, Hermes Agent, and Kimi alike.
    AgentsDir,
    /// An `AGENTS.md` instruction file for harnesses without a skills directory.
    AgentsMd,
    Ollama,
    Generic,
}

/// What a target expects, as documented by its own client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub target: Target,
    /// The identifier used on the command line, in manifests, and in the UI.
    pub id: &'static str,
    pub label: &'static str,
    pub shape: Shape,
    /// Directory (relative to the workspace root) under which the tree is
    /// written; the skill package or the knowledge tree goes beneath it.
    pub root: &'static str,
    /// Where the harness documents this location.
    pub source: &'static str,
    /// When the layout was checked against that documentation.
    pub verified: &'static str,
    /// One-line guidance shown next to the target.
    pub notes: &'static str,
}

impl Profile {
    /// Every profile, in the order the UI lists them.
    #[must_use]
    pub const fn all() -> &'static [Profile] {
        PROFILES
    }

    /// The profile for an identifier such as `claude-code`, or `None` for a
    /// target this crate does not know (it never guesses a layout).
    #[must_use]
    pub fn by_id(id: &str) -> Option<&'static Profile> {
        PROFILES.iter().find(|p| p.id == id)
    }

    /// The profile of a target.
    ///
    /// # Panics
    ///
    /// Never in practice: every `Target` has a profile in the registry
    /// (`every_target_has_a_profile` keeps it so).
    #[must_use]
    pub fn of(target: Target) -> &'static Profile {
        PROFILES
            .iter()
            .find(|p| p.target == target)
            .expect("every target has a profile")
    }
}

impl Target {
    /// Parse an identifier; see [`Profile::by_id`].
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        Profile::by_id(id.trim()).map(|p| p.target)
    }

    /// The identifier of this target.
    #[must_use]
    pub fn id(self) -> &'static str {
        Profile::of(self).id
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

const PROFILES: &[Profile] = &[
    Profile {
        target: Target::ClaudeCode,
        id: "claude-code",
        label: "Claude Code",
        shape: Shape::Skills,
        root: ".claude/skills",
        source: "https://code.claude.com/docs/en/skills (project skills: .claude/skills/<name>/SKILL.md)",
        verified: "2026-09-06",
        notes: "Unzip at the repository root; Claude Code loads the skill from .claude/skills/.",
    },
    Profile {
        target: Target::Codex,
        id: "codex",
        label: "Codex",
        shape: Shape::Skills,
        root: ".agents/skills",
        source: "https://learn.chatgpt.com/docs/build-skills (repository scope: .agents/skills)",
        verified: "2026-09-06",
        notes: "Unzip at the repository root; Codex reads .agents/skills/ in the repository and its parents.",
    },
    Profile {
        target: Target::HermesAgent,
        id: "hermes-agent",
        label: "Hermes Agent",
        shape: Shape::Skills,
        root: ".hermes/skills",
        source: "https://hermes-agent.nousresearch.com/docs/user-guide/features/skills (project: .hermes/skills, also .agents/skills)",
        verified: "2026-09-06",
        notes: "Unzip at the project root; move the package to ~/.hermes/skills/ for a user-wide skill.",
    },
    Profile {
        target: Target::Kimi,
        id: "kimi",
        label: "Kimi Code CLI",
        shape: Shape::Skills,
        root: ".kimi/skills",
        source: "https://moonshotai.github.io/kimi-cli/en/customization/skills.html (project: .kimi/skills, also .agents/skills)",
        verified: "2026-09-06",
        notes: "Unzip at the project root; Kimi also reads .claude/skills/ and .agents/skills/.",
    },
    Profile {
        target: Target::GeminiCli,
        id: "gemini-cli",
        label: "Gemini CLI",
        shape: Shape::Skills,
        root: ".gemini/skills",
        source: "https://geminicli.com/docs/cli/skills/ (workspace: .gemini/skills or .agents/skills)",
        verified: "2026-09-06",
        notes: "Unzip at the workspace root; Gemini CLI also reads .agents/skills/.",
    },
    Profile {
        target: Target::Cursor,
        id: "cursor",
        label: "Cursor",
        shape: Shape::Skills,
        root: ".cursor/skills",
        source: "https://cursor.com/docs/context/skills (project: .cursor/skills or .agents/skills)",
        verified: "2026-09-06",
        notes: "Unzip at the project root; Cursor also reads .agents/skills/ and, for compatibility, .claude/skills/.",
    },
    Profile {
        target: Target::AgentsDir,
        id: "agents",
        label: "Any Agent Skills harness (.agents/skills)",
        shape: Shape::Skills,
        root: ".agents/skills",
        source: "The cross-tool location documented by Codex, Cursor, Gemini CLI, Hermes Agent, and Kimi",
        verified: "2026-09-06",
        notes: "One package several harnesses read; pick this when a repository serves more than one agent.",
    },
    Profile {
        target: Target::AgentsMd,
        id: "agents-md",
        label: "AGENTS.md instruction file",
        shape: Shape::InstructionFile,
        root: "",
        source: "https://agents.md (the AGENTS.md convention) with the full content under knowledge/",
        verified: "2026-09-06",
        notes: "For a harness that reads an instruction file and has no skills directory; merge the index into an existing AGENTS.md by hand.",
    },
    Profile {
        target: Target::Ollama,
        id: "ollama",
        label: "Ollama / bare model (prompt bundle)",
        shape: Shape::PromptBundle,
        root: "okf-prompt",
        source: "https://docs.ollama.com/modelfile (Modelfile FROM/SYSTEM)",
        verified: "2026-09-06",
        notes: "A Modelfile and system prompt with the most useful content inline, plus the full files; `ollama create <name> -f okf-prompt/Modelfile`.",
    },
    Profile {
        target: Target::Generic,
        id: "generic",
        label: "Generic (index plus files)",
        shape: Shape::Generic,
        root: "okf-knowledge",
        source: "This crate's own layout: INDEX.md and one file per concept",
        verified: "2026-09-06",
        notes: "For anything else; point the tool at okf-knowledge/INDEX.md.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_has_a_unique_id_and_a_documented_source() {
        // Arrange
        let profiles = Profile::all();

        // Act
        let mut ids: Vec<&str> = profiles.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        ids.dedup();

        // Assert
        assert_eq!(ids.len(), profiles.len(), "duplicate profile ids");
        for p in profiles {
            assert!(!p.source.is_empty(), "{} has no source", p.id);
            assert!(p.verified.starts_with("2026-"), "{} is unverified", p.id);
            if p.shape == Shape::Skills {
                assert!(p.root.ends_with("skills"), "{} root {}", p.id, p.root);
            }
        }
    }

    #[test]
    fn every_target_has_a_profile() {
        // Arrange
        let targets = [
            Target::ClaudeCode,
            Target::Codex,
            Target::HermesAgent,
            Target::Kimi,
            Target::GeminiCli,
            Target::Cursor,
            Target::AgentsDir,
            Target::AgentsMd,
            Target::Ollama,
            Target::Generic,
        ];

        // Act & Assert
        for target in targets {
            assert_eq!(Profile::of(target).target, target);
            assert_eq!(Target::parse(target.id()), Some(target));
        }
    }

    #[test]
    fn unknown_targets_are_refused_rather_than_guessed() {
        // Arrange & Act & Assert
        assert_eq!(Target::parse("claude-code"), Some(Target::ClaudeCode));
        assert_eq!(Target::parse(" codex "), Some(Target::Codex));
        assert_eq!(Target::parse("windsurf"), None);
        assert_eq!(Target::Cursor.to_string(), "cursor");
    }
}
