# pgokf verifier

An unpublished workspace crate for deterministic verification policies. Run its
unit and integration tests with `cargo test --locked -p pgokf-verifier`; the
workspace's default test selection and `scripts/test-workspace.sh` include it.
Run `cargo run --locked -p pgokf-verifier -- --help` for available commands.

The SQL module supplies lexical boundaries, not a SQL semantic or dynamic-code
proof. Install comparison preserves quoted bytes, token separators that affect
meaning, ordered SQL outside generated regions, and generated-entity multiplicity.
Live catalog parity is still needed to establish SQL semantic equivalence.

`positional-check ROOT` inspects shipped Rust, SQL and text for dependencies on
physical column order. Its bounded source language expands constants, includes,
finite branches, concatenation, formatting and reviewed structural generators;
unknown SQL inputs and generating constructs are refused. It rejects stars,
whole rows, composite casts and arrays, positional writes and ordinal ordering.
`positional-statements ROOT` emits the deterministic product statement registry.
These checks complement live catalog comparison, which permits only the two
complete declared physical orders and refuses a third order.

`migration-check ROOT VERSION` validates reachability, cycles, terminal releases,
consecutive development edges, frozen history and exact finalization receipts.
Executable routine bodies, nested dynamic SQL, data-modifying CTEs, destructive
statements and system-catalog writes are checked before a migration earns proof.

`catalog-validate CONTRACT SNAPSHOT` validates typed runtime and ordinary indexes,
tier roles, database-specific role settings, memberships and row-type dependencies.
`catalog-accept CONTRACT CAPTURE` additionally enforces relation ownership, table
and column grants, tenant policies and complete member-catalog comparison. The
reference must come from an independently reviewed fresh build. Provider shapes
are checked against extension identities and configuration, and HNSW presence uses
an explicit `not_requested` or `rebuilt` policy and eligible dimensions.
`sql/catalog-snapshot.sql` supports PostgreSQL 15 and later.

`parity` runs fresh, historical, live-shaped, refusal, interrupted-transaction and
retry routes with catalog and populated-data checks. `--contract-output NEW_FILE`
and `--controls-output NEW_FILE` emit typed reference and negative-control evidence.
Native clusters use private sockets; Docker clusters and extraction helpers use
durable ownership receipts. Cleanup failure refuses successful completion.

`historical-check FILE` accepts only exact reviewed historical SQL bytes. The
published artifact is bound to an immutable registry digest, and the alternative
source-build artifact is bound to its reviewed source commit in `src/historical.rs`.
`compatibility-parity ROOT PG_CONFIG` extracts from the pinned image. Installed SQL
and `--historical-install FILE` overrides must pass the same authenticity boundary.

`reconciliation-mutations --results NEW_FILE` preserves the declared semantic
cases and accepts `--only NAME` selections. Each campaign freezes its source,
requires unchanged and restored live/unit/control baselines, recompiles every
mutant, and binds actual executable hashes to source manifests. Unit kills require
one exact selected failure and its named semantic assertion; compilation, setup,
unrelated panics and missing tests never earn a kill. Evidence is written outside
the repository, and every selected case must pass before the campaign succeeds.

`workflow-check ROOT` checks reviewed execution-context hashes, PostgreSQL support
matrices and publication prerequisites. `source-guard ROOT` rejects owned Python
files, shebangs, verification dependencies and executable tooling, with an exact
exception for third-party MkDocs setup. The complete source inventory, including
executable permissions and transitive wrapper inputs, is checked against a review
embedded in the trusted verifier binary. A changed on-disk review cannot authorize
new commands. This is an explicit review boundary, not a sandbox.

`policy-campaign ROOT OUTPUT` runs the named workflow attack registry and actual
Homebrew formula substitutions. `mutation-campaign ROOT MANIFEST OUTPUT` compiles
Rust implementation substitutions in an owned clone and requires the exact test
and intended assertion to fail. Setup, compilation and unrelated failures earn
no mutation credit. See [Rust verification](../../docs/rust-verification.md) for
commands, the campaign schema, evidence and source-review maintenance.

