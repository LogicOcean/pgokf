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
    def test_pg19_provisioning_fail_closed(self):
        script = (ROOT / 'packaging/install-postgres.sh').read_text()
        for scenario, candidate, expected in (
                ('ok', '19~beta3-1.pgdg24.04+1', 0),
                ('missing', '(none)', 1), ('drift', '19~beta4-1', 1),
                ('ga', '19.0-1', 1), ('install-failure', '19~beta3-1', 1)):
            with self.subTest(scenario=scenario), tempfile.TemporaryDirectory() as work:
                root = Path(work)
                (root / 'os-release').write_text('VERSION_CODENAME=noble\n')
                # Redirect only OS paths into the fixture; execute the actual logic.
                body = script.replace('/etc/os-release', str(root / 'os-release'))
                body = body.replace('/etc/apt/sources.list.d/pgdg.list', str(root / 'pgdg.list'))
                body = body.replace('/usr/share/postgresql-common/pgdg', str(root / 'keys'))
                (root / 'verify-postgres.sh').write_text((ROOT / 'packaging/verify-postgres.sh').read_text())
                (root / 'verify-postgres.sh').chmod(0o755)
                (root / 'install.sh').write_text(body)
                stub = """
                curl() { :; }; gpg() { cat >/dev/null; }
                apt-cache() { echo "  Candidate: $CANDIDATE"; }
                dpkg-query() { echo "$CANDIDATE"; }
                apt-get() { [[ "$1" != install || "$SCENARIO" != install-failure ]]; }
                export -f curl gpg apt-cache dpkg-query apt-get
                """
                result = subprocess.run(['bash', '-c', stub + 'bash "$1" 19', '_', str(root / 'install.sh')],
                    env={**os.environ, 'SCENARIO': scenario, 'CANDIDATE': candidate}, text=True, capture_output=True)
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertIn('noble-pgdg main 19', (root / 'pgdg.list').read_text())

    def test_stable_and_beta_are_required(self):
        from test_recovery import check_support
        packages = yaml.safe_load((ROOT / '.github/workflows/packages.yml').read_text())
        pgrx = yaml.safe_load((ROOT / '.github/workflows/pgrx-test.yml').read_text())
        self.assertEqual(check_support(packages, pgrx), [])

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
