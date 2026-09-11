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
| `mcp` | the harness's MCP server entry for `pgokf-mcp`, so the agent can query the catalog live - a server it starts, or with `mcp_url` an HTTP endpoint it calls. No secret is ever written into the tree: for a local server the connection string is referenced (Claude Code, Gemini CLI, Kimi and Hermes expand `${OKF_PG_URL}`, Cursor `${env:OKF_PG_URL}`, Codex forwards it through `env_vars`, Copilot inherits its own environment); for an endpoint the bearer token is (see [Pointing at an HTTP endpoint](#pointing-at-an-http-endpoint)) | `.mcp.json`, `.cursor/mcp.json`, `.codex/config.toml`, `.gemini/settings.json`, `.kimi/mcp.json`, `okf-hermes-mcp.yaml`, or `okf-mcp.json` for the generic shapes; each location is documented in the profile |
| `guide` | `USING-THE-CATALOG.md`: identities, trust tiers, the MCP tools, the JSON API, how to rebuild the package | beside the references (`references/`, `knowledge/`) |
| `tools` | `okf.sh`, a POSIX helper over the JSON API (`search`, `get`, `graph`, `bundles`, `health`) for harnesses without MCP; marked executable | `scripts/` in a skill, `tools/` elsewhere |

Every tree also carries `okf-workspace.yaml`, the manifest that reproduces
the selection, and `okf-workspace.lock`, which records the catalog snapshot,
a content hash per catalog file, and a hash of every file the tree holds -
the generated ones included, so what you received can be checked against
what was built. A build against an unchanged catalog is
byte-identical.

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

## One snapshot, freshness, and closure

`build_in_transaction` (what the web download and the MCP tool call) runs
the whole build - ranking, resolution, closure, source reads, freshness
reads, and the lock snapshot - under one `REPEATABLE READ` transaction, so
a tree never mixes catalog generations. The snapshot stays writable: the
audited readers (`get_concept_source()`, the package readers) append one
audit row per read inside it, and a read-only transaction would refuse
those writes. The plain [`build`] keeps the statement-per-stage form for
callers that cannot hand over a mutable client.

On a catalog with the freshness surface (pgokf 0.3.0's
`pgokf.effective_freshness`), each selected concept's effective state is
read in the same snapshot - each skill package member's state too - and
the selection's `stale_policy` decides what happens to a stale one:

- `warn` (the default) keeps it, labelled: a generated banner after the
  frontmatter of a reconstructed document, or - for a skill package or a
  stored-source document, whose bytes are materialized exactly and never
  modified - a deterministic adjacent `<name>.stale-warning.md` (`SKILL.md`
  → `SKILL.stale-warning.md`). A stale package member gets its own adjacent
  warning beside the member file, with the member's own evidence; a prompt
  bundle inlines the warning with the document's text. The tree gains a
  top-level `FRESHNESS.md` with every stale concept's state, reasons, and
  revisions, linked from the index, and the manifest and lockfile record
  the policy, each entry's state/reasons/revisions (a member entry carries
  the member's own state), and each bundle's catalog generation.
- `exclude` drops stale concepts before anything is read, and refuses the
  build - enumerating the stale ids, their roles, and the catalog's
  reasons - when an exact pick, a seed, or a required closure node is
  stale, or when nothing fresh remains. A package ships byte for byte or
  not at all, so a package with a stale member is dropped whole and the
  stale member is what the enumeration names.

`seeds` (`bundle_id:concept_id` refs) start a closure over the catalog's
typed relationships (`pgokf.current_relationships`), walked breadth-first
inside the same snapshot with caller-supplied namespaced `relation_types`,
a `direction` (`outbound`, `inbound`, `both`), and a bounded hop count
(default 2, at most 8). The builder holds no relationship vocabulary of its
own. `require_closure` turns an incomplete closure - a traversed
relationship whose target never resolved, a reached node the trust filter
kept out, or a closure node the exclude policy would drop - into a refusal
naming the offending edges or nodes. The lockfile records the closure's
seeds, filters, hop bound, reached nodes, and unresolved edges. A catalog
without these capabilities builds exactly as it always has (nothing is
warned about, and seeds are refused with a clear message), and a build
that engaged none of it is byte-identical to one from before.

