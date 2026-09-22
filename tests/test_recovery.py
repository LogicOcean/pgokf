#!/usr/bin/env python3
"""Recovery invariants: intentionally require review to change the GA boundary."""
import copy
from workflow_policy import effective_matrix, check_shell_policy
from pathlib import Path
import unittest
import yaml
from test_release_integrity import check_homebrew, KNOWN_RELEASE_DIGESTS
ROOT = Path(__file__).resolve().parents[1]


def check_support(packages, pgrx):
    errors = check_shell_policy(packages) + check_shell_policy(pgrx)
    for name in ('deb', 'docker', 'docker-manifest'):
        matrix = packages['jobs'][name]['strategy']['matrix']
        if (matrix['pg'] != [15, 16, 17, 18]
                or any('pg' in row for row in matrix.get('include', []))
                or matrix.get('exclude')):
            errors.append('stable GA boundary: ' + name)
    matrix = pgrx['jobs']['test']['strategy']['matrix']
    if matrix.get('pg') != [15, 16, 17, 18] or matrix.get('include') != [{'pg': 19, 'image_tag': '19beta3'}]:
        errors.append('required pinned beta compatibility')
    if effective_matrix(matrix) != [{'pg': pg} for pg in (15, 16, 17, 18)] + [{'pg': 19, 'image_tag': '19beta3'}]:
        errors.append('effective compatibility coverage changed')
    job = pgrx['jobs']['test']
    if job.get('continue-on-error') or 'if' in job or 'defaults' in job:
        errors.append('compatibility must fail closed')
    runs = '\n'.join(step.get('run', '') for step in job['steps'])
    for required in ('packaging/install-postgres.sh', 'packaging/check-beta-image.sh',
                     'cargo clippy --locked -p pgokf', 'cargo pgrx test pg${{ matrix.pg }}',
                     'python3 release-tools/scripts/compatibility-image.py',
                     'SMOKE_WITH_OPTIONAL=0 release-tools/packaging/docker/smoke-test.sh'):
        if required not in runs:
            errors.append('missing compatibility execution: ' + required)
        for step in job['steps']:
            if required not in step.get('run', ''):
                continue
            beta_only = required in ('packaging/check-beta-image.sh',
                                     'python3 release-tools/scripts/compatibility-image.py',
                                     'SMOKE_WITH_OPTIONAL=0 release-tools/packaging/docker/smoke-test.sh')
            allowed = ('matrix.pg == 19',) if beta_only else (None, "steps.pgdg.outputs.available == 'true'")
            if step.get('if') not in allowed or step.get('continue-on-error'):
                errors.append('compatibility execution skips or waives failure: ' + required)

    contracts = {
        'Nonproduction PostgreSQL 19 Beta 3 image compatibility': ('matrix.pg == 19', 'python3 release-tools/scripts/compatibility-image.py\nversion=$(sed -n "s/^default_version *= *\'\\([^\']*\\)\'.*/\\1/p" crates/extension/pgokf.control)\nSMOKE_WITH_OPTIONAL=0 release-tools/packaging/docker/smoke-test.sh pgokf-beta-compat "$version"'),
        'Validate source compatibility identity': (None, 'python3 release-tools/scripts/compatibility-source.py .'),
        'Verify immutable Beta 3 base': ('matrix.pg == 19', 'release-tools/packaging/check-beta-image.sh'),
        'Install PostgreSQL ${{ matrix.pg }} development files': (None, 'sudo release-tools/packaging/install-postgres.sh ${{ matrix.pg }}\necho "available=true" >> "$GITHUB_OUTPUT"'),
        'Clippy': ("steps.pgdg.outputs.available == 'true'", 'cargo clippy --locked -p pgokf --no-default-features --features pg${{ matrix.pg }} --all-targets -- -D warnings'),
        'Test': ("steps.pgdg.outputs.available == 'true'", 'cargo pgrx test pg${{ matrix.pg }} --release --no-default-features --features pg${{ matrix.pg }}'),
        'Fresh install versus upgrade parity and mutation probes': ("steps.pgdg.outputs.available == 'true' && matrix.pg == 18", 'PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config\ncargo pgrx install --no-default-features --features pg18 --pg-config "$PG_CONFIG"\nrelease-tools/scripts/compatibility-parity.sh "$PG_CONFIG"'),
    }
    for name, (condition, command) in contracts.items():
        matches = [s for s in job['steps'] if s.get('name') == name]
        if len(matches) != 1 or matches[0].get('if') != condition or matches[0].get('run', '').strip() != command or matches[0].get('continue-on-error') or 'shell' in matches[0]:
            errors.append('active execution contract: ' + name)
    compatibility = packages['jobs'].get('compatibility', {})
    if 'if' in compatibility or compatibility.get('continue-on-error') or compatibility.get('with') != {'source_ref': '${{ needs.prep.outputs.source_ref }}'}:
        errors.append('compatibility source/dependency bypass')
    if packages['jobs'].get('compatibility', {}).get('uses') != './.github/workflows/pgrx-test.yml':
        errors.append('publication requires compatibility')
    for name in ('docker', 'companions'):
        if not set(('prep', 'lint', 'meta', 'deb', 'compatibility', 'homebrew-policy')) <= set(packages['jobs'][name]['needs']):
            errors.append('publication gate: ' + name)
    for name in ('docker-manifest', 'companions-manifest'):
        if not {'docker', 'companions'} <= set(packages['jobs'][name]['needs']):
            errors.append('all builds before release tags: ' + name)
    return errors


