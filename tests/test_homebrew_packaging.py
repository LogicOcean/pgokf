#!/usr/bin/env python3
"""Parsed required job contracts plus executable macOS Homebrew API validation."""
import copy
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import yaml

ROOT = Path(__file__).resolve().parents[1]
GUARDS = ['python3 tests/test_homebrew_packaging.py', 'python3 tests/test_cleanup_ownership.py', 'python3 tests/test_historical_compatibility.py']


def check_workflow(workflow, release=False):
    errors = []
    jobs = workflow['jobs']
    job = jobs.get('homebrew-policy', {})
    if job.get('runs-on') != 'macos-15' or 'if' in job or job.get('continue-on-error', False):
        errors.append('required macOS job disabled')
    steps = job.get('steps', [])
    expected = [{'uses': 'actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1'},
                {'name': 'Execute actual Homebrew environment policy', 'run': 'python3 scripts/test-homebrew-policy.py'}]
    if release:
        expected[0].update(name='Checkout release validation tooling', **{'with': {'ref': '${{ github.sha }}', 'path': 'release-tools'}})
        expected[1]['run'] = 'python3 release-tools/scripts/test-homebrew-policy.py'
    if steps != expected:
        errors.append('Homebrew API execution contract changed')
    guard_job = jobs['lint' if release else 'rust']
    if 'if' in guard_job or guard_job.get('continue-on-error', False):
        errors.append('portable guards disabled')
    name = 'Portable source-packaging and cleanup guards' if release else 'Test required PostgreSQL coverage guards'
    matches = [s for s in guard_job['steps'] if s.get('name') == name]
    if len(matches) != 1:
        return errors + ['missing guard execution']
    step = matches[0]
    condition = "needs.prep.outputs.version != '0.2.0'" if release else None
    if step.get('if') != condition or step.get('continue-on-error', False) or 'shell' in step:
        errors.append('guard condition/failure propagation changed')
    # A small closed execution language: only plain python invocations, each on
    # its own line. No comments, dead branches, shell operators or exit masking.
    lines = step.get('run', '').strip().splitlines()
    allowed = GUARDS + ['python3 -m pip install PyYAML==6.0.3']
    if not release:
        allowed += ['python3 tests/' + f + '.py' for f in
                    ['test_postgres_pin', 'test_recovery', 'test_ci_coverage', 'test_release_integrity', 'test_release_tools']]
    if sorted(lines) != sorted(allowed):
        errors.append('required commands are not active direct execution')
    if release:
        for name in ('docker', 'companions'):
            j = jobs[name]
            if 'homebrew-policy' not in j['needs'] or 'if' in j or j.get('continue-on-error', False):
                errors.append('publication can bypass macOS policy')
        for name in ('prep', 'lint', 'meta', 'deb', 'compatibility'):
            if 'if' in jobs[name] or jobs[name].get('continue-on-error', False):
                errors.append('release prerequisite bypassed: ' + name)
    return errors


class HomebrewPackaging(unittest.TestCase):
    def test_formula_behavior(self):
        if sys.platform != 'darwin':
            self.skipTest('Real Homebrew API runs in required macOS job')
        subprocess.run([sys.executable, str(ROOT / 'scripts/test-homebrew-policy.py')], check=True)

    def test_portable_guards_are_release_gates(self):
        for filename in ('ci.yml', 'packages.yml'):
            workflow = yaml.safe_load((ROOT / '.github/workflows' / filename).read_text())
            self.assertEqual(check_workflow(workflow, filename == 'packages.yml'), [])
            for mutation in ('disabled-job', 'disabled-step', 'comment', 'dead-branch', 'mask', 'removed', 'condition'):
                bad = copy.deepcopy(workflow)
                job = bad['jobs']['homebrew-policy']
                step = job['steps'][-1]
                if mutation == 'disabled-job': job['if'] = False
                elif mutation == 'disabled-step': step['if'] = False
                elif mutation == 'comment': step['run'] = '# ' + step['run']
                elif mutation == 'dead-branch': step['run'] = 'if false; then ' + step['run'] + '; fi'
                elif mutation == 'mask': step['run'] += ' || true'
                elif mutation == 'removed': step['run'] = 'true'
                else: job['if'] = 'always()'
                self.assertTrue(check_workflow(bad, filename == 'packages.yml'), mutation)

    def test_live_gate_cannot_fall_back_to_workspace_wrapper(self):
        text = (ROOT / 'scripts/test-homebrew-packaging.py').read_text()
        for command in ("'install', '--build-from-source'", "'audit', '--strict'", "'test', '--verbose', probe", "'diff', '--exit-code', 'HEAD'"):
            self.assertIn(command, text)
        self.assertNotIn('test-workspace.sh', text)


if __name__ == '__main__':
    unittest.main()