A downloaded plugin cannot update itself, so
`check_plugin_freshness` (the `check_workspace_plugin_freshness` MCP tool)
compares its `okf-workspace.lock` - the pinned bundle sync hashes, catalog
generations where recorded, and the build-time freshness evidence of the
content entries - with the live catalog and answers `current`, `stale`,
`retired`, or `unknown` with per-bundle reasons. Content the catalog
already reported stale when the plugin was built keeps the artifact
`stale` until it is rebuilt (the check compares the bytes that shipped);
a bare matching hash on a catalog without live freshness state is
`unknown`, never `current`.

## Pointing at an HTTP endpoint

By default the `mcp` component configures a **local** server the harness
starts (`pgokf-mcp`, reading its connection string from the environment).
`mcp_url` points it at a `pgokf-mcp --http` endpoint instead - a hosted
agent, or a fleet of agents sharing one connection to the catalog. The
arguments to `build_workspace_plugin`:

```json
{ "target": "claude-code", "mcp_url": "https://catalog.example/mcp" }
```

That endpoint needs a bearer token on every request, and a token is a
secret, so it is treated exactly as the connection string is: **referenced,
never written**. Each harness's remote form was read from its own
documentation and is recorded with its source in the profile:

| Target | Entry | The token |
| ------ | ----- | --------- |
| `claude-code` | `.mcp.json`: `type: http`, `url`, `headers` | `Bearer ${OKF_MCP_TOKEN}`, expanded by the harness |
| `gemini-cli` | `.gemini/settings.json`: `type: http`, `url`, `headers` (`httpUrl` is deprecated) | `Bearer ${OKF_MCP_TOKEN}` - it expands variables in header values |
| `cursor` | `.cursor/mcp.json`: `url`, `headers` | `Bearer ${env:OKF_MCP_TOKEN}` |
| `codex` | `.codex/config.toml`: `url`, `bearer_token_env_var` | the harness reads `OKF_MCP_TOKEN` itself; no header is written |
| `hermes-agent` | `okf-hermes-mcp.yaml`, merged into `~/.hermes/config.yaml` | `Bearer ${OKF_MCP_TOKEN}` |
| `agent-plugin` | `mcp.json`: `type: streamable-http`, `url` | the specification forbids a credential in a package and defines no way to reference one, so the entry names the endpoint only and the client is given the token. It also requires HTTPS for anything but a loopback endpoint, which the build enforces |
| `copilot`, `kimi` | a fragment: `okf-mcp.json` | these document header values as literals, so the entry holds the placeholder `Bearer TOKEN` **and is not written into the harness's own file**: merge it into `.github/mcp.json` / `~/.kimi/mcp.json` and put the token there, out of version control |
| `agents`, `agents-md`, `generic` | `okf-mcp.json`: the shape most clients share | `Bearer ${OKF_MCP_TOKEN}` |

`mcp_url` and `mcp_command` are alternatives - an endpoint is reached, not
started. A URL carrying credentials, a query string, or a fragment is
refused: the endpoint is a path, and a token put anywhere but the header
would be written into files meant to be committed (and into every access log
between the client and the server). Over
HTTP the token's role decides which tools the agent is offered, so a
`reader` token sees the five reading tools and a `builder` token those and
the two plugin tools; the guide says so.

Use it from the web UI (**Plugins** page) or the MCP server
(`build_workspace_plugin` with `components`, `mcp_command` or `mcp_url`,
`web_url`; `list_plugin_targets`); both call the same
[`build_in_transaction`] flow.
