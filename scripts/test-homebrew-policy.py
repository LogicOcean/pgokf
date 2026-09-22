#!/usr/bin/env python3
"""Execute the formula through Homebrew on macOS; no static-policy substitute."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import uuid

ROOT = Path(__file__).resolve().parents[1]


def check(formula):
    if sys.platform != 'darwin':
        raise RuntimeError('Homebrew API policy gate requires macOS')
    repo = Path(subprocess.check_output(['brew', '--repository'], text=True).strip())
    token = uuid.uuid4().hex
    tap_name = 'pgokf-local/policy-' + token
    tap = repo / 'Library/Taps/pgokf-local' / ('homebrew-policy-' + token)
    (tap / 'Formula').mkdir(parents=True)
    try:
        shutil.copyfile(formula, tap / 'Formula/pgokf.rb')
        subprocess.run(['git', '-C', str(tap), 'init', '-q'], check=True)
        subprocess.run(['brew', 'ruby', str(ROOT / 'scripts/test-homebrew-environment.rb'),
                        tap_name + '/pgokf'], check=True,
                       env={**os.environ, 'HOMEBREW_NO_AUTO_UPDATE': '1'})
    finally:
        shutil.rmtree(tap)


if __name__ == '__main__':
    check(Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / 'packaging/homebrew/pgokf.rb')
