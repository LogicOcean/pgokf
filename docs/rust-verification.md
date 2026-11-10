# Rust verification

Repository verification lives in the unpublished `pgokf-verifier` workspace
crate. Run the portable tests through the canonical workspace suite or directly:

```sh
scripts/test-workspace.sh
cargo test --locked -p pgokf-verifier
cargo run --locked -p pgokf-verifier -- workflow-check .
cargo run --locked -p pgokf-verifier -- source-guard .
```

Live PostgreSQL, Docker and Homebrew gates require their real tools. A missing
prerequisite is a failure, not a passing skipped scenario. Portable tests do not
establish live provider or package parity.

## Source and execution review

The source guard requires a valid committed `HEAD`, verifies that the default Git
index matches that commit, and compares checkout type and executable mode with
both the index and committed tree. It inventories tracked files plus untracked
files not excluded by `.gitignore` definitions read from committed `HEAD` blobs.
A modified or untracked `.gitignore` is source input, never inventory policy, so
it cannot hide itself or neighboring files. Repository-local Git settings that
change path discovery are overridden. The verifier pins Git from an absolute
executable selected from the build environment (or the build-only
`PGOKF_BUILD_GIT` override), so runtime `PATH` cannot substitute a wrapper while
non-FHS build layouts remain supported.

The inventory includes tracked files in ignored directories. It rejects source symlinks,
owned Python files and shebangs, verification interpreter invocations,
and verification package dependencies. YAML is parsed structurally with duplicate
keys rejected and merge keys resolved. The only third-party documentation
exceptions are the exact pinned setup step and exact MkDocs Material install
step in the documentation workflow; additional fields invalidate the exemption.

`source-review.json` binds every owned source input except that manifest itself.
Each file entry records `sha256` and an `executable` boolean. This includes extensionless wrappers and data interpreted by wrappers, rather
than guessing which filenames are executable. Its sorted SHA256 and executable-permission inventory must
match the independently reviewed inventory embedded in the verifier at build
time. Editing or rehashing an on-disk manifest cannot authorize new execution
against that trusted verifier. Missing, additional, changed and stale inputs fail.

A source change requires reviewing the changed commands and their transitive
inputs, updating the manifest as part of the reviewed change, and rebuilding the
verifier. Do not automatically refresh the inventory to silence a failure. The
compiled verifier and its embedded review are the trust boundary: rebuilding
with unreviewed authorization data is not a security review. This finite reviewed
program inventory is not a sandbox or a proof of arbitrary shell semantics.

Workflow policy additionally compares parsed execution contexts with
`tests/workflow-contexts.json`, requires the strict absolute system shell, and
checks support matrices, history ownership and publication dependencies. Dynamic
command construction, local actions, aliases and environment-file indirection
cannot escape a changed execution context.

## Scratch isolation

Native parity, reconciliation mutations and installed-package probes create
unique owned directories beneath the platform temporary directory (`TMPDIR` on
Unix). The root must exist, be absolute and have no traversal or symlink leaf.
Creation failure never falls back to another root. Existing process ownership,
cleanup receipts and retention on unproven process shutdown remain authoritative.
Installed-package smoke and reconciliation mutations allocate validated scratch
before creating caller evidence. A refused root leaves caller outputs absent and
pre-existing evidence unchanged, including paths beneath a rejected alias.
Native socket directories use compact names while retaining exclusive random
creation; durable parity receipts use separately retained owned directories.
PostgreSQL's platform socket pathname limit still applies to native probes.

Native readiness probes share the existing one-minute startup deadline and
supervise the owned server while `psql` runs. Both server pipes are drained
during probes; an observed server exit interrupts and reaps the dependent
probe. Output remains capped, with at most 4 KiB per stream in diagnostics.
The retained `startup.json` records the deadline, each attempt, elapsed time,
socket and data paths, child state, final SQL outcome and bounded output tails.
Startup failures also record the cleanup result. Reaping the owned foreground
server proves shutdown even when PostgreSQL exits before becoming ready.

Service intent, start, readiness and stop receipts share the same identity,
including campaign and case IDs. The reconciliation mutation runner assigns a
fresh campaign ID and binds each case to its service receipts. Standalone
callers can set `PGOKF_VERIFICATION_CAMPAIGN` and `PGOKF_VERIFICATION_CASE`;
otherwise IDs are generated. These two diagnostic labels are retained, but the
inherited environment is not serialized.

Portable Rust fault controls cover child exit, output pressure, persistent
refusal and live startup near the deadline. Opt-in native regressions exercise
real PostgreSQL log pressure and a fatal startup configuration in owned scratch:

```sh
PGOKF_NATIVE_BINDIR="$(pg_config --bindir)" cargo test --locked -p pgokf-verifier \
  --test startup_supervision native_postgres -- --ignored
```

The native pressure control uses `RAISE LOG` without creating product data.
Native boundary controls also exercise a real server starting just before the
original deadline, a blocked readiness probe that must not extend that deadline,
and an abandoned Unix socket that PostgreSQL must replace and clean up. Run all
startup controls serially and retain their printed service receipts:

```sh
PGOKF_NATIVE_BINDIR="$(pg_config --bindir)" cargo test --locked -p pgokf-verifier \
  --test startup_supervision --test startup_native_boundaries \
  -- --include-ignored --test-threads=1 --nocapture
```

