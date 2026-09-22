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
    ('disabled-release-step', workflow, lambda s: s.replace("      - name: Portable source-packaging and cleanup guards\n", "      - name: Portable source-packaging and cleanup guards\n        if: false\n")),
    ('disabled-macos-job', workflow, lambda s: s.replace('  homebrew-policy:\n', '  homebrew-policy:\n    if: false\n')),
    ('disabled-macos-step', workflow, lambda s: s.replace('      - name: Execute actual Homebrew environment policy\n', '      - name: Execute actual Homebrew environment policy\n        if: false\n')),
    ('commented-execution', workflow, lambda s: s.replace('run: python3 release-tools/scripts/test-homebrew-policy.py', 'run: "# python3 release-tools/scripts/test-homebrew-policy.py"')),
    ('dead-execution', workflow, lambda s: s.replace('run: python3 release-tools/scripts/test-homebrew-policy.py', 'run: if false; then python3 release-tools/scripts/test-homebrew-policy.py; fi')),
    ('success-mask', workflow, lambda s: s.replace('run: python3 release-tools/scripts/test-homebrew-policy.py', 'run: python3 release-tools/scripts/test-homebrew-policy.py || true')),
    ('removed-execution', workflow, lambda s: s.replace('run: python3 release-tools/scripts/test-homebrew-policy.py', 'run: true')),
    ('bypassed-dependency', workflow, lambda s: s.replace(', homebrew-policy]', ']')),
    ('bypassed-condition', workflow, lambda s: s.replace("      - name: Portable source-packaging and cleanup guards\n", "      - name: Portable source-packaging and cleanup guards\n        if: always()\n")),
    ('manifest-bypass', workflow, lambda s: s.replace("if: needs.prep.outputs.publish == 'true'", 'if: always()')),
    ('masked-shell-default', '.github/workflows/pgrx-test.yml', lambda s: s.replace('  test:\n', '  test:\n    defaults:\n      run:\n        shell: bash {0}\n')),
    ('disabled-tooling-checkout', '.github/workflows/pgrx-test.yml', lambda s: s.replace('      - name: Checkout release validation tooling\n', '      - name: Checkout release validation tooling\n        if: false\n')),
    ('masked-beta-build', '.github/workflows/pgrx-test.yml', lambda s: s.replace('python3 release-tools/scripts/compatibility-image.py', 'python3 release-tools/scripts/compatibility-image.py || true')),
    ('waived-failure', workflow, lambda s: s.replace('  homebrew-policy:\n', '  homebrew-policy:\n    continue-on-error: true\n')),
    ('commented-pgrx', '.github/workflows/pgrx-test.yml', lambda s: s.replace('run: cargo pgrx test', 'run: "# cargo pgrx test').replace('--features pg${{ matrix.pg }}\n\n      - name: Fresh', '--features pg${{ matrix.pg }}"\n\n      - name: Fresh')),
]
pgrx = '.github/workflows/pgrx-test.yml'
for name, exclusion in (
    ('exclude-pg18-flow', '[{pg: 18}]'),
    ('exclude-pg18-block', '\n          - pg: 18'),
    ('exclude-multiple', '[{pg: 15}, {pg: 16}, {pg: 17}, {pg: 18}]'),
    ('exclude-alias', '[&required {pg: 18}, *required]'),
):
    mutations.append((name, pgrx, lambda s, e=exclusion: s.replace(
        '        pg: [15, 16, 17, 18]\n', '        pg: [15, 16, 17, 18]\n        exclude: ' + e + '\n')))
for scope in ('workflow', 'job', 'step'):
    def inject(s, scope=scope):
        if scope == 'workflow':
            s = s.replace('permissions:\n', 'env:\n  BASH_ENV: /tmp/pgokf-mask.sh\n\npermissions:\n')
        elif scope == 'job':
            s = s.replace('  test:\n', '  test:\n    env:\n      BASH_ENV: /tmp/pgokf-mask.sh\n')
        else:
            s = s.replace('      - name: Clippy\n', '      - name: Clippy\n        env:\n          BASH_ENV: /tmp/pgokf-mask.sh\n')
        return s.replace('    steps:\n', "    steps:\n      - run: echo \"trap 'exit 0' EXIT\" > /tmp/pgokf-mask.sh\n")
    mutations.append(('startup-trap-' + scope, pgrx, inject))
mutations += [
    ('github-env-startup', pgrx, lambda s: s.replace('    steps:\n', "    steps:\n      - run: |\n          echo \"trap 'exit 0' EXIT\" > /tmp/pgokf-mask.sh\n          echo BASH_ENV=/tmp/pgokf-mask.sh >> \"$GITHUB_ENV\"\n")),
    ('step-shell-override', pgrx, lambda s: s.replace('      - name: Clippy\n', '      - name: Clippy\n        shell: bash {0}\n')),
    ('workflow-shell-override', pgrx, lambda s: s.replace('/usr/bin/env -u BASH_ENV -u ENV bash --noprofile --norc -e -o pipefail {0}', 'bash {0}')),
    ('test-success-mask', pgrx, lambda s: s.replace('run: cargo pgrx test pg${{ matrix.pg }} --release --no-default-features --features pg${{ matrix.pg }}', 'run: cargo pgrx test pg${{ matrix.pg }} --release --no-default-features --features pg${{ matrix.pg }} || true')),
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
