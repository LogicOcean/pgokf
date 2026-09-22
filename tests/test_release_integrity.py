#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Release-integrity guards: the current release identity must agree across
every artifact, and the committed release SQL must be structurally sound.

Each check is a pure function over file contents, so the reviewer's exact
mutations are reproduced here as in-memory negative fixtures: a wrong
install-script version function, a stale Homebrew URL/digest pair, a stale
exact pin or lock entry, a missing/renamed finalization edge, a script
sourced from the final release, a missing install script, a receipt
containing a data mutation, and a stale workflow/package version.

These are source-level structure checks. Runtime semantics are proven by the
live gates (scripts/upgrade-parity.py executes the committed install script
in an isolated scratch cluster and compares the resulting catalog
object-by-object); this suite's job is to catch identity drift before those
gates run.
"""
import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

FIRST_PARTY = [
    'pgokf', 'okf-parser', 'okf-sync', 'pgokf-companion', 'pgokf-embed',
    'pgokf-ingest', 'pgokf-mcp', 'pgokf-pgconn', 'pgokf-web', 'pgokf-workspace',
]

# Authenticated SHA-256 digests of published GitHub codeload archives, one per
# published release. A release's digest is appended here by the post-tag
# commit that also advances the Homebrew formula (docs/release-checklist.md).
KNOWN_RELEASE_DIGESTS = {
    '0.2.0': '194441d1b4d6bd5cf5f22a39a3b6c923e9a68d7692202ff7c4bb05109df812e5',
}


def control_version(control: str) -> str:
    m = re.search(r"default_version\s*=\s*'([^']+)'", control)
    assert m, 'pgokf.control must declare default_version'
    return m[1]


def workspace_version(cargo_toml: str) -> str:
    m = re.search(r'(?m)^version\s*=\s*"([^"]+)"', cargo_toml)
    assert m, 'workspace Cargo.toml must declare version'
    return m[1]


# ---------------------------------------------------------------------------
# Cargo identity: internal exact pins, lockfile first-party entries, and the
# serde_cbor / RUSTSEC-2021-0127 dependency-graph regression.
# ---------------------------------------------------------------------------

def check_lockfile(lock: str, version: str) -> list[str]:
    problems = []
    packages = re.findall(r'(?m)^\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"', lock)
    seen = {name: ver for name, ver in packages}
    for name in FIRST_PARTY:
        if name not in seen:
            problems.append(f'Cargo.lock has no package entry for {name}')
        elif seen[name] != version:
            problems.append(f'Cargo.lock pins {name} at {seen[name]}, not {version}')
    for name, _ in packages:
        if name == 'serde_cbor':
            problems.append('serde_cbor is back in the dependency graph (RUSTSEC-2021-0127); '
                            'the vendored pgrx patch (vendor/pgrx) removes it - do not reintroduce it')
    return problems


def check_internal_pins(manifests: dict[str, str], version: str) -> list[str]:
    problems = []
    for path, text in manifests.items():
        for dep, pin in re.findall(r'(?m)^([a-z0-9-]+)\s*=\s*\{\s*version\s*=\s*"=([^"]+)"', text):
            if dep in FIRST_PARTY and pin != version:
                problems.append(f'{path}: exact pin {dep} ={pin} != {version}')
    return problems


# ---------------------------------------------------------------------------
# SQL release graph and the committed install script.
# ---------------------------------------------------------------------------

def parse_edges(names: list[str]) -> list[tuple[str, str]]:
    edges = []
    for name in names:
        m = re.fullmatch(r'pgokf--(.+)--(.+)\.sql', name)
        if m:
            edges.append((m[1], m[2]))
    return edges


def check_sql_graph(names: list[str], version: str) -> list[str]:
    """The shipped chain must run 0.1.0 -> ... -> <version>, with <version>
    the unique terminal: reachable, and the source of no script."""
    problems = []
    edges = parse_edges(names)
    visited, frontier = {'0.1.0'}, ['0.1.0']
    while frontier:
        at = frontier.pop()
        for src, dst in edges:
            if src == at and dst not in visited:
                visited.add(dst)
                frontier.append(dst)
    if version not in visited:
        problems.append(f'no upgrade path from 0.1.0 to {version}')
    for src, dst in edges:
        if src == version:
            problems.append(f'final release {version} is the source of pgokf--{src}--{dst}.sql')
        if src not in visited:
            problems.append(f'unreachable upgrade edge {src} -> {dst}')
    sources = {src for src, _ in edges}
    for terminal in {v for e in edges for v in e} - sources:
        if terminal != version:
            problems.append(f'upgrade chain terminates at {terminal}, not {version}')
    if f'pgokf--{version}.sql' not in names:
        problems.append(f'committed install script pgokf--{version}.sql is missing')
    return problems


def receipt_statements(sql: str) -> list[str]:
    executable = '\n'.join(line.split('--')[0] for line in sql.splitlines())
    return [re.sub(r'\s+', ' ', s).strip().upper()
            for s in executable.split(';') if s.split()]


def check_receipt(sql: str) -> list[str]:
    stmts = receipt_statements(sql)
    if stmts != ['SELECT PGOKF_PRIVATE.REGISTER_DUMP_RELATIONS()']:
        return [f'finalization receipt must be exactly the dump-relation registration, got {stmts}']
    return []


def check_install_script(sql: str, lib_rs: str) -> list[str]:
    """The committed install script must bind pgokf.version() exactly once, to
    the pgrx-generated C wrapper - a SQL-language redefinition (the reviewer's
    planted mutation returned '0.2.0') would lie about the release identity."""
    problems = []
    defs = re.findall(r'CREATE\s+(?:OR\s+REPLACE\s+)?FUNCTION\s+pgokf\.(?:"version"|version)\(\)', sql)
    if len(defs) != 1:
        problems.append(f'expected exactly one pgokf.version() definition, found {len(defs)}')
    m = re.search(r'CREATE\s+FUNCTION\s+pgokf\.(?:"version"|version)\(\)[^;]*?LANGUAGE\s+(\w+)[^;]*?AS\s+\'([^\']+)\',\s*\'([^\']+)\'',
                  sql, re.S)
    if not m:
        problems.append('pgokf.version() is not the pgrx C binding (MODULE_PATHNAME, version_wrapper)')
    elif m[1].lower() != 'c' or m[2] != 'MODULE_PATHNAME' or m[3] != 'version_wrapper':
        problems.append(f'pgokf.version() must be LANGUAGE c AS MODULE_PATHNAME/version_wrapper, '
                        f'got LANGUAGE {m[1]} AS {m[2]}/{m[3]}')
    if re.search(r'LANGUAGE\s+sql', sql, re.I) and re.search(
            r'FUNCTION\s+pgokf\.(?:"version"|version)\(\)[^;]*LANGUAGE\s+sql', sql, re.S | re.I):
        problems.append('pgokf.version() must not be redefined in SQL')
    if not re.search(r'fn version\(\)\s*->\s*&\'static str \{\s*env!\("CARGO_PKG_VERSION"\)', lib_rs):
        problems.append('lib.rs version() must return env!("CARGO_PKG_VERSION")')
    return problems


# ---------------------------------------------------------------------------
# Homebrew: the in-repo formula is valid at every commit. Pre-tag it stays
# pinned to the last published release (url + authenticated digest + matching
# assertions); post-tag a deliberate main commit advances all three to the new
# release. The one invalid state is a new URL carrying an old digest.
# ---------------------------------------------------------------------------

def parse_formula(formula: str) -> tuple[str, str, list[str]]:
    url = re.search(r'url\s+"https://github\.com/LogicOcean/pgokf/archive/refs/tags/v([^"]+)\.tar\.gz"', formula)
    sha = re.search(r'sha256\s+"([0-9a-f]{64})"', formula)
    asserts = re.findall(r'assert_match\s+(?:"([^"]+)"|"default_version = \'([^\']+)\'")', formula)
    versions = [a or b for a, b in asserts]
    return (url[1] if url else '', sha[1] if sha else '', versions)


def check_homebrew(formula: str, version: str) -> list[str]:
    problems = []
    url_version, sha, assertions = parse_formula(formula)
    if url_version == "0.3.0":
        return ["v0.3.0 is an unpublished retired candidate, never a published tuple"]
    if not url_version or not sha:
        return ['formula must pin a versioned tag archive url and a sha256']
    if not assertions or any(a != url_version and f"'{url_version}'" not in a for a in assertions):
        problems.append(f'formula test assertions {assertions} do not match url version {url_version}')
    if url_version == version:
        if sha != KNOWN_RELEASE_DIGESTS.get(version):
            problems.append("current formula requires an authenticated post-tag digest record")
        # Post-tag state: the digest must be the new archive's, never a
        # previous release's authenticated digest carried forward.
        for old, digest in KNOWN_RELEASE_DIGESTS.items():
            if old != version and sha == digest:
                problems.append(f'formula url is v{version} but sha256 is the authenticated v{old} '
                                'digest - a knowingly broken pairing (fixed-point impossibility); '
                                'pre-tag the formula must stay pinned to the last published release')
    elif url_version in KNOWN_RELEASE_DIGESTS:
        # Pre-tag state: pinned to a published release with its exact digest.
        if sha != KNOWN_RELEASE_DIGESTS[url_version]:
            problems.append(f'formula pins v{url_version} but sha256 is not its authenticated digest')
    else:
        problems.append(f'formula pins v{url_version}, which is neither the current release '
                        f'{version} nor a published release with an authenticated digest')
    return problems


# ---------------------------------------------------------------------------
# packages.yml publication guards (F2): publication is immutable-tag-only.
# ---------------------------------------------------------------------------

def check_packages_workflow(text: str) -> list[str]:
    problems = []
    dispatch = re.search(r'workflow_dispatch:\n(.*?)\n\npermissions:', text, re.S)
    if not dispatch:
        problems.append('packages.yml has no workflow_dispatch block')
    else:
        if 'publish_images' in dispatch[1]:
            problems.append('publish_images boolean must not exist: it let arbitrary branch source '
                            'publish release image names; use the release_tag input')
        if 'release_tag' not in dispatch[1]:
            problems.append('workflow_dispatch needs an explicit release_tag input for current publication')
    if not re.search(r'uses: actions/checkout@[0-9a-f]{40}.*?\n\s+with:\n\s+fetch-depth: 0', text):
        problems.append('prep checkout must fetch-depth: 0 to prove tag identity')
    if 'packaging/resolve-release.sh' not in text:
        problems.append('prep must resolve version/publish through packaging/resolve-release.sh')
    if 'needs.prep.outputs.source_ref' not in text:
        problems.append('downstream jobs must check out needs.prep.outputs.source_ref '
                        '(the proven tag commit), never the dispatch branch HEAD')
    import yaml
    jobs = yaml.safe_load(text)['jobs']
    for name, job in jobs.items():
        if name != 'prep':
            for step in job.get('steps', []):
                if step.get('name') == 'Checkout release validation tooling':
                    if step.get('with') != {'ref': '${{ github.sha }}', 'path': 'release-tools'}:
                        problems.append('validation tooling must use the workflow commit in a separate directory')
                    continue
                if step.get('uses', '').startswith('actions/checkout@'):
                    if step.get('with', {}).get('ref') != '${{ needs.prep.outputs.source_ref }}':
                        problems.append(f'{name} must check out the proven source SHA')
        for key, value in job.get('env', {}).items():
            if key in ('VERSION', 'LOCAL_TAG', 'TAG') and 'needs.prep.outputs.version' not in str(value):
                problems.append(f'{name} {key} must use the proven package version')
    for manifest, build in (('docker-manifest', 'docker'), ('companions-manifest', 'companions')):
        if build not in jobs[manifest]['needs']:
            problems.append(f'{manifest} must require every {build} matrix leg')
    for line in text.splitlines():
        if re.search(r'pgokf(-companions)?:0\.[0-9]', line):
            problems.append(f'hardcoded image version in workflow: {line.strip()}')
    return problems


# ---------------------------------------------------------------------------
# IO wrappers over the checkout.
# ---------------------------------------------------------------------------

def read(path: str) -> str:
    return (ROOT / path).read_text()


def manifests() -> dict[str, str]:
    return {str(p.relative_to(ROOT)): p.read_text() for p in ROOT.glob('crates/*/Cargo.toml')}


class ReleaseIdentity(unittest.TestCase):
    maxDiff = None

    def test_version_agrees_across_artifacts(self):
        version = control_version(read('crates/extension/pgokf.control'))
        self.assertEqual(workspace_version(read('Cargo.toml')), version, 'workspace version')
        for name, manifest in manifests().items():
            declaration = tomllib.loads(manifest)['package']['version']
            self.assertIn(declaration, ({'workspace': True}, version), name)
        meta = json.loads(read('META.json'))
        self.assertEqual(meta['version'], version, 'META.json version')
        self.assertEqual(meta['provides']['pgokf']['version'], version, 'META.json provides version')
        self.assertEqual(meta['provides']['pgokf']['file'], f'crates/extension/sql/pgokf--{version}.sql')
        self.assertTrue((ROOT / meta['provides']['pgokf']['file']).is_file(), 'provides.file exists')
        self.assertRegex(read('packaging/rpm/pgokf.spec'), rf'(?m)^Version:\s+{re.escape(version)}$')
        env_example = read('deploy/compose/.env.example')
        self.assertIn(f'ghcr.io/logicocean/pgokf:{version}-pg18', env_example)
        self.assertIn(f'ghcr.io/logicocean/pgokf-companions:{version}', env_example)
        self.assertIn(f'"{version}"', read('.github/ISSUE_TEMPLATE/bug_report.yml'))
        for dockerfile in ('packaging/docker/Dockerfile', 'packaging/docker/Dockerfile.companions'):
            self.assertIn(f'ARG PGOKF_VERSION="{version}"', read(dockerfile))
            for stale in re.findall(r'(?m)(?:pgokf|pgokf-companions):(\d+\.\d+\.\d+)(?:-pg\d+)?\b', read(dockerfile)):
                self.assertEqual(stale, version, f'{dockerfile} example pins a stale version')

    def test_version_mutation_fixtures_are_rejected(self):
        version = control_version(read('crates/extension/pgokf.control'))
        spec = read('packaging/rpm/pgokf.spec')
        stale = re.sub(r'(?m)^Version:\s+.+$', 'Version:        0.2.0', spec)
        self.assertNotRegex(stale, rf'(?m)^Version:\s+{re.escape(version)}$',
                            'fixture sanity: the stale RPM mutation removes the current Version line')

    def test_lockfile_and_internal_pins(self):
        version = control_version(read('crates/extension/pgokf.control'))
        self.assertEqual(check_lockfile(read('Cargo.lock'), version), [])
        self.assertEqual(check_internal_pins(manifests(), version), [])
        self.assertIn('[patch.crates-io]', read('Cargo.toml'),
                      'the vendored pgrx patch (vendor/pgrx, removes serde_cbor) must be wired')

    def test_lockfile_mutation_fixtures_are_rejected(self):
        version = control_version(read('crates/extension/pgokf.control'))
        lock = read('Cargo.lock')
        stale_lock = lock.replace(f'name = "okf-parser"\nversion = "{version}"',
                                  f'name = "okf-parser"\nversion = "{version}-dev3"')
        self.assertIn('okf-parser', ' '.join(check_lockfile(stale_lock, version)))
        poisoned = lock + '\n[[package]]\nname = "serde_cbor"\nversion = "0.11.2"\n'
        self.assertIn('serde_cbor', ' '.join(check_lockfile(poisoned, version)))
        stale_pin = {p: t.replace(f'version = "={version}"', f'version = "={version}-dev3"', 1)
                     for p, t in manifests().items()}
        self.assertTrue(any(f'={version}-dev3' in p for p in check_internal_pins(stale_pin, version)))

    def test_sql_graph_and_finalization(self):
        version = control_version(read('crates/extension/pgokf.control'))
        names = [p.name for p in (ROOT / 'crates/extension/sql').iterdir()]
        self.assertEqual(check_sql_graph(names, version), [])
        receipt = [n for n in parse_edges(names)
                   if n[1] == version and n[0] == '0.3.0']
        self.assertEqual(len(receipt), 1, 'exactly one immutable candidate -> final edge')
        self.assertEqual(check_receipt(read(f'crates/extension/sql/pgokf--{receipt[0][0]}--{version}.sql')), [])

    def test_sql_graph_mutation_fixtures_are_rejected(self):
        version = control_version(read('crates/extension/pgokf.control'))
        names = [p.name for p in (ROOT / 'crates/extension/sql').iterdir()]
        edge = next(e for e in parse_edges(names)
                    if e[1] == version and e[0] == '0.3.0')
        edge_name = f'pgokf--{edge[0]}--{version}.sql'
        missing = [n for n in names if n != edge_name]
        self.assertTrue(check_sql_graph(missing, version), 'missing finalization edge must be rejected')
        renamed = [f'pgokf--{edge[0]}--{version}-final.sql' if n == edge_name else n for n in names]
        self.assertTrue(check_sql_graph(renamed, version), 'renamed finalization edge must be rejected')
        sourced = names + [f'pgokf--{version}--{version}.1.sql']
        self.assertTrue(any('source' in p for p in check_sql_graph(sourced, version)),
                        'a script sourced from the final release must be rejected')
        no_install = [n for n in names if n != f'pgokf--{version}.sql']
        self.assertTrue(any('install script' in p for p in check_sql_graph(no_install, version)))
        receipt = read(f'crates/extension/sql/{edge_name}')
        self.assertTrue(check_receipt(receipt + '\nDELETE FROM pgokf.bundles;\n'),
                        'a receipt carrying a data mutation must be rejected')

    def test_install_script_version_binding(self):
        self.assertEqual(check_install_script(read('crates/extension/sql/pgokf--0.3.1.sql'),
                                              read('crates/extension/src/lib.rs')), [])

    def test_install_script_mutation_fixtures_are_rejected(self):
        sql = read('crates/extension/sql/pgokf--0.3.1.sql')
        lib = read('crates/extension/src/lib.rs')
        # The reviewer's planted override: a SQL-language version function.
        override = sql + ("\nCREATE OR REPLACE FUNCTION pgokf.\"version\"() RETURNS TEXT\n"
                          "IMMUTABLE STRICT PARALLEL SAFE\nLANGUAGE sql AS 'SELECT ''0.2.0''';\n")
        self.assertTrue(check_install_script(override, lib), 'SQL version override must be rejected')
        wrong_source = lib.replace('env!("CARGO_PKG_VERSION")', '"0.2.0"')
        self.assertTrue(check_install_script(sql, wrong_source),
                        'a hardcoded version() body must be rejected')

    def test_homebrew_sql_survives_shell_quoting(self):
        import shlex
        formula = read('packaging/homebrew/pgokf.rb')
        expression = re.search(r'output = shell_output\(\s*(.*?)\s*,?\s*\)', formula, re.S)[1].rstrip(',')
        code = 'pg_bin="/bin"; port=15432; puts(' + expression + ')'
        command = subprocess.check_output(['ruby', '-e', code], text=True).strip()
        arguments = shlex.split(command)
        self.assertIn("WHERE extname='pgokf';", arguments[-1])

    def test_homebrew_state_machine(self):
        version = control_version(read('crates/extension/pgokf.control'))
        self.assertEqual(check_homebrew(read('packaging/homebrew/pgokf.rb'), version), [])

    def test_homebrew_mutation_fixtures_are_rejected(self):
        version = control_version(read('crates/extension/pgokf.control'))
        good = read('packaging/homebrew/pgokf.rb')
        url_version, _, _ = parse_formula(good)
        # The exact F1 defect: new release URL carrying the previous digest.
        new_url_old_digest = good.replace(f'v{url_version}.tar.gz', f'v{version}.tar.gz')
        if url_version != version:
            problems = check_homebrew(new_url_old_digest, version)
            self.assertTrue(any('digest' in p for p in problems),
                            f'url/digest mismatch must be rejected: {problems}')
        # A stale test assertion (disagrees with the url version).
        stale_assert = re.sub(r'assert_match "[^"]+"', 'assert_match "0.0.0"', good, count=1)
        self.assertTrue(check_homebrew(stale_assert, version),
                        'a stale formula test assertion must be rejected')

    def test_homebrew_post_tag_tuple_and_rollback(self):
        version = control_version(read('crates/extension/pgokf.control'))
        formula = read('packaging/homebrew/pgokf.rb')
        old, digest, _ = parse_formula(formula)
        current_digest = 'a' * 64
        current = formula.replace(old, version).replace(digest, current_digest)
        saved = dict(KNOWN_RELEASE_DIGESTS)
        self.addCleanup(lambda: (KNOWN_RELEASE_DIGESTS.clear(), KNOWN_RELEASE_DIGESTS.update(saved)))
        KNOWN_RELEASE_DIGESTS[version] = current_digest
        self.assertEqual(check_homebrew(current, version), [])

        previous_versions = [release for release in KNOWN_RELEASE_DIGESTS if release != version]
        self.assertTrue(previous_versions, 'a post-tag formula needs a known published rollback tuple')
        previous = max(previous_versions, key=lambda release: tuple(map(int, release.split('.'))))
        rollback = current.replace(version, previous).replace(
            current_digest, KNOWN_RELEASE_DIGESTS[previous])
        self.assertEqual(check_homebrew(rollback, version), [],
                         'complete prior-release tuple is a valid rollback')
        self.assertTrue(check_homebrew(
            current.replace(current_digest, KNOWN_RELEASE_DIGESTS[previous]), version),
            'current URL with a prior-release digest must be rejected')
        self.assertTrue(check_homebrew(
            current.replace(f"default_version = '{version}'",
                            f"default_version = '{previous}'"), version),
            'a mixed current/previous assertion tuple must be rejected')

    def test_homebrew_unknown_current_digest_rejected(self):
        version = control_version(read('crates/extension/pgokf.control'))
        good = read('packaging/homebrew/pgokf.rb')
        old, digest, _ = parse_formula(good)
        mutation = good.replace(old, version).replace(digest, 'a' * 64)
        self.assertTrue(check_homebrew(mutation, version))

    def test_packages_workflow_guards(self):
        self.assertEqual(check_packages_workflow(read('.github/workflows/packages.yml')), [])

    def test_stale_workflow_version_and_source_rejected(self):
        good = read('.github/workflows/packages.yml')
        self.assertTrue(check_packages_workflow(good.replace(
            'VERSION: ${{ needs.prep.outputs.version }}', 'VERSION: 0.2.0')))
        self.assertTrue(check_packages_workflow(good.replace(
            'ref: ${{ needs.prep.outputs.source_ref }}', 'ref: ${{ github.sha }}')))

    def test_dependency_patch_boundary(self):
        for path in (ROOT / 'crates').rglob('*.rs'):
            self.assertNotRegex(path.read_text(), r'\bPostgresType\b', str(path))
        for name in ('packaging/docker/Dockerfile', 'packaging/docker/Dockerfile.companions'):
            self.assertIn('COPY . .', read(name))
        self.assertNotRegex(read('.dockerignore'), r'(?m)^/?vendor/?$')
        metadata = json.loads(subprocess.check_output(
            ['cargo', 'metadata', '--locked', '--format-version', '1'], cwd=ROOT))
        resolved = [p for p in metadata['packages'] if p['name'] == 'pgrx']
        self.assertEqual(len(resolved), 1)
        self.assertEqual(Path(resolved[0]['manifest_path']), ROOT / 'vendor/pgrx/Cargo.toml')
        self.assertEqual(resolved[0]['version'], '0.19.2')
        self.assertNotIn('serde_cbor', [p['name'] for p in metadata['packages']])

    def test_resolve_release_script(self):
        """Execute the actual workflow resolver with synthetic refs and a
        stubbed git. Reproduces the prior branch-publication acceptance and
        requires rejection."""
        script = (ROOT / 'packaging/resolve-release.sh').read_text()
        version = control_version(read('crates/extension/pgokf.control'))

        def run(git_head, git_tag_sha, ref, release_tag):
            stub = (f'git() {{\n'
                    f'  if [ "$1" = diff ]; then return 0; fi\n'
                    f'  if [ "$1" = rev-parse ] && [ "$2" = HEAD ]; then echo "{git_head}"; return; fi\n'
                    f'  if [ "$1" = rev-parse ]; then echo "{git_tag_sha}"; return; fi\n'
                    f'  return 1\n'
                    f'}}\n')
            with tempfile.TemporaryDirectory() as work:
                env = {**os.environ, 'GITHUB_REF': ref, 'RELEASE_TAG_INPUT': release_tag,
                       'GITHUB_OUTPUT': work + '/out'}
                result = subprocess.run(['bash', '-e', '-o', 'pipefail', '-c', stub + script],
                                        env=env, cwd=ROOT, text=True, capture_output=True)
                out = Path(work + '/out').read_text() if Path(work + '/out').exists() else ''
                return result.returncode, out

        sha_a, sha_b = 'a' * 40, 'b' * 40
        # The prior defect's exact probe: branch source may never publish.
        code, out = run(sha_a, sha_a, 'refs/heads/main', '')
        self.assertEqual((code, 'publish=false' in out), (0, True), 'branch without tag must not publish')
        # Matching tag event on the tag's own commit publishes.
        code, out = run(sha_a, sha_a, f'refs/tags/v{version}', '')
        self.assertEqual((code, 'publish=true' in out), (0, True), 'matching tag must publish')
        self.assertIn('source_ref=' + sha_a, out, 'jobs must build the proven SHA')
        # Tag event where HEAD is not the tag's commit refuses.
        code, _ = run(sha_b, sha_a, f'refs/tags/v{version}', '')
        self.assertNotEqual(code, 0, 'HEAD != tag commit must fail closed')
        # Mismatched tags refuse.
        for bad in ('v0.2.0', 'v0.3.0', f'v{version}-dev3'):
            code, _ = run(sha_a, sha_a, bad if bad.startswith('v') else bad, '')
            code, _ = run(sha_a, sha_a, f'refs/tags/{bad}', '')
            self.assertNotEqual(code, 0, f'{bad} must fail closed')
        # Current-release dispatch on the exact immutable tag publishes.
        code, out = run(sha_a, sha_a, 'refs/heads/main', f'v{version}')
        self.assertEqual((code, 'publish=true' in out), (0, True),
                         'explicit current tag must publish')
        # Catch-up naming another version, a non-tag, or a tag whose commit
        # is not checked out refuses.
        for bad_tag, head, tag_sha in (('v0.2.0', sha_a, sha_a), ('release', sha_a, sha_a),
                                       (f'v{version}', sha_b, sha_a)):
            code, _ = run(head, tag_sha, 'refs/heads/main', bad_tag)
            self.assertNotEqual(code, 0, f'dispatch {bad_tag} with HEAD/tag mismatch must fail closed')


class RealGitPublication(unittest.TestCase):
    """Tag fixtures exist only in a temporary repository, never the checkout."""
    def test_tag_dispatch_and_identity(self):
        import shutil
        with tempfile.TemporaryDirectory() as work:
            root = Path(work)
            for name in ('Cargo.toml', 'Cargo.lock', 'META.json', 'crates/extension/pgokf.control'):
                target = root / name
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(ROOT / name, target)
            for path in ROOT.glob('crates/*/Cargo.toml'):
                target = root / path.relative_to(ROOT)
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(path, target)
            def git(*args):
                return subprocess.check_output(['git', *args], cwd=root, text=True).strip()
            git('init', '-q')
            git('config', 'user.email', 'fixture@example.invalid')
            git('config', 'user.name', 'Release fixture')
            originals = {p: p.read_text() for p in root.rglob('*') if p.is_file() and '.git' not in p.parts}
            for p, text in originals.items():
                p.write_text(text.replace('0.3.1', '0.2.0'))
            git('add', '.')
            git('commit', '-qm', 'Historical fixture')
            git('tag', 'v0.2.0')
            for p, text in originals.items():
                p.write_text(text)
            git('add', '.')
            git('commit', '-qm', 'Current fixture')
            git('tag', '-a', 'v0.3.1', '-m', 'Annotated fixture')
            current = git('rev-parse', 'HEAD')
            def resolve(ref, tag='', publish_input='true'):
                output = root / 'output'
                output.unlink(missing_ok=True)
                env = {**os.environ, 'GITHUB_REF': ref, 'RELEASE_TAG_INPUT': tag,
                       'PUBLISH_INPUT': publish_input, 'GITHUB_OUTPUT': str(output)}
                result = subprocess.run(['bash', str(ROOT / 'packaging/resolve-release.sh')],
                                        cwd=root, env=env, text=True, capture_output=True)
                return result.returncode, output.read_text() if output.exists() else ''
            self.assertIn('publish=false', resolve('refs/heads/main')[1])
            code, output = resolve('refs/tags/v0.3.1')
            self.assertEqual(code, 0)
            self.assertIn('source_ref=' + current, output)
            self.assertIn('publish=true', output)
            self.assertNotEqual(resolve('refs/tags/v0.2.0')[0], 0)
            self.assertNotEqual(resolve('refs/heads/main', 'v0.2.0')[0], 0)
            git('checkout', '-q', 'v0.2.0')
            code, output = resolve('refs/heads/main', 'v0.2.0')
            self.assertNotEqual(code, 0)
            self.assertNotIn('publish=true', output)
            self.assertNotEqual(resolve('refs/tags/v0.2.0')[0], 0)
            git('checkout', '-q', 'v0.3.1')
            for file in ('Cargo.toml', 'Cargo.lock', 'META.json', 'crates/extension/Cargo.toml'):
                path = root / file
                original = path.read_text()
                path.write_text(original.replace('0.3.1', '0.2.0'))
                self.assertNotEqual(resolve('refs/tags/v0.3.1')[0], 0, file)
                path.write_text(original)


if __name__ == '__main__':
    unittest.main()