Portable controls additionally require draining final shutdown diagnostics and
bounding a blocked probe by the remaining startup budget. These focused tests do
not establish native product parity or explain earlier failures whose process
output was not retained.

The Homebrew policy gate copies the formula into owned scratch and passes its
absolute path to the real `Formulary.factory`. The subprocess uses Homebrew's
documented developer mode for local formula paths; an explicit operator ban on
loading paths still fails closed. All four environment-composition predicates
remain required. The policy gate creates no live tap. The separate installation
and packaging campaign retains its existing tap and installation behavior.

On macOS, the Rust `environment_portability` integration test compares the live
tap inventory byte-for-byte and denies writes to that namespace while invoking
the actual Formula API. It also records the loaded formula path and checks that
owned scratch is removed. Process-isolated package failure controls verify
`TMPDIR` placement, symlink refusal and durable cleanup without mutating the test
runner's environment.

The compiled semantic controls for these boundaries are in
`crates/pgokf-verifier/fixtures/environment-mutations.json` and run through
`mutation-campaign` against committed source, like the campaigns below.
Both evidence-ordering substitutions must compile and fail the CLI controls'
no-output-before-validation assertion; setup failures do not count as kills.

## Mutation campaigns

On macOS with Homebrew, run the required workflow attack families and actual
formula environment substitutions:

```sh
cargo run --locked -p pgokf-verifier -- policy-campaign . /tmp/policy-evidence
```

The output directory must be new. The campaign runs the Rust workflow mutation
suite and establishes the actual Homebrew formula baseline before testing three
formula substitutions. It requires each intended Rust validation diagnostic, preserves
bounded logs, and removes only its owned temporary formula directory. Formula
controls are distinct from compiled Rust implementation mutants.

The reusable implementation campaign runs an exact Rust test against an isolated
clone of committed `HEAD`, compiling each changed implementation before testing:

```sh
cargo run --locked -p pgokf-verifier -- mutation-campaign . campaign.json /tmp/mutation-evidence
```

The manifest is duplicate-key-safe JSON with this structure:

```json
{
  "mutations": [
    {
      "name": "deadline-refusal",
      "path": "crates/pgokf-verifier/src/process.rs",
      "before": "return Err(\"timeout exceeded\".into());",
      "after": "let _ = killpg(group, Signal::SIGKILL); break owned.child.wait().map_err(|e| e.to_string())?;",
      "test": "process::tests::timeout_and_both_output_caps_refuse",
      "target": "lib",
      "failure_contains": "called `Result::unwrap_err()`"
    }
  ]
}
```

Use `{"integration":"source_hygiene"}` for an integration-test target. Each
replacement must occur exactly once in implementation source, outside its test
module. The exact selected test must pass before substitution and after
restoration. A kill requires a successful mutant compile, exactly one failed
selected test, and the intended assertion text. Compile errors, timeouts, missing
tests, unrelated failures and survivors earn no kill credit and fail the campaign.
The source commit, campaign, baseline/build/test/restoration logs, results and
owned-directory cleanup receipt are retained outside the source tree.

Set an external `CARGO_TARGET_DIR` to reuse a verifier build cache. Do not share
that target concurrently with another source checkout or mutation campaign.
The runner uses at most two build jobs, honors `CARGO_BUILD_JOBS=1`, and disables
incremental compilation.

Startup supervision implementation recipes are maintained in
`crates/pgokf-verifier/fixtures/startup-mutations.json` and use the same
`mutation-campaign` command. Their exact assertions cover supervision, draining,
identity, diagnostics, cleanup, receipt publication and the original deadline.
Recipes preserve references to otherwise removed bindings so warnings-denied
compilation reaches the semantic assertion; compiler rejection earns no credit.

## Verification surfaces

The single crate exposes these policies and drivers; use `--help` for the exact
arguments supported by the current checkout.

| Surface | Rust entry point |
|---|---|
| SQL lexical boundaries and install equivalence | `sql-statements`, `compare-install-sql` |
| Runtime indexes, tier roles and catalog metadata | `catalog-validate`, `catalog-compare`, `catalog-accept` |
| Isolated fresh/upgrade and reconciliation probes | `parity`, `reconciliation-mutations` |
| Source compatibility and release identity | `compatibility-source`, `release-check`, `release-resolve` |
| Immutable release history and checkout shapes | `history-check`, `checkout-shapes` |
| Workflow support, commands and publication prerequisites | `workflow-check` and workflow integration tests |
| Homebrew environment and source mechanics | `homebrew-policy`, `homebrew-packaging` |
| Compatibility images and Docker smoke | `compatibility-image`, `image-smoke`, `companions-smoke` |
| Durable resource ownership | `owned-create`, `owned-cleanup`, `ownership-controls` |
| Deterministic benchmark corpus and minimum dataset | `corpus`, `corpus-field`, `corpus-check` |
| Native MCP surface, wire, role and tenant probes | `mcp-native` |
| Legacy MCP wire fixtures | `mcp-golden` |
| Source/image/rollback evidence | `source-manifest`, `rollback-manifest`, `rollback-check` |
| Source hygiene and execution review | `source-guard` |
| Mutation evidence | `policy-campaign`, `mutation-campaign` |

Record live provider legs, protocol scenarios, Rust tests, policy input controls
and compiled implementation mutants separately. Repeated runs and alternate
spellings of one control do not increase a distinct-case count. Retain external
logs and exact source identities; do not check generated evidence into the
repository. A successful workflow or unit suite does not authorize publication,
release or deployment.
