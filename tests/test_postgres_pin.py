import os
from pathlib import Path
import subprocess
import unittest
ROOT = Path(__file__).resolve().parents[1]

class Pin(unittest.TestCase):
    def test_installed_versions(self):
        script = ROOT / 'packaging/verify-postgres.sh'
        for version, expected in [('19~beta3-1.pgdg13+1', 0), ('19~beta2-1.pgdg13+1', 1), ('19~rc1-1', 1), ('19.0-1', 1), ('', 1)]:
            with self.subTest(version=version):
                stub = 'dpkg-query() { echo "$TEST_VERSION"; }; export -f dpkg-query; '
                run = subprocess.run(['bash', '-c', stub + 'bash "$1" 19 19beta3', '_', str(script)], env={**os.environ, 'TEST_VERSION': version}, capture_output=True)
                self.assertEqual(run.returncode, expected, run.stderr)


class ImagePin(unittest.TestCase):
    def test_changed_or_missing_beta_image_fails(self):
        expected = 'sha256:f0056b553c58e4533ba81921d9bd5b49641d4fa176e1add956a678b034007ac4'
        for digest, code in [(expected, 0), ('sha256:' + 'a'*64, 1), ('', 1)]:
            stub = 'docker() { printf "%s\\n" "$TEST_DIGEST"; }; export -f docker; '
            run = subprocess.run(['bash', '-c', stub + 'bash "$1"', '_', str(ROOT/'packaging/check-beta-image.sh')], env={**os.environ, 'TEST_DIGEST':digest}, capture_output=True)
            self.assertEqual(run.returncode, code, run.stderr)

if __name__ == '__main__':
    unittest.main()
