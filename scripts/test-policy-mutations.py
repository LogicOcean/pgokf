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
    ('removed-execution', workflow, lambda s: s.replace('run: python3 release-tools/scripts/test-homebrew-policy.py', 'run: "true"')),
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
    ('workflow-shell-override', pgrx, lambda s: s.replace('/usr/bin/env -u BASH_ENV -u ENV /bin/bash --noprofile --norc -e -o pipefail {0}', 'bash {0}')),
    ('test-success-mask', pgrx, lambda s: s.replace('run: cargo pgrx test pg${{ matrix.pg }} --release --no-default-features --features pg${{ matrix.pg }}', 'run: cargo pgrx test pg${{ matrix.pg }} --release --no-default-features --features pg${{ matrix.pg }} || true')),
]

# Shell selection regressions run the unchanged complete suite, not merely a
# helper predicate. A diagnostic match prevents incidental failures counting.
reasons = {}
def shell_case(name, relative, transform, reason):
    mutations.append((name, relative, transform))
    reasons[name] = reason

wrapper = '''      - name: Prepare shell wrapper
        run: |
          mkdir -p "$RUNNER_TEMP/review-shell"
          printf '#!/bin/sh\\n/bin/bash "$@"\\nexit 0\\n' > "$RUNNER_TEMP/review-shell/bash"
          chmod +x "$RUNNER_TEMP/review-shell/bash"
          echo "$RUNNER_TEMP/review-shell" >> "$GITHUB_PATH"

'''
for variant, body in (
    ('exact-wrapper', wrapper),
    ('symlink-wrapper', wrapper.replace('chmod +x', 'mv "$RUNNER_TEMP/review-shell/bash" "$RUNNER_TEMP/mask"\n          ln -s "$RUNNER_TEMP/mask" "$RUNNER_TEMP/review-shell/bash"\n          chmod +x')),
):
    shell_case(variant, pgrx, lambda s, b=body: s.replace('      - name: Clippy\n', b + '      - name: Clippy\n'), 'GITHUB_PATH manipulation')

for filename, job, step in (
    (pgrx, 'test', 'Clippy'),
    ('.github/workflows/ci.yml', 'homebrew-policy', 'Execute actual Homebrew environment policy'),
    (workflow, 'homebrew-policy', 'Execute actual Homebrew environment policy'),
):
    prefix = Path(filename).stem
    for scope in ('workflow', 'job', 'step'):
        def path_env(s, scope=scope, job=job, step=step):
            if scope == 'workflow':
                if '\nenv:\n' in s:
                    return s.replace('\nenv:\n', '\nenv:\n  PATH: /tmp/mask\n', 1)
                return s.replace('permissions:\n', 'env: {PATH: /tmp/mask}\n\npermissions:\n', 1)
            if scope == 'job':
                return s.replace('  ' + job + ':\n', '  ' + job + ':\n    env: {PATH: /tmp/mask}\n', 1)
            return s.replace('      - name: ' + step + '\n', '      - name: ' + step + '\n        env: {PATH: /tmp/mask}\n', 1)
        shell_case(prefix + '-path-' + scope, filename, path_env, 'PATH environment override')
    for name, new in (
        ('relative-shell', 'bash'), ('alternate-shell', '/tmp/bash'),
        ('symlink-shell', '/tmp/system-bash-link'),
    ):
        shell_case(prefix + '-' + name, filename,
                   lambda s, new=new: s.replace('ENV /bin/bash --noprofile', 'ENV ' + new + ' --noprofile'),
                   'explicit clean strict shell required')
    shell_case(prefix + '-removed-shell', filename,
               lambda s: s.replace('    shell: /usr/bin/env -u BASH_ENV -u ENV /bin/bash --noprofile --norc -e -o pipefail {0}', '    working-directory: .'),
               'explicit clean strict shell required')

