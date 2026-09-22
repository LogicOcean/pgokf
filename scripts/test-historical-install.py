#!/usr/bin/env python3
"""Actual v0.2.0 fresh-install check; no invented upgrade routes for old source."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile

spec = importlib.util.spec_from_file_location('identity', Path(__file__).with_name('compatibility-source.py'))
identity = importlib.util.module_from_spec(spec)
spec.loader.exec_module(identity)


def run(*args):
    return subprocess.check_output(args, text=True).strip()


def main():
    root, pg_config = Path(sys.argv[1]).resolve(), sys.argv[2]
    if identity.validate(root) != '0.2.0':
        raise ValueError('historical check cannot replace current parity')
    installed = Path(run(pg_config, '--sharedir')) / 'extension'
    if (installed / 'pgokf.control').read_bytes() != (root / 'crates/extension/pgokf.control').read_bytes():
        raise ValueError('installed source identity differs')
    binaries = Path(run(pg_config, '--bindir'))
    with tempfile.TemporaryDirectory(prefix='pgokf-historical-') as work:
        work = Path(work)
        run(str(binaries / 'initdb'), '-D', str(work / 'data'), '-U', 'postgres', '--auth=trust', '--locale=C', '--encoding=UTF8')
        try:
            run(str(binaries / 'pg_ctl'), '-D', str(work / 'data'), '-l', str(work / 'postgres.log'), '-w', '-o', f"-c listen_addresses='' -k {work}", 'start')
            result = run(str(binaries / 'psql'), '-h', str(work), '-U', 'postgres', '-d', 'postgres', '-v', 'ON_ERROR_STOP=1', '-tAc', "CREATE EXTENSION pgokf; SELECT extversion FROM pg_extension WHERE extname='pgokf';")
            if result.splitlines()[-1] != '0.2.0':
                raise ValueError('historical extension version differs')
        finally:
            if (work / 'data/postmaster.pid').exists():
                run(str(binaries / 'pg_ctl'), '-D', str(work / 'data'), '-w', 'stop')
    print('PASS: immutable 0.2.0 actual fresh install')


if __name__ == '__main__':
    main()
