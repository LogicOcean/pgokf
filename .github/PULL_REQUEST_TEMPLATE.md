<!--
Thanks for contributing to pgokf! Please fill this in and check the boxes.
By opening this PR you agree to the project's Contributor License Agreement
(see CONTRIBUTING.md) — required because pgokf is dual-licensed (AGPL + commercial).
-->

## Summary

<!-- What does this change and why? -->

Closes #

## Type of change

- [ ] Bug fix (non-breaking)
- [ ] New feature (non-breaking)
- [ ] Breaking change (surface / behavior — called out in the CHANGELOG)
- [ ] Docs / tooling / CI only

## Checklist

- [ ] `cargo fmt --all -- --check` is clean
- [ ] `cargo clippy --workspace --all-targets --no-default-features --features pg18 -- -D warnings` is clean
- [ ] `cargo deny check` passes
- [ ] `cargo test -p pgokf --no-default-features --features pg18` passes (unit + api-stability)
- [ ] `RUST_TEST_THREADS=1 cargo pgrx test pg18 --no-default-features --features pg18` passes
- [ ] New/changed public SQL objects carry a `COMMENT ON` and are listed in `tests/api_stability.rs`
- [ ] Any stored-data change ships an upgrade script (`sql/pgokf--<from>--<to>.sql`), verified `upgrade == fresh`
- [ ] New source files carry an `SPDX-License-Identifier: AGPL-3.0-only` header
- [ ] Docs / CHANGELOG updated
- [ ] I agree to the project CLA (see `CONTRIBUTING.md`)

## How this was tested

<!-- Live steps, new tests, or evidence. -->
