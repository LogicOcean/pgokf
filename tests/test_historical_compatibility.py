"""Real immutable Git source must never reach current publication jobs."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import yaml

ROOT = Path(__file__).resolve().parents[1]


class PublicationEligibility(unittest.TestCase):
    def test_historical_refusal_and_unreachable_artifacts(self):
        workflow = yaml.safe_load((ROOT / '.github/workflows/packages.yml').read_text())
        jobs = workflow['jobs']
        # Every artifact-capable job must be dominated by successful prep.
        # Explicit status functions could override the implicit success gate.
        def gated(name):
            if name == 'prep':
                return True
            job = jobs[name]
            self.assertNotIn('always()', str(job.get('if', '')))
            self.assertNotIn('failure()', str(job.get('if', '')))
            self.assertNotIn('cancelled()', str(job.get('if', '')))
            needs = job.get('needs', [])
            if isinstance(needs, str):
                needs = [needs]
            return any(gated(n) for n in needs)
        artifacts = [name for name, job in jobs.items() if any(
            'upload-artifact@' in step.get('uses', '') or
            'push' in str(step.get('with', {})) or
            'imagetools create' in step.get('run', '')
            for step in job.get('steps', []))]
        self.assertEqual(set(artifacts), {'deb', 'docker', 'companions', 'docker-manifest', 'companions-manifest'})
        self.assertTrue(all(gated(name) for name in artifacts))
        prep = jobs['prep']
        self.assertNotIn('continue-on-error', prep)
        resolve = next(s for s in prep['steps'] if s.get('id') == 'resolve')
        self.assertEqual(resolve['run'], '../release-tools/packaging/resolve-release.sh')
        self.assertNotIn('continue-on-error', resolve)
        self.assertNotIn('if', resolve)
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'source'
            subprocess.run(['git', 'clone', '--quiet', '--shared', '--no-checkout', str(ROOT), str(source)], check=True)
            for tag in ('v0.2.0', 'v0.3.0'):
                subprocess.run(['git', '-C', str(source), 'checkout', '--quiet', '--detach', tag], check=True)
                for ref, dispatch in ((f'refs/tags/{tag}', ''), ('refs/heads/main', tag), ('refs/heads/main', '')):
                    output = Path(directory) / 'output'
                    output.unlink(missing_ok=True)
                    result = subprocess.run(['bash', str(ROOT / 'packaging/resolve-release.sh')], cwd=source,
                        env={**os.environ, 'GITHUB_REF': ref, 'RELEASE_TAG_INPUT': dispatch, 'GITHUB_OUTPUT': str(output)}, capture_output=True, text=True)
                    self.assertNotEqual(result.returncode, 0, result.stdout)
                    self.assertNotIn('publish=true', output.read_text())
                    self.assertNotIn('source_ref=', output.read_text())
                    reachable = [name for name in artifacts if result.returncode == 0 or not gated(name)]
                    self.assertEqual(reachable, [])
                result = subprocess.run(['python3', str(ROOT / 'scripts/compatibility-source.py'), str(source)], capture_output=True)
                self.assertNotEqual(result.returncode, 0)

    def test_current_provision_failure_propagates_under_hostile_startup(self):
        import shlex
        from workflow_policy import CLEAN_SHELL
        workflow = yaml.safe_load((ROOT / '.github/workflows/pgrx-test.yml').read_text())
        steps = workflow['jobs']['test']['steps']
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            hook = root / 'startup'; hook.write_text("trap 'exit 0' EXIT\n")
            script = root / 'step.sh'
            provision = next(s for s in steps if s.get('id') == 'pgdg')['run'].replace('${{ matrix.pg }}', '18')
            beta = next(s for s in steps if s.get('name', '').startswith('Nonproduction'))['run']
            for body, stub in ((provision, 'sudo() { return 7; };\n'), (beta, 'python3() { return 7; };\n')):
                script.write_text(stub + body)
                env = {**os.environ, 'BASH_ENV': str(hook), 'ENV': str(hook), 'GITHUB_OUTPUT': str(root / 'output')}
                masked = subprocess.run(['bash', '-e', '-o', 'pipefail', str(script)], env=env, capture_output=True)
                self.assertEqual(masked.returncode, 0, 'probe must reproduce original startup masking')
                result = subprocess.run(shlex.split(CLEAN_SHELL.replace('{0}', str(script))), env=env, capture_output=True)
                self.assertEqual(result.returncode, 7)
                self.assertFalse((root / 'output').exists())


if __name__ == '__main__':
    unittest.main()