Docker creation and cleanup use durable owned receipts, independent volume
nonces, exact resource identities and repeated inspections through the
`owned-create`, `owned-cleanup` and `ownership-controls` commands. Protocol,
provider and package acceptance still requires the applicable live gates;
portable tests alone do not establish that coverage.

Release and packaging commands use the same Rust verifier:

```sh
cargo run --locked -p pgokf-verifier -- release-check .
cargo run --locked -p pgokf-verifier -- history-check .
cargo run --locked -p pgokf-verifier -- workflow-check .
cargo run --locked -p pgokf-verifier -- checkout-shapes . evidence-checkouts
cargo run --locked -p pgokf-verifier -- homebrew-policy .
cargo run --locked -p pgokf-verifier -- homebrew-packaging . evidence-homebrew
cargo run --locked -p pgokf-verifier -- homebrew-observation evidence-homebrew/test.log 0.4.0-dev3
cargo run --locked -p pgokf-verifier -- compatibility-image . . pgokf-beta-compat beta-image.json
cargo run --locked -p pgokf-verifier -- image-smoke . IMAGE VERSION optional evidence-image
cargo run --locked -p pgokf-verifier -- package-checksums 1.4.0 packaging/docker/pg_textsearch.sha256
cargo run --locked -p pgokf-verifier -- rollback-check rollback.json
```

Evidence destinations must be new. Homebrew commands require macOS; the packaging
build additionally requires arm64. The Ruby adapter exercises Homebrew's actual
Formula API; Rust owns the driver, assertions, identity and cleanup. Image smoke
uses standard Docker context environment settings and receipt-bound resources.
Source and image manifests identify the current source separately from validation
tooling. Rollback validation consumes immutable image identities and an
authenticated previous formula tuple; it performs no publication or deployment.

`release-resolve SOURCE_ROOT` consumes `GITHUB_REF`, `RELEASE_TAG_INPUT` and
`GITHUB_OUTPUT`. It writes output only after all identity and Git checks pass.
`workflow-contexts ROOT` prints candidate context hashes for review; generating
hashes alone is not approval of changed workflow execution.

For a local rollback rehearsal, build both immutable images from authenticated
Git source, using a clean current checkout with full release history:

```sh
cargo run --locked -p pgokf-verifier -- image-build . . 0.4.0-dev3 18 pgokf-local-current current-image.json
cargo run --locked -p pgokf-verifier -- image-build . . 0.2.0 18 pgokf-local-previous previous-image.json
cargo run --locked -p pgokf-verifier -- rollback-manifest current-image.json previous-image.json packaging/homebrew/pgokf.rb rollback.json
cargo run --locked -p pgokf-verifier -- rollback-check rollback.json
```

The historical builder accepts only pinned release commits with authenticated
published formula digests. It never changes current-source release eligibility.
Both source and tooling are captured from immutable Git archives before build;
image labels bind the source inventory, commit, tooling commit and PostgreSQL
selection. Validation checks those labels against the exact local image ID and
reconstructs the source inventory from Git. This assumes the local Docker daemon
and Git object store are trusted; labels are not publisher signatures. Images
without these build records must be rebuilt, not relabelled. The commands build
local images and create manifests only; they do not publish or deploy.

`compose-check ROOT` renders every shipped profile without creating services.
`companions-smoke IMAGE NEW_EVIDENCE` checks each packaged companion's help,
interval refusal and non-root user with owned resource receipts.

`mcp-golden ROOT PG_CONFIG MCP_BINARY NEW_EVIDENCE_DIR` captures and compares all
72 frozen fixtures against a disposable PostgreSQL 18 cluster. `mcp-native` takes
the same arguments and executes the full wire, semantic, role, tenant, cache,
resource, tool and Skills surface, including negative controls and coverage
closure. Both require the candidate extension installed into the selected
PostgreSQL installation. Evidence directories must be new; artifact hashes and
owned process receipts bind each run and cleanup.

