// SPDX-License-Identifier: AGPL-3.0-only
//! The registry of target profiles: where each agent harness looks for what
//! this crate writes, and which of the three shapes it takes.
//!
//! Every layout here was read from the client's own documentation when the
//! adapter was written (the `verified` field records the date and source);
//! the builder refuses a target it does not know rather than guess one.
//!
//! The first profile is the portable one: an Agent Plugin directory as the
//! Agent Plugins Specification 1.0.0 (<https://github.com/agentplugins/agent-plugins-spec>)
//! defines it: `plugin.json` at the root, skills under `skills/`, MCP servers
//! in `mcp.json`. Any client that implements that specification loads it
//! from a directory path; the per-harness profiles remain for clients that
//! read their own layouts.

use std::fmt;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

/// The canonical `$schema` of an Agent Plugins 1.0.0 `plugin.json`.
pub const AGENT_PLUGIN_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
/// The canonical `$schema` of an Agent Plugins 1.0.0 `mcp.json`.
pub const AGENT_PLUGIN_MCP_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json";

/// The identifier of a target the user described rather than one from the
/// registry (see [`CustomHarness`]).
pub const CUSTOM_TARGET_ID: &str = "custom";

/// One of the shapes a workspace tree can take (spec §21.2): what is being
/// built, before the question of which agent reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Shape {
    /// A self-contained Agent Plugin directory (Agent Plugins 1.0.0):
    /// `<name>/plugin.json`, `<name>/skills/<skill>/...`, `<name>/mcp.json`.
    AgentPlugin,
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

impl Shape {
    /// Every shape, in the order the UI offers them.
    #[must_use]
    pub const fn all() -> &'static [Shape] {
        &[
            Shape::AgentPlugin,
            Shape::Skills,
            Shape::InstructionFile,
            Shape::PromptBundle,
            Shape::Generic,
        ]
    }

    /// The identifier used in forms, manifests, and tool arguments.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Shape::AgentPlugin => "agent-plugin",
            Shape::Skills => "skills",
            Shape::InstructionFile => "instruction-file",
            Shape::PromptBundle => "prompt-bundle",
            Shape::Generic => "generic",
        }
    }

    /// Parse an identifier from [`Self::id`].
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        let id = id.trim();
        Self::all().iter().copied().find(|s| s.id() == id)
    }

    /// A short display name.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Shape::AgentPlugin => "Agent Plugin",
            Shape::Skills => "Skills package",
            Shape::InstructionFile => "Instruction file",
            Shape::PromptBundle => "Prompt bundle",
            Shape::Generic => "Generic files",
        }
    }

    /// One line on what the shape is, for a chooser.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Shape::AgentPlugin => {
                "A self-contained plugin directory (Agent Plugins 1.0.0: plugin.json, skills/, mcp.json) that any conformant client installs."
            }
            Shape::Skills => {
                "An Agent Skills package (SKILL.md plus references) placed in the directory the agent scans for skills."
            }
            Shape::InstructionFile => {
                "An AGENTS.md instruction file with a compact index and the full content under knowledge/."
            }
            Shape::PromptBundle => {
                "A Modelfile and system prompt with the most useful content inline, for a bare model server."
            }
            Shape::Generic => "An INDEX.md plus one file per concept, for anything else.",
        }
    }

    /// The registry profile a harness of this shape is laid out like when
    /// the harness itself is not in the registry.
    ///
    /// # Panics
    ///
    /// Never in practice: every shape has a base profile in the registry
    /// (`shapes_round_trip_their_ids_and_name_a_base_profile` keeps it so).
    #[must_use]
    pub fn base(self) -> &'static Profile<'static> {
        let target = match self {
            Shape::AgentPlugin => Target::AgentPlugin,
            Shape::Skills => Target::AgentsDir,
            Shape::InstructionFile => Target::AgentsMd,
            Shape::PromptBundle => Target::Ollama,
            Shape::Generic => Target::Generic,
        };
        Profile::of(target).expect("every shape has a base profile")
    }
}

/// A target harness the builder can write for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// A portable Agent Plugin directory for any client that implements the
    /// Agent Plugins Specification.
    AgentPlugin,
    ClaudeCode,
    Codex,
    Copilot,
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
    /// A harness the user described (a [`CustomHarness`] carried in the
    /// build options), laid out like its shape's base profile.
    Custom,
}

