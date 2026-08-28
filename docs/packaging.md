# Packaging & Distribution

How pgokf is packaged and released for every supported channel. Supported
PostgreSQL majors: **15, 16, 17, 18, 19** (19 is not GA as of 2026-08 and PGDG
publishes no `postgresql-19` package yet — its build/CI legs are advisory until
PGDG ships packages).

Every format shares one build primitive, so there is exactly one place where
the extension image is produced.

## The single build primitive

`cargo pgrx package` compiles the extension and writes a **filesystem image**
whose directory tree mirrors the target root, using the paths reported by the
supplied `pg_config`:

```bash
cd crates/extension
cargo pgrx package \
  --no-default-features --features pg${PGVER} \
  --pg-config /path/to/pg${PGVER}/bin/pg_config
```

Output (Debian/Ubuntu example, `PGVER=18`):

```
target/release/pgokf-pg18/
└── usr/
    ├── lib/postgresql/18/lib/pgokf.so                      # $(pg_config --pkglibdir)
    └── share/postgresql/18/extension/
        ├── pgokf.control
        ├── pgokf--0.1.0.sql
        └── pgokf--0.1.0--0.1.1.sql                          # $(pg_config --sharedir)/extension
```

On PGDG RPM systems the same command against `/usr/pgsql-18/bin/pg_config`
produces `usr/pgsql-18/lib/...` and `usr/pgsql-18/share/extension/...`.

**Because the tree already mirrors the install root, every packaging format
does the same thing: run this command, then copy the resulting tree into the
package payload.** `--no-default-features --features pg${PGVER}` pins the build
to exactly one PostgreSQL major.

Pinned toolchain (keep in sync with `rust-toolchain.toml` and CI):
`rustc 1.96.0`, `cargo-pgrx 0.19.2` (`cargo install --locked cargo-pgrx --version 0.19.2`).

---

## Debian / Ubuntu (`.deb`)

One package per major, named `postgresql-N-pgokf`, depending on `postgresql-N`
— mirroring PGDG's own per-major extension packages.

```bash
packaging/deb/build-deb.sh 18          # or 15 / 16 / 17 / 19
```

