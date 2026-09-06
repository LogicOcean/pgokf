# pgokf-workspace

Workspace injection for the pgokf catalog (extension spec §21): turn a
selection of catalog concepts into an agent plugin and deliver it as a zip
to unpack at a workspace root, or as files written in place.

| Target | Shape | Written under |
| ------ | ----- | ------------- |
| `claude-code` | Agent Skills package | `.claude/skills/<name>/` |
| `codex` | Agent Skills package | `.agents/skills/<name>/` |
| `hermes-agent` | Agent Skills package | `.hermes/skills/<name>/` |
| `kimi` | Agent Skills package | `.kimi/skills/<name>/` |
| `gemini-cli` | Agent Skills package | `.gemini/skills/<name>/` |
| `cursor` | Agent Skills package | `.cursor/skills/<name>/` |
| `agents` | Agent Skills package | `.agents/skills/<name>/` (read by several harnesses) |
| `agents-md` | Instruction file | `AGENTS.md` + `knowledge/` |
| `ollama` | Prompt bundle | `okf-prompt/Modelfile`, `system-prompt.md`, `knowledge/` |
| `generic` | Index plus files | `okf-knowledge/INDEX.md` + `concepts/` |

Every layout was read from the harness's own documentation when the adapter
was written (each profile records the source and date), and the builder
refuses a target it does not know rather than guess a layout.

An Agent Skills package is `SKILL.md` (name, a description that says what
the knowledge covers and when to use it, catalog metadata) plus one file per
concept under `references/`, the concept's exact stored source when the
catalog keeps source, otherwise a document reconstructed from the indexed
fields and marked as such. Every tree also carries `okf-workspace.yaml`,
the manifest that reproduces the selection, and `okf-workspace.lock`, which
records the catalog snapshot and a content hash per file. A build against
an unchanged catalog is byte-identical.

Selectors (`bundle_ids`, `types`, `tags`, `concept_ids`, `query`,
`verified_only`, `limit` up to 500) resolve through the reader API, so what
the session may not see is simply absent. Tree paths are the catalog paths
with any backslash or control character replaced (the lockfile keeps the
original beside the file name), bundles that slug alike get their id
appended, and a build that would write two files at one path is refused.
Writing into a directory validates every path first, never follows a
symbolic link, and refuses existing files unless told to overwrite. Reading a concept's source for a
build goes through the audited `get_concept_source()`, which is what that
log is for.

Use it from the web UI (**Plugins** page) or the MCP server
(`build_workspace_plugin`, `list_plugin_targets`); both call the same
[`build`] function.
