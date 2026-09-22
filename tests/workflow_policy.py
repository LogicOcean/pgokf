"""Small semantic policy for matrix expansion and trusted shell startup."""
from itertools import product
import re
import hashlib
import json
from pathlib import Path

CLEAN_SHELL = '/usr/bin/env -u BASH_ENV -u ENV /bin/bash --noprofile --norc -e -o pipefail {0}'


def effective_matrix(matrix):
    """Actions excludes base combinations, then applies includes in order.

    Includes can augment original axis values but cannot overwrite them;
    unmatched includes create standalone jobs (including an excluded leg).
    """
    axes = {k: v for k, v in matrix.items() if k not in ('include', 'exclude')}
    original = [dict(zip(axes, values)) for values in product(*axes.values())] if axes else []
    original = [row for row in original if not any(
        all(row.get(k) == v for k, v in exclusion.items())
        for exclusion in matrix.get('exclude', []))]
    rows = [dict(row) for row in original]
    extra = []
    for inclusion in matrix.get('include', []):
        matched = False
        for base, row in zip(original, rows):
            if all(k not in base or base[k] == v for k, v in inclusion.items()):
                row.update(inclusion)
                matched = True
        if not matched:
            extra.append(dict(inclusion))
    return rows + extra


def execution_context(workflow):
    """Digest parsed job definitions, not YAML syntax (anchors resolve first).

    A closed, reviewed contract is necessary: arbitrary shell/action code can
    compute environment-file names or pass them through outputs. Token scans
    alone cannot prove it harmless. Pinned setup actions intentionally extend
    PATH; only their reviewed configuration is admitted. This does not sandbox
    those actions, called scripts, or the runner itself.
    """
    def digest(value):
        return hashlib.sha256(json.dumps(value, sort_keys=True,
                                         separators=(',', ':')).encode()).hexdigest()
    return {'env': digest(workflow.get('env', {})),
            'jobs': {name: digest(job) for name, job in workflow['jobs'].items()}}


def check_shell_policy(workflow):
    errors = []
    if workflow.get('defaults') != {'run': {'shell': CLEAN_SHELL}}:
        errors.append('explicit clean strict shell required')
    contracts = json.loads(Path(__file__).with_name('workflow-contexts.json').read_text())
    if execution_context(workflow) != contracts.get(workflow.get('name')):
        errors.append('unreviewed shell-selection execution context')
    # Environment-file writes can change later execution, including computed
    # hook names. None are needed in these workflows: outputs carry job data.
    hook = re.compile(r'\b(?:BASH_ENV|ENV|SHELLOPTS|BASHOPTS|GITHUB_ENV)\b|BASH_FUNC_')
    def inspect(scope):
        env = scope.get('env', {})
        if not isinstance(env, dict) or any(hook.search(str(k)) for k in env):
            errors.append('startup environment injection')
        if isinstance(env, dict) and 'PATH' in env:
            errors.append('PATH environment override')
        # Scan values as well: env aliases and action inputs can carry the file
        # name. Computed/encoded spellings are caught by the closed contract.
        text = str({k: v for k, v in scope.items() if k not in ('jobs', 'steps', 'defaults')})
        if re.search(r'\bGITHUB_PATH\b|::add-path::', text):
            errors.append('GITHUB_PATH manipulation')
        if re.search(r'\bPATH\s*=', str(scope.get('run', ''))):
            errors.append('PATH command override')
        if hook.search(str(scope.get('run', ''))):
            errors.append('startup/environment-file manipulation')
    inspect(workflow)
    for job in workflow['jobs'].values():
        inspect(job)
        if 'defaults' in job:
            errors.append('job shell override')
        for step in job.get('steps', []):
            inspect(step)
            if 'shell' in step:
                errors.append('step shell override')
    return errors