class Recovery(unittest.TestCase):
    def workflows(self):
        return [yaml.safe_load((ROOT / '.github/workflows' / f).read_text()) for f in ('packages.yml', 'pgrx-test.yml')]

    def test_support_boundary(self):
        self.assertEqual(check_support(*self.workflows()), [])

    def test_boundary_mutations(self):
        packages, pgrx = self.workflows()
        for name in ('deb', 'docker', 'docker-manifest'):
            bad = copy.deepcopy(packages)
            bad['jobs'][name]['strategy']['matrix']['pg'].append(19)
            self.assertTrue(check_support(bad, pgrx))
            bad = copy.deepcopy(packages)
            bad['jobs'][name]['strategy']['matrix'].setdefault('include', []).append(
                {'pg': 19, 'arch': 'amd64', 'runner': 'ubuntu-24.04'})
            self.assertTrue(check_support(bad, pgrx), 'include must not bypass stable boundary')
        for field, value in [('include', []), ('include', [{'pg': 19, 'image_tag': '19'}])]:
            bad = copy.deepcopy(pgrx)
            bad['jobs']['test']['strategy']['matrix'][field] = value
            self.assertTrue(check_support(packages, bad))

    def test_effective_matrix_yaml_forms(self):
        packages, pgrx = self.workflows()
        for value in ('[{pg: 18}]', '- pg: 18', '[{pg: 15}, {pg: 16}, {pg: 17}, {pg: 18}]', '[&leg {pg: 18}, *leg]'):
            bad = copy.deepcopy(pgrx)
            bad['jobs']['test']['strategy']['matrix']['exclude'] = yaml.safe_load(value)
            self.assertTrue(check_support(packages, bad), value)
        # Includes run after exclusions; an explicit included Beta job survives
        # an exclusion of the same major from the base Cartesian expansion.
        matrix = {'pg': [18], 'arch': ['amd64', 'arm64'],
                  'exclude': [{'arch': 'arm64'}],
                  'include': [{'pg': 18, 'runner': 'linux'}, {'pg': 19, 'image_tag': '19beta3'}]}
        self.assertEqual(effective_matrix(matrix), [
            {'pg': 18, 'arch': 'amd64', 'runner': 'linux'}, {'pg': 19, 'image_tag': '19beta3'}])

    def test_publication_gate_mutations(self):
        packages, pgrx = self.workflows()
        for job in ('docker', 'companions', 'docker-manifest', 'companions-manifest'):
            for dependency in packages['jobs'][job]['needs']:
                bad = copy.deepcopy(packages)
                bad['jobs'][job]['needs'].remove(dependency)
                if dependency != 'prep' or job in ('docker', 'companions'):
                    self.assertTrue(check_support(bad, pgrx), (job, dependency))
        bad = copy.deepcopy(pgrx)
        bad['jobs']['test']['steps'] = []
        self.assertTrue(check_support(packages, bad))
        for field, value in [('if', 'false'), ('continue-on-error', True)]:
            bad = copy.deepcopy(pgrx)
            step = next(s for s in bad['jobs']['test']['steps'] if s.get('name') == 'Test')
            step[field] = value
            self.assertTrue(check_support(packages, bad), 'beta tests cannot silently skip or waive failure')


    def test_version_and_upgrade_edge(self):
        self.assertIn("default_version = '0.3.1'", (ROOT / 'crates/extension/pgokf.control').read_text())
        self.assertTrue((ROOT / 'crates/extension/sql/pgokf--0.3.0--0.3.1.sql').is_file())
        self.assertTrue((ROOT / 'crates/extension/sql/pgokf--0.3.1.sql').is_file())

    def test_unpublished_homebrew_even_if_digest_registered(self):
        formula = (ROOT / 'packaging/homebrew/pgokf.rb').read_text()
        self.assertIn('v0.2.0.tar.gz', formula)
        self.assertNotIn('0.3.0', KNOWN_RELEASE_DIGESTS)
        bad = formula.replace('0.2.0', '0.3.0')
        KNOWN_RELEASE_DIGESTS['0.3.0'] = KNOWN_RELEASE_DIGESTS['0.2.0']
        try:
            self.assertTrue(check_homebrew(bad, '0.3.1'))
        finally:
            del KNOWN_RELEASE_DIGESTS['0.3.0']

    def test_stable_deb_rejects_beta(self):
        import subprocess
        result = subprocess.run(['bash', 'packaging/deb/build-deb.sh', '19'], cwd=ROOT, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('unsupported PG_MAJOR', result.stderr)

    def test_linux_doc_regression(self):
        self.assertIn('architecture-specific: `x86_64`', (ROOT / 'crates/extension/src/catalog/export.rs').read_text())

    def test_image_feature_separation(self):
        text = (ROOT / 'packaging/docker/Dockerfile').read_text()
        self.assertIn('ARG PG_IMAGE_TAG=${PG_MAJOR}', text)
        self.assertEqual(text.count('FROM postgres:${PG_IMAGE_TAG}'), 2)
        self.assertIn('verify-postgres.sh', text)

if __name__ == '__main__':
    unittest.main()
