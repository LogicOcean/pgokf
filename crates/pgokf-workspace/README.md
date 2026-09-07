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
| `copilot` | Agent Skills package | `.github/skills/<name>/` (MCP entry in `.github/mcp.json`) |
| `agents` | Agent Skills package | `.agents/skills/<name>/` (read by several harnesses) |
| `agents-md` | Instruction file | `AGENTS.md` + `knowledge/` |
| `ollama` | Prompt bundle | `okf-prompt/Modelfile`, `system-prompt.md`, `knowledge/` |
| `generic` | Index plus files | `okf-knowledge/INDEX.md` + `concepts/` |

Every layout was read from the harness's own documentation when the adapter
was written (each profile records the source and date), and the builder
refuses a target it does not know rather than guess a layout. Each target
has a *kind* (`Shape`: `agent-plugin`, `skills`, `instruction-file`,
`prompt-bundle`, `generic`), which is what the UI asks first. An agent that
is not in the registry is built as `target: custom` with a `CustomHarness`
(a display name, the kind, and, for a skills package, the directory it
reads skills from, validated to stay inside the workspace); its layout is
the kind's base profile under that directory, and the manifest and lockfile
record the description so the tree is reproducible.

The first target, `agent-plugin`, is the portable one: a self-contained
plugin directory as the [Agent Plugins Specification 1.0.0](https://github.com/agentplugins/agent-plugins-spec)
defines it, for any client that implements that specification:

```text
<name>/
├── plugin.json            # the closed portable manifest ($schema, name, version, description, keywords)
├── skills/
│   ├── <name>/SKILL.md    # the knowledge skill (references/ hold the documents)
│   └── <package>/...      # every stored skill package, byte for byte
├── mcp.json               # component `mcp`: {"type": "stdio", "command": "pgokf-mcp",
│                          #   "args": ["--env-file", "${PLUGIN_DATA}/pgokf.env"]}
├── okf-workspace.yaml
└── okf-workspace.lock
```

`version` is `1.<yyyymmdd>.<seconds of day>+<digest>`: the ordered part is
the newest sync among the included bundles, so a rebuild after the catalog
changed compares newer, and the build metadata digests every other file and
the manifest's own fields, so it changes exactly when the plugin's content
does. The MCP entry embeds no connection string and depends on no ambient
variable (the specification forbids both): `pgokf-mcp --env-file` reads
`OKF_PG_URL` from a file you create once under the client-managed
`${PLUGIN_DATA}` directory, the only expansion the specification defines;
its `command` must be a bare program name or a `./`-relative path, as the
specification requires. A package whose name would collide with the plugin's
own directory or another package is refused rather than renamed, because a
harness that discovers skills by directory would skip it.

An Agent Skills package is `SKILL.md` (name, a description that says what
the knowledge covers and when to use it, catalog metadata) plus one file per
concept under `references/`, the concept's exact stored source when the
catalog keeps source, otherwise a document reconstructed from the indexed
fields and marked as such.

Skill packages stored in the catalog (a bundle's own `SKILL.md`
directories, ingested since pgokf 0.2.0 with their `scripts/`,
`references/`, and `assets/`) are not re-described: a selected `Skill`
concept is copied **whole and byte for byte** from the typed projections,
through the audited `get_skill()`, `get_script()`, and `get_reference()`
readers. For a native Agent Skills consumer the package lands beside the
knowledge skill under its own name (`.claude/skills/<name>/SKILL.md`,
`scripts/` executable, `references/`, `assets/`), so the harness loads it
like any skill; the other shapes put it under `knowledge/skills/<name>/`
(or `<root>/skills/<name>/`). Two packages that share a name get the
bundle id appended, and the index lists the packages under a **Skills**
heading. A script or reference selected without its package is written at
its catalog path (scripts under the skill's `scripts/`, executable); one
whose package is also selected is dropped, since the package carries it.
The lockfile records the package hash beside each package file.

Three optional components make the package more than a skills directory:

| Component | What it adds | Where |
| --------- | ------------ | ----- |
| `mcp` | the harness's MCP server entry for `pgokf-mcp`, so the agent can query the catalog live; the connection string is never written into the tree (Claude Code and Gemini CLI expand `${OKF_PG_URL}`, Cursor `${env:OKF_PG_URL}`, Codex forwards it through `env_vars`, Hermes gets a snippet to merge into its user config with a placeholder) | `.mcp.json`, `.cursor/mcp.json`, `.codex/config.toml`, `.gemini/settings.json`, `.kimi/mcp.json`, `okf-hermes-mcp.yaml`, or `okf-mcp.json` for the generic shapes; each location is documented in the profile |
| `guide` | `USING-THE-CATALOG.md`: identities, trust tiers, the MCP tools, the JSON API, how to rebuild the package | beside the references (`references/`, `knowledge/`) |
| `tools` | `okf.sh`, a POSIX helper over the JSON API (`search`, `get`, `graph`, `bundles`, `health`) for harnesses without MCP; marked executable | `scripts/` in a skill, `tools/` elsewhere | Every tree also carries `okf-workspace.yaml`,
the manifest that reproduces the selection, and `okf-workspace.lock`, which
records the catalog snapshot and a content hash per file. A build against
an unchanged catalog is byte-identical.

Selectors (`bundle_ids`, `types`, `tags`, `concept_ids`, `query`,
`verified_only`, `limit` up to 500) resolve through the reader API, so what
the session may not see is simply absent. `picks` name specific files by
identity (`bundle_id:concept_id`); they are added to whatever the other
selectors match, ordered first so the limit never cuts them, and a picked
`SKILL.md` brings its whole package while a picked script or reference is
one file. Tree paths are the catalog paths
with any backslash or control character replaced (the lockfile keeps the
original beside the file name), bundles that slug alike get their id
appended, and a build that would write two files at one path is refused.
Writing into a directory validates every path first, never follows a
symbolic link, and refuses existing files unless told to overwrite. Reading a concept's source for a
build goes through the audited `get_concept_source()` (or the package
readers above), which is what that log is for.

Use it from the web UI (**Plugins** page) or the MCP server
(`build_workspace_plugin` with `components`, `mcp_command`, `web_url`;
`list_plugin_targets`); both call the same [`build`] function.
