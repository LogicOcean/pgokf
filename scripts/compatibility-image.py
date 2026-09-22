#!/usr/bin/env python3
"""Build exact source using current Beta 3 validation tooling in a separate context."""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
subprocess.run(['python3', str(ROOT / 'scripts/compatibility-source.py'), '.'], check=True)
with tempfile.TemporaryDirectory(prefix='pgokf-beta-build-') as work:
    recipe = Path(work) / 'Dockerfile'
    recipe.write_text((ROOT / 'packaging/docker/Dockerfile').read_text().replace(
        'COPY packaging/verify-postgres.sh', 'COPY --from=validation packaging/verify-postgres.sh'))
    subprocess.run(['docker', 'build', '-f', str(recipe), '--build-context', 'validation=' + str(ROOT),
                    '--build-arg', 'PG_MAJOR=19', '--build-arg', 'PG_IMAGE_TAG=19beta3',
                    '--build-arg', 'WITH_PGVECTOR=0', '--build-arg', 'WITH_PG_CRON=0',
                    '--build-arg', 'WITH_PG_TEXTSEARCH=0', '-t', 'pgokf-beta-compat', '.'], check=True)
