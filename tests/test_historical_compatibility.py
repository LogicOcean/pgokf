"""Execute current workflow tooling from a disposable immutable historical checkout."""
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import yaml

ROOT = Path(__file__).resolve().parents[1]
HISTORICAL = '59d29d3dc4b78cd5c5000bf473b0f097c9fb2ea4'


class HistoricalCompatibility(unittest.TestCase):
    def test_checkout_and_dependency_flow(self):
        workflow = yaml.safe_load((ROOT / '.github/workflows/pgrx-test.yml').read_text())
        job = workflow['jobs']['test']
        steps = job['steps']
        checkouts = [s for s in steps if s.get('uses', '').startswith('actions/checkout@')]
        self.assertEqual([s['with'] for s in checkouts], [
            {'ref': '${{ inputs.source_ref || github.sha }}'},
            {'ref': '${{ github.sha }}', 'path': 'release-tools'}])
        self.assertNotIn('if', job)
        self.assertNotIn('continue-on-error', job)
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'source'
            subprocess.run(['git', 'clone', '--quiet', '--shared', '--no-checkout', str(ROOT), str(source)], check=True)
            subprocess.run(['git', '-C', str(source), 'checkout', '--quiet', '--detach', HISTORICAL], check=True)
            tools = source / 'release-tools'
            shutil.copytree(ROOT / 'scripts', tools / 'scripts')
            shutil.copytree(ROOT / 'packaging', tools / 'packaging')
            self.assertFalse((source / 'packaging/install-postgres.sh').exists())
            self.assertFalse((source / 'scripts/upgrade-parity.py').exists())
            result = subprocess.run(['python3', 'release-tools/scripts/compatibility-source.py', '.'], cwd=source, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), '0.2.0')
            # Resolve the actual historical checkout, then execute workflow commands
            # with external provisioning boundaries isolated. No fabricated SQL pass.
            env = {**os.environ, 'GITHUB_REF': 'refs/heads/main', 'RELEASE_TAG_INPUT': 'v0.2.0', 'GITHUB_OUTPUT': str(Path(directory) / 'output')}
            subprocess.run(['bash', 'release-tools/packaging/resolve-release.sh'], cwd=source, env=env, check=True, capture_output=True)
            self.assertIn('source_ref=' + HISTORICAL, Path(env['GITHUB_OUTPUT']).read_text())
            for major in (15, 16, 17, 18):
                provision = next(s for s in steps if s.get('id') == 'pgdg')
                self.assertNotIn('if', provision)
                self.assertNotIn('continue-on-error', provision)
                command = provision['run'].replace('${{ matrix.pg }}', str(major))
                # sudo boundary checks executable + syntax of the actual script,
                # returns a controlled package-manager failure when requested.
                stub = 'sudo() { test -x "$1" && bash -n "$1" && return "$STATUS"; }; export -f sudo;\n'
                for status in ('0', '7'):
                    output = Path(directory) / f'pg{major}-{status}'
                    result = subprocess.run(['bash', '-e', '-c', stub + command], cwd=source,
                        env={**env, 'GITHUB_OUTPUT': str(output), 'STATUS': status}, capture_output=True, text=True)
                    self.assertEqual(result.returncode, int(status), result.stderr)
                    self.assertEqual(output.exists(), status == '0', 'failed provisioning must not enable downstream tests')
            for path in ('packaging/check-beta-image.sh', 'scripts/compatibility-parity.sh', 'scripts/test-historical-install.py',
                         'scripts/compatibility-image.py', 'packaging/docker/fetch-pg-textsearch.sh'):
                self.assertTrue((tools / path).is_file(), path)
            # Real dispatcher reaches the historical checker and fails on invalid
            # pg_config instead of succeeding or reading missing historical tooling.
            result = subprocess.run(['bash', 'release-tools/scripts/compatibility-parity.sh', '/missing-pg-config'], cwd=source, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('/missing-pg-config', result.stderr)
            self.assertNotIn("can't open file", result.stderr)
            # A source mutation cannot borrow the historical exception.
            with (source / 'Cargo.toml').open('a') as stream: stream.write('\n# changed\n')
            result = subprocess.run(['python3', 'release-tools/scripts/compatibility-source.py', '.'], cwd=source, capture_output=True)
            self.assertNotEqual(result.returncode, 0)


if __name__ == '__main__':
    unittest.main()
