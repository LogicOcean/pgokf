#!/usr/bin/env python3
"""Portable structural gate; live arm64 gate: scripts/test-homebrew-packaging.py."""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


def check_policy(text):
    required = [
        'if OS.mac?',
        'ENV["MACOSX_DEPLOYMENT_TARGET"] = MacOS.version.to_s',
        'if ENV.key?("CARGO_ENCODED_RUSTFLAGS")',
        'ENV.append "CARGO_ENCODED_RUSTFLAGS", "-Clink-arg=-Wl,-undefined,dynamic_lookup", "\\x1f"',
        'ENV.append "RUSTFLAGS", "-C link-arg=-Wl,-undefined,dynamic_lookup"',
        'ENV["PGRX_HOME"] = buildpath/"pgrx-home"',
        'system cargo_pgrx, "pgrx", "package"',
    ]
    return [line for line in required if line not in text]


class HomebrewPackaging(unittest.TestCase):
    def test_formula_policy_and_mutations(self):
        text = (ROOT / 'packaging/homebrew/pgokf.rb').read_text()
        self.assertEqual(check_policy(text), [])
        for line in (
            'ENV["MACOSX_DEPLOYMENT_TARGET"] = MacOS.version.to_s',
            'ENV.append "RUSTFLAGS", "-C link-arg=-Wl,-undefined,dynamic_lookup"',
            'ENV.append "CARGO_ENCODED_RUSTFLAGS", "-Clink-arg=-Wl,-undefined,dynamic_lookup", "\\x1f"',
        ):
            self.assertTrue(check_policy(text.replace(line, '')), line)
        self.assertLess(text.index('MacOS.version.to_s'), text.index('system "cargo"'))
        self.assertNotIn('ENV["HOMEBREW_RUSTFLAGS"] =', text)
        self.assertNotIn('ENV["RUSTFLAGS"] =', text)

    def test_portable_guards_are_release_gates(self):
        for workflow in ('ci.yml', 'packages.yml'):
            text = (ROOT / '.github/workflows' / workflow).read_text()
            for guard in ('test_homebrew_packaging.py', 'test_cleanup_ownership.py'):
                self.assertIn('python3 tests/' + guard, text)

    def test_live_gate_cannot_fall_back_to_workspace_wrapper(self):
        text = (ROOT / 'scripts/test-homebrew-packaging.py').read_text()
        self.assertIn("'install', '--build-from-source'", text)
        self.assertIn("'audit', '--strict'", text)
        self.assertIn("'test', '--verbose', probe", text)
        self.assertNotIn('test-workspace.sh', text)
        self.assertIn("'diff', '--exit-code', 'HEAD'", text)


if __name__ == '__main__':
    unittest.main()
