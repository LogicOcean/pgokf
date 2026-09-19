#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Execute PG19 provisioning failure paths without network/root (requires PyYAML)."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[1]


class RequiredCoverage(unittest.TestCase):
    def test_pg19_provisioning_cannot_pass_without_packages(self):
        for workflow in ('pgrx-test.yml', 'packages.yml'):
            jobs = yaml.safe_load((ROOT / '.github/workflows' / workflow).read_text())['jobs']
            for name, job in jobs.items():
                for step in job.get('steps', []):
                    if step.get('id') != 'pgdg':
                        continue
                    script = step['run'].replace('${{ matrix.pg }}', '19').replace('${{ matrix.experimental }}', 'true')
                    self.assertNotRegex(script, r'\$\{\{', 'unexpanded workflow expression')
                    for scenario in ('missing', 'install-failure'):
                        with self.subTest(workflow=workflow, job=name, scenario=scenario), tempfile.TemporaryDirectory() as work:
                            stub = '''
                            curl() { :; }
                            apt-cache() { [ "$SCENARIO" != missing ]; }
                            sudo() {
                                if [ "$1" = apt-get ] && [ "$2" = install ]; then return 1; fi
                                cat >/dev/null
                            }
                            '''
                            result = subprocess.run(['bash', '-e', '-o', 'pipefail', '-c', stub + script],
                                                    env={**os.environ, 'SCENARIO': scenario, 'GITHUB_OUTPUT': work + '/out'},
                                                    text=True, capture_output=True)
                            self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_pg19_is_required(self):
        for workflow in ('pgrx-test.yml', 'packages.yml'):
            jobs = yaml.safe_load((ROOT / '.github/workflows' / workflow).read_text())['jobs']
            required = ('test',) if workflow == 'pgrx-test.yml' else ('deb', 'docker', 'docker-manifest')
            for name in required:
                job = jobs[name]
                matrix = job.get('strategy', {}).get('matrix', {})
                majors = matrix.get('pg', []) + [row.get('pg') for row in matrix.get('include', [])]
                with self.subTest(workflow=workflow, job=name):
                    self.assertIn(19, majors)
                    self.assertFalse(job.get('continue-on-error', False))
                    self.assertFalse(any(row.get('experimental') for row in matrix.get('include', []) if row.get('pg') == 19))

    def test_pg19_missing_image_fails(self):
        jobs = yaml.safe_load((ROOT / '.github/workflows/packages.yml').read_text())['jobs']
        step = next(step for step in jobs['docker']['steps'] if step.get('id') == 'base')
        script = step['run'].replace('${{ matrix.pg }}', '19').replace('${{ matrix.experimental }}', 'true')
        with tempfile.TemporaryDirectory() as work:
            result = subprocess.run(['bash', '-e', '-c', 'docker() { return 1; };' + script],
                                    env={**os.environ, 'GITHUB_OUTPUT': work + '/out'},
                                    text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0, result.stdout)


if __name__ == '__main__':
    unittest.main()