/// How a harness's MCP configuration refers to the catalog connection
/// without the secret being written into the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvRef {
    /// `${OKF_PG_URL}`: expanded by the harness from its environment.
    Dollar,
    /// `${env:OKF_PG_URL}`: Cursor's interpolation form.
    DollarEnv,
    /// The harness forwards named variables from its own environment
    /// (Codex `env_vars`), so no value appears in the file at all.
    Forward,
    /// The harness expands nothing: a placeholder the user replaces in a
    /// file that lives outside the repository (Hermes).
    Placeholder,
    /// Agent Plugins: the server reads its connection string from an env
    /// file under the client-managed `${PLUGIN_DATA}` directory, the only
    /// expansion the specification defines; nothing about the catalog
    /// appears in the package.
    PluginData,
    /// The harness documents no expansion form but starts a stdio server
    /// with its own environment: no `OKF_PG_URL` entry is written, and the
    /// variable is set where the harness starts (Copilot).
    Inherit,
}

/// The file format of a harness's MCP configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpFormat {
    /// `{"mcpServers": {...}}`, the shape most clients share.
    McpServersJson,
    /// Copilot's `{"mcpServers": {...}}` with typed `local` entries and a
    /// `tools` allow-list.
    CopilotJson,
    /// Codex `config.toml` with `[mcp_servers.<name>]` tables.
    CodexToml,
    /// A YAML fragment to merge under Hermes's `mcp_servers:` key.
    HermesYaml,
    /// Agent Plugins `mcp.json`: `$schema` plus typed `mcpServers` entries.
    AgentPluginJson,
}

/// Where a harness reads MCP servers from, as its documentation states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpSpec {
    /// Path of the file, relative to the workspace root.
    pub path: &'static str,
    pub format: McpFormat,
    pub env_ref: EnvRef,
    /// Whether the harness picks the file up from the workspace on its
    /// own; otherwise the guide tells the user where to merge it.
    pub auto_loaded: bool,
    pub source: &'static str,
}

/// What a target expects, as documented by its own client. Registry
/// profiles are `'static`; a [`CustomHarness`] lends one for its lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile<'a> {
    pub target: Target,
    /// The identifier used on the command line, in manifests, and in the UI.
    pub id: &'a str,
    pub label: &'a str,
    pub shape: Shape,
    /// Directory (relative to the workspace root) under which the tree is
    /// written; the skill package or the knowledge tree goes beneath it.
    pub root: &'a str,
    /// Where the harness documents this location.
    pub source: &'a str,
    /// When the layout was checked against that documentation.
    pub verified: &'a str,
    /// One-line guidance shown next to the target.
    pub notes: &'a str,
    /// The harness's MCP configuration, when it has one.
    pub mcp: Option<McpSpec>,
}

impl Profile<'static> {
    /// Every registry profile, in the order the UI lists them.
    #[must_use]
    pub const fn all() -> &'static [Profile<'static>] {
        PROFILES
    }

    /// The registry profile for an identifier such as `claude-code`, or
    /// `None` for a target this crate does not know (it never guesses a
    /// layout; a custom harness is described explicitly instead).
    #[must_use]
    pub fn by_id(id: &str) -> Option<&'static Profile<'static>> {
        PROFILES.iter().find(|p| p.id == id)
    }

    /// The registry profile of a target; `None` only for [`Target::Custom`],
    /// whose profile comes from its [`CustomHarness`].
    #[must_use]
    pub fn of(target: Target) -> Option<&'static Profile<'static>> {
        PROFILES.iter().find(|p| p.target == target)
    }

    /// The registry profiles of one shape, in registry order.
    pub fn with_shape(shape: Shape) -> impl Iterator<Item = &'static Profile<'static>> {
        PROFILES.iter().filter(move |p| p.shape == shape)
    }
}

impl Target {
    /// Parse an identifier; see [`Profile::by_id`]. `custom` parses to
    /// [`Target::Custom`], whose layout the build options must describe.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        let id = id.trim();
        if id == CUSTOM_TARGET_ID {
            return Some(Target::Custom);
        }
        Profile::by_id(id).map(|p| p.target)
    }

    /// The identifier of this target.
    #[must_use]
    pub fn id(self) -> &'static str {
        Profile::of(self).map_or(CUSTOM_TARGET_ID, |p| p.id)
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// The longest display name a custom harness may have.
const LABEL_MAX: usize = 60;
/// The longest skills directory a custom harness may name.
const SKILLS_DIR_MAX: usize = 200;

