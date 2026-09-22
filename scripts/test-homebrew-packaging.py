#!/usr/bin/env python3
"""Build/test immutable local HEAD through the real formula on arm64 macOS.

Run with an empty evidence directory argument. Requires Homebrew rust/PG17.
Creates a private local fixture tap, never edits an existing tap. The production
formula tuple stays unchanged; only the probe's source tuple/name/assertions
change. This is not authentication of the future GitHub release archive.
"""
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import uuid

ROOT = Path(__file__).resolve().parents[1]


def main():
    if (platform.system(), platform.machine()) != ('Darwin', 'arm64'):
        raise SystemExit('Live gate requires arm64 macOS; run tests/test_homebrew_packaging.py elsewhere')
    out = Path(sys.argv[1]).resolve()
    out.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ, HOMEBREW_NO_AUTO_UPDATE='1', HOMEBREW_NO_INSTALL_CLEANUP='1')
    for key in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'MACOSX_DEPLOYMENT_TARGET',
                'CARGO_TARGET_DIR', 'PGRX_HOME', 'CARGO_HOME'):
        env.pop(key, None)

    def run(label, args, **kwargs):
        with (out / (label + '.log')).open('w') as log:
            result = subprocess.run(args, cwd=ROOT, env=env, stdout=log,
                                    stderr=subprocess.STDOUT, **kwargs)
        with (out / 'results.jsonl').open('a') as log:
            log.write(json.dumps(dict(label=label, command=args, code=result.returncode)) + '\n')
        result.check_returncode()

    run('clean', ['git', 'diff', '--exit-code', 'HEAD'])
    sha = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    archive = out / f'pgokf-{sha}.tar.gz'
    run('archive', ['git', 'archive', '--format=tar.gz', '--prefix=pgokf-0.3.1/',
                    '-o', str(archive), sha])
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.chmod(0o444)
    token = uuid.uuid4().hex
    tap_name = f'pgokf-local/packaging-{token}'
    repo = Path(subprocess.check_output(['brew', '--repository'], text=True).strip())
    tap = repo / 'Library/Taps/pgokf-local' / f'homebrew-packaging-{token}'
    tap.mkdir(parents=True, exist_ok=False)
    formulas = tap / 'Formula'
    formulas.mkdir()
    original = (ROOT / 'packaging/homebrew/pgokf.rb').read_text()
    (formulas / 'pgokf.rb').write_text(original)
    probe_name = 'pgokf-packaging-probe'
    probe = f'{tap_name}/{probe_name}'
    # Restrict substitutions to the authenticated tuple, class and assertions.
    local = original.replace('class Pgokf < Formula', 'class PgokfPackagingProbe < Formula')
    local = local.replace('https://github.com/LogicOcean/pgokf/archive/refs/tags/v0.2.0.tar.gz', archive.as_uri())
    local = local.replace('194441d1b4d6bd5cf5f22a39a3b6c923e9a68d7692202ff7c4bb05109df812e5', digest)
    local = local.replace("default_version = '0.2.0'", "default_version = '0.3.1'")
    local = local.replace('assert_match "0.2.0", output', 'assert_match "0.3.1", output')
    (formulas / f'{probe_name}.rb').write_text(local)
    shutil.copytree(formulas, out / 'formulas')
    (out / 'identity.json').write_text(json.dumps(dict(sha=sha, sha256=digest,
                                                     archive=str(archive), tap=tap_name), indent=2))
    try:
        run('tap-init', ['git', '-C', str(tap), 'init', '-q'])
        run('environment', ['brew', 'ruby', str(ROOT / 'scripts/test-homebrew-environment.rb'), f'{tap_name}/pgokf'])
        if 'PASS: four actual formula environment composition cases' not in (out / 'environment.log').read_text():
            raise RuntimeError('formula environment composition failed')
        run('audit', ['brew', 'audit', '--strict', f'{tap_name}/pgokf'])
        run('install', ['brew', 'install', '--build-from-source', '--verbose', probe])
        run('test', ['brew', 'test', '--verbose', probe])
        if hashlib.sha256(archive.read_bytes()).hexdigest() != digest:
            raise RuntimeError('immutable source archive changed')
    finally:
        installed = subprocess.run(['brew', 'list', '--versions', probe], env=env,
                                   capture_output=True, text=True)
        if installed.stdout.strip():
            run('uninstall', ['brew', 'uninstall', '--force', probe])
        shutil.rmtree(tap)


if __name__ == '__main__':
    main()