`build-deb.sh` runs the build primitive, stages the tree into a package root,
renders `DEBIAN/control` from [`packaging/deb/control.template`](https://github.com/LogicOcean/pgokf/blob/main/packaging/deb/control.template),
and calls `dpkg-deb --root-owner-group --build`. Output defaults to
`target/packaging/deb/postgresql-N-pgokf_<version>-1_<arch>.deb`.

| Override      | Default                                               |
| ------------- | ----------------------------------------------------- |
| `PG_MAJOR`    | `18` (or the first positional argument)               |
| `PG_CONFIG`   | `/usr/lib/postgresql/$PG_MAJOR/bin/pg_config`         |
| `DEB_REVISION`| `1`                                                   |
| `OUTPUT_DIR`  | `<repo>/target/packaging/deb`                         |
| `MAINTAINER`  | repo author                                           |

Inspect and install:

```bash
dpkg-deb -I postgresql-18-pgokf_0.1.0-1_amd64.deb   # control metadata
dpkg-deb -c postgresql-18-pgokf_0.1.0-1_amd64.deb   # payload file list
sudo apt install ./postgresql-18-pgokf_0.1.0-1_amd64.deb
```

Building the `.deb` for a major requires that major's `postgresql-server-dev-N`
(for `pg_config`) — only the locally installed major can be built on a given
host.

---

## RHEL / Fedora (`.rpm`)

[`packaging/rpm/pgokf.spec`](https://github.com/LogicOcean/pgokf/blob/main/packaging/rpm/pgokf.spec) follows the PGDG
convention: package `pgokf_NN`, installed under `/usr/pgsql-NN`, depending on
`postgresql NN-server`. The major is chosen at build time:

```bash
rpmbuild -ba packaging/rpm/pgokf.spec --define 'pgmajorversion 16'
# or under mock for a clean chroot:
mock -r rocky-9-x86_64 --define 'pgmajorversion 16' \
     --buildsrpm --spec packaging/rpm/pgokf.spec --sources .
```

`%build` installs the pinned `cargo-pgrx` into a build-local root and runs the
build primitive; `%install` copies the staged tree into `%{buildroot}`.
`Source0` is a `pgokf-0.1.0.tar.gz` of the repository at the release tag.

---

## PGXN (`META.json`)

[`META.json`](https://github.com/LogicOcean/pgokf/blob/main/META.json) is a PGXN meta-spec v1.0.0 distribution manifest
(`name` `pgokf`, `version` `0.1.0`, `provides.pgokf`, `prereqs` PostgreSQL
≥ 15, `resources`, MIT license). `provides.pgokf.file` points at the generated
`crates/extension/sql/pgokf--0.1.0.sql`, which the release process emits into
the tree before building the PGXN zip.

Validate locally:

```bash
jq empty META.json                                  # well-formed JSON
# required v1 fields present:
jq -e 'has("name") and has("version") and has("abstract")
       and has("maintainer") and has("license") and has("provides")
       and has("meta-spec")' META.json
```

---

## Docker image (ghcr.io)

[`packaging/docker/Dockerfile`](https://github.com/LogicOcean/pgokf/blob/main/packaging/docker/Dockerfile) is a stock
`postgres:N` image with the extension pre-installed, so `CREATE EXTENSION
pgokf;` works out of the box (auto-created on first init). Build from the
**repository root**:

```bash
docker build -f packaging/docker/Dockerfile \
  --build-arg PG_MAJOR=18 \
  -t ghcr.io/logicocean/pgokf:0.1.0-pg18 .
```

Add a `.dockerignore` excluding `target/` and `.git/` to keep the build
context small. Details and a compose snippet: [packaging/docker/README.md](https://github.com/LogicOcean/pgokf/blob/main/packaging/docker/README.md).

---

## Homebrew tap

[`packaging/homebrew/pgokf.rb`](https://github.com/LogicOcean/pgokf/blob/main/packaging/homebrew/pgokf.rb) builds from
source against Homebrew's `postgresql@N` and installs into that keg. For a tap
`LogicOcean/homebrew-pgokf`:

```bash
brew tap logicocean/pgokf
brew install pgokf
```

Update `url`, `sha256` (from the release tarball), and the `postgresql@N`
dependency at each release.

---

## Release process

`PGVER` ranges over 15-19; 19 is best-effort until PGDG ships packages.

1. **Gate.** Complete [release-checklist.md](release-checklist.md) (static,
   supply-chain, schema, and per-major live smoke gates). Confirm
   [CHANGELOG.md](https://github.com/LogicOcean/pgokf/blob/main/CHANGELOG.md) records the release.
2. **Version bump.** Ensure the version agrees across
   `Cargo.toml` (`[workspace.package]`), `crates/extension/pgokf.control`
   (`default_version`), `META.json` (`version` and `provides.pgokf.version`),
   and `packaging/rpm/pgokf.spec` (`Version`).
3. **Tag.** `git tag v0.1.0 && git push origin v0.1.0`. CI
   ([`.github/workflows/packages.yml`](https://github.com/LogicOcean/pgokf/blob/main/.github/workflows/packages.yml))
   builds the `.deb`s, validates `META.json`, and builds the Docker images per
   major, uploading them as workflow artifacts.
4. **PGXN.** Emit the generated SQL into the tree
   (`cd crates/extension && cargo pgrx schema pg18 > sql/pgokf--0.1.0.sql`),
   build the distribution zip (repo contents + `META.json` + generated SQL),
   and upload it at <https://manager.pgxn.org/> under the `pgokf` distribution.
5. **Docker.** Push each major's image to
   `ghcr.io/logicocean/pgokf:0.1.0-pgN` and retag the default major as
   `latest`.
6. **Homebrew.** In the tap repo, update `pgokf.rb` `url` + `sha256` for
   `v0.1.0` and open the PR (`brew audit --new pgokf` locally first).
7. **Announce.** GitHub Release notes from the CHANGELOG entry.

## Build-output hygiene

Built `.deb`/`.rpm` files and images are **never committed**. `build-deb.sh`
writes under `target/` (git-ignored); `packaging/**/build/`, `*.deb`, and
`*.rpm` are also git-ignored. Build into `target/packaging/` or `/tmp`.
