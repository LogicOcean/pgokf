# Vendored `pgrx` 0.19.2

This directory is the published crates.io `pgrx` 0.19.2 crate with one minimal
delta. It is wired in through `[patch.crates-io]` in the workspace root
`Cargo.toml`, so every build - local, CI, Docker, RPM, deb, Homebrew - uses
this exact source. Upstream is MIT-licensed; the license text is in `LICENSE`.

## Exact source

- Crate: `pgrx 0.19.2` from crates.io
- `.crate` archive SHA-256:
  `8d7e6f85d841cd9c01092ad3b9d60fc778bf54dc6174bbd24126250659af7d91`
- Upstream git revision (from the crate's `.cargo_vcs_info.json`):
  `70383e884582d1bcc7cd681d10886b995a2830cb`
  (<https://github.com/pgcentralfoundation/pgrx>), path `pgrx`

## Why

`pgrx 0.19.2` (the latest published release, and current upstream `develop`
at the revision above) has a mandatory dependency on `serde_cbor 0.11.2`,
which is unmaintained ([RUSTSEC-2021-0127](https://rustsec.org/advisories/RUSTSEC-2021-0127.html);
no patched release exists). The advisory is an unmaintained-crate notice, not
a demonstrated exploitable vulnerability, but the release policy is a clean,
unsuppressed `cargo audit --deny warnings`, so the dependency is removed from
the resolved graph rather than ignored.

## Exact delta

1. `Cargo.toml`: the `[dependencies.serde_cbor]` section is removed.
2. `src/datum/varlena.rs`: the three `#[doc(hidden)]` helpers `cbor_encode`,
   `cbor_decode`, and `cbor_decode_into_context` (the only serde_cbor users in
   the crate) are removed, along with the imports only they used
   (`serde::{Deserialize, Serialize}`, `StringInfo`, `varsize_any_exhdr`).
3. `src/inoutfuncs.rs`: the doc comment naming serde_cbor is updated.

Nothing else is changed. The helpers existed for `#[derive(PostgresType)]`
default storage functions; no crate in this workspace (and nothing in
`pgrx-macros`/`pgrx-sql-entity-graph` output used here) derives a custom
`PostgresType`, so the removal changes no runtime or SQL behavior - any
future use fails at compile time, which is the fail-closed property we want.
The workspace's dependency-graph regression
(`tests/test_release_integrity.py`) fails if `serde_cbor` re-enters
`Cargo.lock` or the patch is dropped.

## Updating

When a maintained pgrx release drops the serde_cbor dependency, delete this
directory, the `[patch.crates-io]` entry, and this note, and take the normal
dependency update. Until then, a pgrx upgrade re-applies this delta onto the
new published crate and updates this file.
