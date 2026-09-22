#!/usr/bin/env python3
"""Recovery invariants: intentionally require review to change the GA boundary."""
import copy
from pathlib import Path
import unittest
import yaml
from test_release_integrity import check_homebrew, KNOWN_RELEASE_DIGESTS
ROOT = Path(__file__).resolve().parents[1]


def check_support(packages, pgrx):
    errors = []
    for name in ('deb', 'docker', 'docker-manifest'):
        matrix = packages['jobs'][name]['strategy']['matrix']
        if (matrix['pg'] != [15, 16, 17, 18]
                or any('pg' in row for row in matrix.get('include', []))
                or matrix.get('exclude')):
            errors.append('stable GA boundary: ' + name)
    matrix = pgrx['jobs']['test']['strategy']['matrix']
    if matrix.get('pg') != [15, 16, 17, 18] or matrix.get('include') != [{'pg': 19, 'image_tag': '19beta3'}]:
        errors.append('required pinned beta compatibility')
    job = pgrx['jobs']['test']
    if job.get('continue-on-error') or job.get('if'):
        errors.append('compatibility must fail closed')
    runs = '\n'.join(step.get('run', '') for step in job['steps'])
    for required in ('packaging/install-postgres.sh', 'packaging/check-beta-image.sh',
                     'cargo clippy --locked -p pgokf', 'cargo pgrx test pg${{ matrix.pg }}',
                     'PG_IMAGE_TAG=${{ matrix.image_tag }}',
                     'SMOKE_WITH_OPTIONAL=0 packaging/docker/smoke-test.sh'):
        if required not in runs:
            errors.append('missing compatibility execution: ' + required)
        for step in job['steps']:
            if required not in step.get('run', ''):
                continue
            beta_only = required in ('packaging/check-beta-image.sh',
                                     'PG_IMAGE_TAG=${{ matrix.image_tag }}',
                                     'SMOKE_WITH_OPTIONAL=0 packaging/docker/smoke-test.sh')
            allowed = ('matrix.pg == 19',) if beta_only else (None, "steps.pgdg.outputs.available == 'true'")
            if step.get('if') not in allowed or step.get('continue-on-error'):
                errors.append('compatibility execution skips or waives failure: ' + required)

    if packages['jobs'].get('compatibility', {}).get('uses') != './.github/workflows/pgrx-test.yml':
        errors.append('publication requires compatibility')
    for name in ('docker', 'companions'):
        if not set(('prep', 'lint', 'meta', 'deb', 'compatibility')) <= set(packages['jobs'][name]['needs']):
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
