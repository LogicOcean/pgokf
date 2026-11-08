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

The source guard inventories tracked and nonignored untracked files using Git,
including tracked files in ignored directories. It rejects source symlinks,
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