`mcp-external` takes the same arguments and executes the pinned official core and
Skills scenarios, both SDK modes and ten Inspector calls. It installs official
Node clients in the evidence directory. `PGOKF_MCP_EXTERNAL_CLIENTS` may name an
existing directory containing the pinned `@modelcontextprotocol` packages, and
`PGOKF_MCP_SKILLS_SOURCE` may name a clean checkout of the pinned Skills source.
All three npm package archives and installed published files are authenticated
before use. Official results must match frozen check identities and statuses;
intentional repeated requirement IDs are distinguished by their check names.
`PGOKF_MCP_SCHEMA_DIR` supplies an optional cache whose descriptors and bytes are
validated against the unchanged source pins.

`mcp-native ... --mutation NAME` removes one concrete probe family and must fail
coverage closure: `tool-calls`, `resource-reads`, `lifecycle`, `headers`, `schemas`,
`role-tenant`, `cache-scope`, `cache-positive`, or `cache-ttl`. Mutation controls
are evidence about verification behavior, not passing product scenarios.

`corpus --count N --out DIRECTORY --seed INTEGER [--manifest FILE]` creates the
deterministic benchmark corpus. `corpus-field MANIFEST FIELD` reads typed manifest
fields for the benchmark shell driver. `corpus-check DIRECTORY` requires at least
10,000 actual Markdown files for benchmark acceptance. All verifier subprocesses use bounded argv
execution; PostgreSQL processes have inherited `PG*` configuration removed.

Live observation fault tests require explicit local images and new evidence paths:

```sh
PGOKF_SMOKE_IMAGE=IMAGE PGOKF_SMOKE_FAULT=roots PGOKF_SMOKE_EVIDENCE=evidence-roots \
  cargo test --locked -p pgokf-verifier --test image_smoke_faults server_observation_fault_is_refused -- --exact --ignored
PGOKF_SMOKE_IMAGE=COMPANIONS_IMAGE PGOKF_SMOKE_FAULT=uid PGOKF_SMOKE_EVIDENCE=evidence-uid \
  cargo test --locked -p pgokf-verifier --test image_smoke_faults companion_observation_fault_is_refused -- --exact --ignored
```

Server cases are `version`, `policy_init`, `restore_init`, `roots`, `extensions`, `roles`, `materialization`, `cron_binding`,
`cron_removed`, `health`, `bm25_build`, `bm25_reader`, `vector`, `backup_count`,
`backup_data`, `restore_count`, `restore_policy`, and `restore_health`. Companion
cases are `embed`, `ingest`, `web`, `mcp`, `interval`, and `uid`. Server cases require
a PostgreSQL 18 image with the optional providers. Each test alters one actual
Docker observation and requires the relevant refusal while retaining owned cleanup
receipts. These explicit live tests supplement the ordinary portable test suite.

`postgres-target MAJOR IMAGE_TAG`, `postgres-candidate MAJOR IMAGE_TAG VERSION`,
`postgres-check MAJOR IMAGE_TAG`, and `postgres-beta-image` own the reviewed
PostgreSQL target, apt candidate, installed server/header, and Docker index checks.
The packaging shell entry points only dispatch to these commands or provision the
selected packages. For root provisioning, build the verifier first and supply its
absolute executable path through `PGOKF_VERIFIER`; this is an executable path,
never a shell command string. Docker builds compile the canonical verifier in the
builder stage before compiling the extension. Historical compatibility builds
supply the verifier from their immutable current-tooling context.

`image-build-with-builder` accepts the same arguments as `image-build` and also
retains the identical immutable builder context as `LOCAL_TAG-builder`. This local
stage supports compiler checks such as `cargo check --locked -p pgokf --all-targets
--no-default-features --features pg19,pg_test`; use owned-container creation and
cleanup for execution. The output image manifest still identifies only the runtime
image, not the builder stage. Set `CARGO_BUILD_JOBS=2` and `CARGO_INCREMENTAL=0`
inside the check container to bound compiler resource use.

Portable Linux tests need Ruby's standard `Pathname` and JSON libraries to
observe the actual formula command encoding. CI installs Ruby explicitly. Ruby
adapters emit command/environment observations only; Rust validates the exact SQL,
four input cases, install execution, deployment target and composed flag values.
