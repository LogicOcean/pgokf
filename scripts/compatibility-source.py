#!/usr/bin/env python3
"""Validate immutable historical identity before selecting compatibility behavior."""
from pathlib import Path
import re
import subprocess
import sys

HISTORICAL = '59d29d3dc4b78cd5c5000bf473b0f097c9fb2ea4'


def validate(root):
    root = Path(root).resolve()
    version = re.search(r"default_version\s*=\s*'([^']+)'", (root / 'crates/extension/pgokf.control').read_text())[1]
    if version == '0.3.0':
        raise ValueError('retired 0.3.0 source forbidden')
    if version == '0.2.0':
        head = subprocess.check_output(['git', '-C', str(root), 'rev-parse', 'HEAD'], text=True).strip()
        if head != HISTORICAL:
            raise ValueError('historical exception requires exact immutable 0.2.0 commit')
        subprocess.run(['git', '-C', str(root), 'diff', '--exit-code', 'HEAD'], check=True)
    return version


if __name__ == '__main__':
    print(validate(sys.argv[1] if len(sys.argv) > 1 else '.'))