/// A harness the registry does not know, described by the user: a display
/// name, the shape it takes, and, for a skills package, the directory it
/// scans for Agent Skills packages. Its layout is that of the shape's base
/// profile with the directory swapped in; nothing about the harness is
/// guessed beyond what the user said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomHarness {
    pub label: String,
    /// The kind of tree (serialized as `kind`, the word the manifest and
    /// the tool arguments use).
    #[serde(rename = "kind")]
    pub shape: Shape,
    /// Where the harness reads skills from, relative to the workspace root
    /// (`Shape::Skills` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills_dir: Option<String>,
}

impl CustomHarness {
    /// Validate a description. The name is display text (bounded, no
    /// control characters); the skills directory is a relative path of
    /// plain segments (no `..`, no drive or backslash), required for a
    /// skills package and ignored for the other shapes.
    ///
    /// # Errors
    ///
    /// An empty or unprintable name, a missing directory for a skills
    /// package, or a directory that could escape the workspace.
    pub fn new(label: &str, shape: Shape, skills_dir: Option<&str>) -> Result<Self> {
        let label = label.trim();
        if label.is_empty() {
            return Err(anyhow!("a custom agent needs a name"));
        }
        if label.chars().count() > LABEL_MAX || label.chars().any(char::is_control) {
            return Err(anyhow!(
                "the agent name must be at most {LABEL_MAX} printable characters"
            ));
        }
        let given = skills_dir.map(str::trim).filter(|d| !d.is_empty());
        let skills_dir = match (shape, given) {
            (Shape::Skills, None) => {
                return Err(anyhow!(
                    "say where {label} reads Agent Skills packages from (a directory relative to \
                     the workspace root, for example .agents/skills)"
                ));
            }
            (Shape::Skills, Some(dir)) => Some(validated_skills_dir(dir)?),
            _ => None,
        };
        Ok(Self {
            label: label.to_owned(),
            shape,
            skills_dir,
        })
    }

    /// The profile this harness is built with: its shape's base profile,
    /// under the directory it named.
    #[must_use]
    pub fn profile(&self) -> Profile<'_> {
        let base = self.shape.base();
        Profile {
            target: Target::Custom,
            id: CUSTOM_TARGET_ID,
            label: &self.label,
            shape: self.shape,
            root: self.skills_dir.as_deref().unwrap_or(base.root),
            source: "Described by the user; laid out like the shape's base profile",
            verified: "",
            notes: match self.shape {
                Shape::Skills => {
                    "An agent you named: the skill package goes under the directory you gave, and the MCP entry is a snippet to merge into that agent's own configuration."
                }
                _ => base.notes,
            },
            mcp: base.mcp,
        }
    }
}

/// A skills directory that stays inside the workspace: relative, made of
/// plain segments, without `.`/`..`, backslashes, or drive letters.
fn validated_skills_dir(dir: &str) -> Result<String> {
    let dir = dir.trim().trim_end_matches('/');
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    let ok = !dir.is_empty()
        && dir.len() <= SKILLS_DIR_MAX
        && !dir.starts_with('/')
        && dir
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != ".." && seg.chars().all(plain));
    if ok {
        Ok(dir.to_owned())
    } else {
        Err(anyhow!(
            "the skills directory must be a relative path of plain segments (letters, digits, \
             '.', '_', '-'), for example .agents/skills"
        ))
    }
}

