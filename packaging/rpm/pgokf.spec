# pgokf.spec -- RPM spec for the pgokf PostgreSQL extension (RHEL / Fedora).
#
# Follows the PGDG per-major packaging convention: one binary package per
# PostgreSQL major, named  pgokf_NN  and installed under the versioned PGDG
# tree  /usr/pgsql-NN.  The major is selected at build time:
#
#   rpmbuild -ba packaging/rpm/pgokf.spec --define 'pgmajorversion 16'
#
# (or pass it to mock / koji the same way). It defaults to 18.
#
# Layout produced -- identical artifacts to the .deb, only the prefix differs:
#   %{pginstdir}/lib/pgokf.so
#   %{pginstdir}/share/extension/pgokf.control
#   %{pginstdir}/share/extension/pgokf--%{version}.sql
#   %{pginstdir}/share/extension/pgokf--%{version}--0.1.1.sql
#
# Like every other format, the build delegates the filesystem image entirely
# to `cargo pgrx package`, whose output tree mirrors the target root; %install
# is a single copy of that tree into the buildroot (see docs/packaging.md).

%{!?pgmajorversion: %global pgmajorversion 18}
%global pginstdir     /usr/pgsql-%{pgmajorversion}
%global sname         pgokf
%global cargo_pgrx_version 0.19.2

# pgrx ships a prebuilt cdylib; skip the debuginfo/ELF post-processing that
# assumes a distro-standard build, and do not let rpmbuild strip the .so.
%global debug_package %{nil}
%global __strip       /bin/true

Name:           %{sname}_%{pgmajorversion}
Version:        0.1.0
Release:        1%{?dist}
Summary:        Materialized PostgreSQL catalog for Open Knowledge Format bundles

License:        MIT
URL:            https://github.com/LogicOcean/okf-pg-catalog
Source0:        %{sname}-%{version}.tar.gz

BuildRequires:  postgresql%{pgmajorversion}-devel
BuildRequires:  cargo
BuildRequires:  rust >= 1.96
BuildRequires:  clang
BuildRequires:  gcc
BuildRequires:  pkgconfig

Requires:       postgresql%{pgmajorversion}-server
Requires(post): postgresql%{pgmajorversion}-server

%description
pgokf materializes Open Knowledge Format (OKF) bundles -- directories of
UTF-8 Markdown concept documents with YAML frontmatter -- into a
transactional, queryable PostgreSQL catalog with native full-text search and
link-graph traversal. The on-disk bundle remains the portable source of
truth; PostgreSQL becomes a projection optimized for metadata queries.

This package is built for PostgreSQL %{pgmajorversion}. After installation,
run "CREATE EXTENSION pgokf;" in a database on a PostgreSQL %{pgmajorversion}
cluster.

%prep
%setup -q -n %{sname}-%{version}

%build
# cargo-pgrx is the build driver; install it into a build-local root so the
# build never mutates the invoking user's ~/.cargo (reproducible under mock).
export PATH="%{_builddir}/cargo-pgrx-bin/bin:${PATH}"
export CARGO_HOME="%{_builddir}/.cargo"
cargo install cargo-pgrx --version %{cargo_pgrx_version} --locked \
    --root "%{_builddir}/cargo-pgrx-bin"
cargo pgrx init --pg%{pgmajorversion}=%{pginstdir}/bin/pg_config

cd crates/extension
# --no-default-features pins the build to exactly one pgNN feature.
cargo pgrx package \
    --no-default-features --features pg%{pgmajorversion} \
    --pg-config %{pginstdir}/bin/pg_config

%install
rm -rf %{buildroot}
# The pgrx output tree already mirrors the target root under
# target/release/%{sname}-pg%{pgmajorversion}/usr/pgsql-NN/... , so the entire
# install is one verbatim copy into the buildroot.
cp -a target/release/%{sname}-pg%{pgmajorversion}/. %{buildroot}/

%files
%{pginstdir}/lib/%{sname}.so
%{pginstdir}/share/extension/%{sname}.control
%{pginstdir}/share/extension/%{sname}--%{version}.sql
%{pginstdir}/share/extension/%{sname}--%{version}--0.1.1.sql

%changelog
* Wed Aug 27 2026 David Saroka <david.saroka@gmail.com> - 0.1.0-1
- Initial RPM packaging of pgokf for PostgreSQL 15-19.