for name, body, reason in (
    ('path-command', 'export PATH=/tmp/mask:$PATH', 'PATH command override'),
    ('github-path-plain', 'echo /tmp/mask >> "$GITHUB_PATH"', 'GITHUB_PATH manipulation'),
    ('github-path-braced', 'echo /tmp/mask >> "${GITHUB_PATH}"', 'GITHUB_PATH manipulation'),
    ('github-path-expression', "echo /tmp/mask >> '${{ github.path }}'", 'unreviewed shell-selection execution context'),
    ('github-path-env-expression', "echo /tmp/mask >> '${{ env.GITHUB_PATH }}'", 'GITHUB_PATH manipulation'),
    ('github-path-tee', 'echo /tmp/mask | tee -a "$GITHUB_PATH"', 'GITHUB_PATH manipulation'),
    ('github-path-python', '''python3 -c 'import os; open(os.environ["GITHUB_PATH"], "a").write("/tmp/mask\\n")' ''', 'GITHUB_PATH manipulation'),
    ('github-path-computed', '''key=GITHUB_; key+=PATH; echo /tmp/mask >> "${!key}"''', 'unreviewed shell-selection execution context'),
    ('github-path-split', '''echo /tmp/mask >> "$(printenv GITHUB_"PATH")"''', 'unreviewed shell-selection execution context'),
    ('github-path-output', 'echo /tmp/mask >> "${{ steps.setup.outputs.file }}"', 'unreviewed shell-selection execution context'),
    ('github-env-braced', 'echo PATH=/tmp/mask >> "${GITHUB_ENV}"', 'startup/environment-file manipulation'),
    ('github-env-expression', "echo PATH=/tmp/mask >> '${{ github.env }}'", 'unreviewed shell-selection execution context'),
    ('legacy-add-path', 'echo "::add-path::/tmp/mask"', 'GITHUB_PATH manipulation'),
):
    shell_case(name, pgrx, lambda s, b=body: s.replace('    steps:\n', '    steps:\n      - run: |\n          ' + b + '\n', 1), reason)

for name, fragment, reason in (
    ('path-env-alias', '      - run: "true"\n        env: &mask {PATH: /tmp/mask}\n      - run: "true"\n        env: *mask\n', 'PATH environment override'),
    ('path-env-merge', '      - run: "true"\n        env: &mask {PATH: /tmp/mask}\n      - run: "true"\n        env: {<<: *mask}\n', 'PATH environment override'),
    ('github-path-run-alias', '      - run: &write echo /tmp/mask >> "$GITHUB_PATH"\n      - run: *write\n', 'GITHUB_PATH manipulation'),
    ('github-path-env-alias', '      - run: echo /tmp/mask >> "$FILE"\n        env: {FILE: "${{ github.path }}"}\n', 'unreviewed shell-selection execution context'),
    ('action-path-indirection', '      - uses: actions/github-script@60a0d83039c74a4aee543508d2ffcb1c3799cdea\n        with:\n          script: core.addPath("/tmp/mask")\n', 'unreviewed shell-selection execution context'),
    ('local-action-indirection', '      - uses: ./mask-shell\n', 'unreviewed shell-selection execution context'),
):
    shell_case(name, pgrx, lambda s, f=fragment: s.replace('    steps:\n', '    steps:\n' + f, 1), reason)
shell_case('action-input-indirection', pgrx, lambda s: s.replace('toolchain: "1.96.0"', 'toolchain: "${{ steps.setup.outputs.toolchain }}"'), 'unreviewed shell-selection execution context')
shell_case('shell-anchor-replacement', pgrx, lambda s: s.replace('shell: /usr/bin/env -u BASH_ENV -u ENV /bin/bash --noprofile --norc -e -o pipefail {0}', 'shell: &shell bash {0}').replace('  test:\n', '  test:\n    defaults:\n      run:\n        shell: *shell\n'), 'explicit clean strict shell required')

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
            output = (out / (name + '.log')).read_text()
            reason = reasons.get(name)
            if reason:
                assert reason in output, name + ' failed for the wrong policy reason'
                assert 'FAILED (failures=' in output and 'ERROR:' not in output, name + ' did not reach policy assertions'
            elif relative == formula:
                assert 'ERROR: test_formula_behavior' in output and 'CalledProcessError' in output, name + ' missed the actual formula gate'
            else:
                assert 'FAILED (failures=' in output and 'ERROR:' not in output, name + ' did not reach policy assertions'
            results.append(dict(name=name, code=result.returncode, expected_reason=reason,
                                reason_matched=reason in output if reason else None))
            print(name, result.returncode, flush=True)
            assert result.returncode != 0, name + ' escaped full suite'
        finally:
            path.write_text(original)
(out / 'results.json').write_text(json.dumps(results, indent=2))
