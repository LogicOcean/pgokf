#!/usr/bin/env python3
"""Create labelled volumes for every image VOLUME and bind creation to exact ID.

Usage: create-owned-container.py KEY VALUE NEW_RECEIPT IMAGE [create options] -- [command]
Failures deliberately leave resources for inspection, never guess cleanup ownership.
Receipts live in a private per-run directory. Only this creator issues deletion proof.
"""
import importlib.util
import json
from pathlib import Path
import sys
import uuid

spec = importlib.util.spec_from_file_location('cleanup', Path(__file__).with_name('cleanup-owned-container.py'))
cleanup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cleanup)


def create(key, value, receipt, image, *args):
    if not key or not value or key == 'pgokf.creation-nonce':
        raise ValueError('nonempty independent ownership label required')
    if Path(receipt).exists():
        raise FileExistsError(receipt)
    nonce = uuid.uuid4().hex
    # Reserve before creating anything, including on receipt-name collision.
    cleanup.persist_exclusive(str(receipt) + '.intent.json', dict(nonce=nonce, image=image))
    image_info, = json.loads(cleanup.docker('image', 'inspect', image))
    options, command = list(args), []
    if '--' in options:
        cut = options.index('--')
        options, command = options[:cut], options[cut + 1:]
    mounts, volumes = [], []
    for destination in sorted(image_info['Config'].get('Volumes') or {}):
        name = 'pgokf-' + uuid.uuid4().hex
        if name in cleanup.docker('volume', 'ls', '-q').splitlines():
            raise ValueError('volume name already exists')
        # Unique random name and independent nonce; inspect the actual create result.
        created = cleanup.docker('volume', 'create', '--label', key + '=' + value,
                                 '--label', 'pgokf.creation-nonce=' + nonce, name).strip()
        volume, = json.loads(cleanup.docker('volume', 'inspect', created))
        if (created != name or volume['Labels'] != {key: value, 'pgokf.creation-nonce': nonce}
                or not volume.get('CreatedAt')):
            raise ValueError('ambiguous volume creation')
        cleanup.persist_exclusive(str(receipt) + '.' + name + '.json', volume)
        volumes.append(volume)
        mounts += ['--mount', f'type=volume,source={name},target={destination}']
    cid = cleanup.docker('create', '--label', key + '=' + value, *mounts,
                         *options, image_info['Id'], *command).strip()
    container, = json.loads(cleanup.docker('inspect', cid))
    if (container['Config'].get('Labels') or {}).get(key) != value:
        raise ValueError('container label changed')
    names = {m['Name'] for m in container['Mounts'] if m['Type'] == 'volume'}
    if any(v['Name'] not in names for v in volumes):
        raise ValueError('created volume not attached')
    cleanup.persist_exclusive(receipt, dict(container=cleanup.identity(container), volumes=volumes,
                                            ownership_label={key: value}, nonce=nonce))
    return cid


if __name__ == '__main__':
    print(create(*sys.argv[1:]))
