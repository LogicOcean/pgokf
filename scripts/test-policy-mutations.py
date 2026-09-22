#!/usr/bin/env python3
"""Run the full Python suite against isolated executable-policy mutations (macOS)."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
if sys.platform != 'darwin':
    raise SystemExit('formula mutation execution requires real macOS Homebrew')
out = Path(sys.argv[1]).resolve()
out.mkdir(parents=True, exist_ok=False)
formula = 'packaging/homebrew/pgokf.rb'
workflow = '.github/workflows/packages.yml'
mutations = [
    ('commented-deployment', formula, lambda s: s.replace('      ENV["MACOSX_DEPLOYMENT_TARGET"]', '      # ENV["MACOSX_DEPLOYMENT_TARGET"]')),
    ('commented-flags', formula, lambda s: s.replace('        ENV.append', '        # ENV.append')),
    ('dead-formula-branch', formula, lambda s: s.replace('if OS.mac?', 'if false')),
    ('disabled-release-step', workflow, lambda s: s.replace("if: needs.prep.outputs.version != '0.2.0'", "# if: needs.prep.outputs.version != '0.2.0'\n        if: false")),
    ('disabled-macos-job', workflow, lambda s: s.replace('  homebrew-policy:\n', '  homebrew-policy:\n    if: false\n')),
    ('disabled-macos-step', workflow, lambda s: s.replace('      - name: Execute actual Homebrew environment policy\n', '      - name: Execute actual Homebrew environment policy\n        if: false\n')),
    ('commented-execution', workflow, lambda s: s.replace('run: python3 release-tools/scripts/test-homebrew-policy.py', 'run: "# python3 release-tools/scripts/test-homebrew-policy.py"')),
    ('dead-execution', workflow, lambda s: s.replace('run: python3 release-tools/scripts/test-homebrew-policy.py', 'run: if false; then python3 release-tools/scripts/test-homebrew-policy.py; fi')),
    ('success-mask', workflow, lambda s: s.replace('run: python3 release-tools/scripts/test-homebrew-policy.py', 'run: python3 release-tools/scripts/test-homebrew-policy.py || true')),
    ('removed-execution', workflow, lambda s: s.replace('run: python3 release-tools/scripts/test-homebrew-policy.py', 'run: true')),
    ('bypassed-dependency', workflow, lambda s: s.replace(', homebrew-policy]', ']')),
    ('bypassed-condition', workflow, lambda s: s.replace("if: needs.prep.outputs.version != '0.2.0'", "if: needs.prep.outputs.version == '0.2.0'")),
    ('waived-failure', workflow, lambda s: s.replace('  homebrew-policy:\n', '  homebrew-policy:\n    continue-on-error: true\n')),
    ('commented-pgrx', '.github/workflows/pgrx-test.yml', lambda s: s.replace('run: cargo pgrx test', 'run: "# cargo pgrx test').replace('--features pg${{ matrix.pg }}\n\n      - name: Fresh', '--features pg${{ matrix.pg }}"\n\n      - name: Fresh')),
]
results = []
with tempfile.TemporaryDirectory(prefix='pgokf-policy-mutations-') as work:
    source = Path(work) / 'source'
    subprocess.run(['git', 'clone', '--quiet', '--shared', str(ROOT), str(source)], check=True)
    for name, relative, transform in mutations:
        path = source / relative
        original = path.read_text()
        try:
            changed = transform(original)
            assert changed != original, name
            path.write_text(changed)
            with (out / (name + '.log')).open('w') as log:
                result = subprocess.run([sys.executable, '-m', 'unittest', 'discover', '-s', 'tests', '-p', 'test_*.py', '-v'], cwd=source,
                    env={**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'}, stdout=log, stderr=subprocess.STDOUT)
            results.append(dict(name=name, code=result.returncode))
            print(name, result.returncode, flush=True)
            assert result.returncode != 0, name + ' escaped full suite'
        finally:
            path.write_text(original)
(out / 'results.json').write_text(json.dumps(results, indent=2))
