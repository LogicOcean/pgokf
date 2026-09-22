"""Small semantic policy for matrix expansion and trusted shell startup."""
from itertools import product
import re

CLEAN_SHELL = '/usr/bin/env -u BASH_ENV -u ENV bash --noprofile --norc -e -o pipefail {0}'


def effective_matrix(matrix):
    """Actions excludes base combinations, then applies includes in order.

    Includes can augment original axis values but cannot overwrite them;
    unmatched includes create standalone jobs (including an excluded leg).
    """
    axes = {k: v for k, v in matrix.items() if k not in ('include', 'exclude')}
    original = [dict(zip(axes, values)) for values in product(*axes.values())]
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


def check_shell_policy(workflow):
    errors = []
    if workflow.get('defaults') != {'run': {'shell': CLEAN_SHELL}}:
        errors.append('explicit clean strict shell required')
    # Environment-file writes can change later execution, including computed
    # hook names. None are needed in these workflows: outputs carry job data.
    hook = re.compile(r'\b(?:BASH_ENV|ENV|SHELLOPTS|BASHOPTS|GITHUB_ENV)\b|BASH_FUNC_')
    def inspect(scope):
        env = scope.get('env', {})
        if not isinstance(env, dict) or any(hook.search(str(k)) for k in env):
            errors.append('startup environment injection')
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