const PROFILES: &[Profile<'static>] = &[
    Profile {
        target: Target::AgentPlugin,
        id: "agent-plugin",
        label: "Agent Plugin (portable, Agent Plugins 1.0.0)",
        shape: Shape::AgentPlugin,
        root: "",
        source: "https://github.com/agentplugins/agent-plugins-spec (spec/1.0.0.md: plugin.json at the root, skills/<name>/SKILL.md, mcp.json)",
        verified: "2026-09-07",
        notes: "A self-contained plugin directory for any client that implements the Agent Plugins Specification; install it wherever that client loads plugins from. The MCP server reads its connection string from ${PLUGIN_DATA}/pgokf.env, which you create once.",
        mcp: Some(McpSpec {
            path: "mcp.json",
            format: McpFormat::AgentPluginJson,
            env_ref: EnvRef::PluginData,
            auto_loaded: true,
            source: "https://github.com/agentplugins/agent-plugins-spec (spec/1.0.0.md §7.2: mcp.json, stdio command as one token, ${PLUGIN_DATA} expansion in args)",
        }),
    },
    Profile {
        target: Target::ClaudeCode,
        id: "claude-code",
        label: "Claude Code",
        shape: Shape::Skills,
        root: ".claude/skills",
        source: "https://code.claude.com/docs/en/skills (project skills: .claude/skills/<name>/SKILL.md)",
        verified: "2026-09-06",
        notes: "Unzip at the repository root; Claude Code loads the skill from .claude/skills/.",
        mcp: Some(McpSpec {
            path: ".mcp.json",
            format: McpFormat::McpServersJson,
            env_ref: EnvRef::Dollar,
            auto_loaded: true,
            source: "https://code.claude.com/docs/en/mcp (project scope .mcp.json; ${VAR} expansion)",
        }),
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
        mcp: Some(McpSpec {
            path: ".codex/config.toml",
            format: McpFormat::CodexToml,
            env_ref: EnvRef::Forward,
            auto_loaded: true,
            source: "https://learn.chatgpt.com/docs/extend/mcp?surface=cli (project .codex/config.toml, [mcp_servers.<name>], env_vars)",
        }),
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
        mcp: Some(McpSpec {
            path: "okf-hermes-mcp.yaml",
            format: McpFormat::HermesYaml,
            env_ref: EnvRef::Placeholder,
            auto_loaded: false,
            source: "https://hermes-agent.nousresearch.com/docs/user-guide/features/mcp (mcp_servers in ~/.hermes/config.yaml only)",
        }),
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
        mcp: Some(McpSpec {
            path: ".kimi/mcp.json",
            format: McpFormat::McpServersJson,
            env_ref: EnvRef::Dollar,
            auto_loaded: true,
            source: "https://moonshotai.github.io/kimi-cli/en/customization/mcp.html (project .kimi/mcp.json)",
        }),
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
        mcp: Some(McpSpec {
            path: ".gemini/settings.json",
            format: McpFormat::McpServersJson,
            env_ref: EnvRef::Dollar,
            auto_loaded: true,
            source: "https://geminicli.com/docs/tools/mcp-server/ (.gemini/settings.json mcpServers; $VAR expansion)",
        }),
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
        mcp: Some(McpSpec {
            path: ".cursor/mcp.json",
            format: McpFormat::McpServersJson,
            env_ref: EnvRef::DollarEnv,
            auto_loaded: true,
            source: "https://cursor.com/docs/context/mcp (.cursor/mcp.json; ${env:NAME} interpolation)",
        }),
    },
    Profile {
        target: Target::Copilot,
        id: "copilot",
        label: "GitHub Copilot",
        shape: Shape::Skills,
        root: ".github/skills",
        source: "https://docs.github.com/en/copilot/concepts/agents/about-agent-skills (project skills: .github/skills, .claude/skills, or .agents/skills; personal: ~/.copilot/skills)",
        verified: "2026-09-07",
        notes: "Unzip at the repository root; Copilot (the coding agent, the CLI, and agent mode in VS Code and JetBrains) loads the skill from .github/skills/. It also reads .claude/skills/ and .agents/skills/.",
        mcp: Some(McpSpec {
            path: ".github/mcp.json",
            format: McpFormat::CopilotJson,
            env_ref: EnvRef::Inherit,
            auto_loaded: true,
            source: "https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/add-mcp-servers (project-level .mcp.json or .github/mcp.json; mcpServers entries with type local, command, args, env, tools; no expansion form is documented, so OKF_PG_URL is set where Copilot starts)",
        }),
    },
    Profile {
        target: Target::AgentsDir,
        id: "agents",
        label: "Generic .agents/ directory (any Agent Skills harness)",
        shape: Shape::Skills,
        root: ".agents/skills",
        source: "The cross-tool location documented by Codex, Cursor, Gemini CLI, Hermes Agent, and Kimi",
        verified: "2026-09-06",
        notes: "The cross-tool .agents/skills/ location read by Codex, Cursor, Gemini CLI, Hermes Agent, and Kimi; pick it when a repository serves more than one agent.",
        mcp: Some(McpSpec {
            path: "okf-mcp.json",
            format: McpFormat::McpServersJson,
            env_ref: EnvRef::Dollar,
            auto_loaded: false,
            source: "The mcpServers shape shared by Claude Code, Cursor, Gemini CLI, and Kimi; merge it into the harness's own file",
        }),
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
        mcp: Some(McpSpec {
            path: "okf-mcp.json",
            format: McpFormat::McpServersJson,
            env_ref: EnvRef::Dollar,
            auto_loaded: false,
            source: "The mcpServers shape shared by most clients; merge it into the harness's own file",
        }),
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
        mcp: None,
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
        mcp: Some(McpSpec {
            path: "okf-mcp.json",
            format: McpFormat::McpServersJson,
            env_ref: EnvRef::Dollar,
            auto_loaded: false,
            source: "The mcpServers shape shared by most clients; merge it into the harness's own file",
        }),
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
            if let Some(mcp) = p.mcp {
                assert!(
                    !mcp.source.is_empty() && !mcp.path.is_empty(),
                    "{} mcp",
                    p.id
                );
            }
        }
    }

    #[test]
    fn every_target_has_a_profile() {
        // Arrange
        let targets = [
            Target::AgentPlugin,
            Target::ClaudeCode,
            Target::Codex,
            Target::Copilot,
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
            assert_eq!(Profile::of(target).expect("registered").target, target);
            assert_eq!(Target::parse(target.id()), Some(target));
        }
        assert_eq!(Target::parse("custom"), Some(Target::Custom));
        assert_eq!(Target::Custom.id(), "custom");
        assert!(Profile::of(Target::Custom).is_none());
    }

    #[test]
    fn shapes_round_trip_their_ids_and_name_a_base_profile() {
        // Arrange / Act / Assert
        for shape in Shape::all() {
            assert_eq!(Shape::parse(shape.id()), Some(*shape));
            assert_eq!(shape.base().shape, *shape, "{}", shape.id());
            assert!(!shape.label().is_empty() && !shape.description().is_empty());
        }
        assert_eq!(Shape::parse(" skills "), Some(Shape::Skills));
        assert_eq!(Shape::parse("plugin"), None);
        assert!(Profile::with_shape(Shape::Skills).count() >= 7);
    }

    #[test]
    fn a_custom_harness_takes_its_shape_s_base_layout_under_its_own_directory() {
        // Arrange
        let skills = CustomHarness::new("  Acme Agent ", Shape::Skills, Some(".acme/skills/"))
            .expect("valid");
        let plugin =
            CustomHarness::new("Acme", Shape::AgentPlugin, Some("ignored")).expect("valid");

        // Act
        let skills_profile = skills.profile();
        let plugin_profile = plugin.profile();

        // Assert
        assert_eq!(skills.label, "Acme Agent");
        assert_eq!(skills_profile.root, ".acme/skills");
        assert_eq!(skills_profile.target, Target::Custom);
        assert_eq!(skills_profile.shape, Shape::Skills);
        assert_eq!(
            skills_profile.mcp.map(|m| m.path),
            Shape::Skills.base().mcp.map(|m| m.path)
        );
        assert_eq!(plugin.skills_dir, None);
        assert_eq!(plugin_profile.root, "");
        assert_eq!(plugin_profile.shape, Shape::AgentPlugin);
    }

    #[test]
    fn a_custom_harness_refuses_unsafe_names_and_directories() {
        // Arrange / Act / Assert
        assert!(CustomHarness::new("", Shape::Generic, None).is_err());
        assert!(CustomHarness::new("a\u{7}b", Shape::Generic, None).is_err());
        assert!(CustomHarness::new(&"x".repeat(61), Shape::Generic, None).is_err());
        assert!(CustomHarness::new("Acme", Shape::Skills, None).is_err());
        for bad in [
            "/abs/skills",
            "../up/skills",
            "a/./b",
            "a//b",
            "C:\\skills",
            "sk ills",
            "a/..",
        ] {
            assert!(
                CustomHarness::new("Acme", Shape::Skills, Some(bad)).is_err(),
                "{bad}"
            );
        }
        assert_eq!(
            CustomHarness::new("Acme", Shape::Skills, Some("tools/agent_skills"))
                .expect("valid")
                .skills_dir
                .as_deref(),
            Some("tools/agent_skills")
        );
    }

    #[test]
    fn unknown_targets_are_refused_rather_than_guessed() {
        // Arrange & Act & Assert
        assert_eq!(Target::parse("agent-plugin"), Some(Target::AgentPlugin));
        assert_eq!(Target::parse("claude-code"), Some(Target::ClaudeCode));
        assert_eq!(Target::parse(" codex "), Some(Target::Codex));
        assert_eq!(Target::parse("windsurf"), None);
        assert_eq!(Target::Cursor.to_string(), "cursor");
    }
}
