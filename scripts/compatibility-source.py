#!/usr/bin/env python3
"""Refuse historical and retired sources in current compatibility machinery."""
from pathlib import Path
import re
import sys


def validate(root):
    root = Path(root).resolve()
    version = re.search(r"default_version\s*=\s*'([^']+)'", (root / 'crates/extension/pgokf.control').read_text())[1]
    if version != '0.3.1':
        raise ValueError('only current release 0.3.1 is eligible; historical/retired source forbidden')
    return version


if __name__ == '__main__':
    print(validate(sys.argv[1] if len(sys.argv) > 1 else '.'))
