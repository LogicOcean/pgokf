#!/usr/bin/env python3
"""Persist ownership before removing one explicitly labelled test container.

Usage: cleanup-owned-container.py FULL_ID LABEL_KEY LABEL_VALUE NEW_RECEIPT
Unproven volumes are always retained; association is never ownership. No name prefixes, event guesses, force volume rm or prune.
"""
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile


def docker(*args):
    return subprocess.check_output([*shlex.split(os.environ.get('DOCKER', 'docker')), *args], text=True)


def persist_exclusive(path, value):
    """Publish a complete, fsynced receipt without replacing prior evidence."""
    path = Path(path)
    with tempfile.NamedTemporaryFile(mode='w', dir=path.parent, delete=False) as stream:
        temporary = Path(stream.name)
        try:
            json.dump(value, stream, indent=2)
            stream.flush()
            os.fsync(stream.fileno())
            # Atomic and exclusive; a pre-existing receipt is an error.
            os.link(temporary, path)
        finally:
            temporary.unlink()
    directory = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def identity(container):
    return {field: container[field] for field in ('Id', 'Created', 'Mounts', 'Config')}


def cleanup(container_id, key, value, receipt, creation=None):
    if not re.fullmatch(r'[a-f0-9]{64}', container_id) or not key or not value:
        raise ValueError('full immutable container ID and nonempty ownership label required')
    container, = json.loads(docker('inspect', container_id))
    if container['Id'] != container_id or (container['Config'].get('Labels') or {}).get(key) != value:
        raise ValueError('container ownership does not match')
    volumes = [json.loads(docker('volume', 'inspect', mount['Name']))[0]
               for mount in container['Mounts'] if mount['Type'] == 'volume']
    owned = []
    if creation:
        proof = json.loads(Path(creation).read_text())
        if proof['container'] != identity(container) or proof['ownership_label'] != {key: value}:
            raise ValueError('creation receipt does not bind this container')
        for volume in proof['volumes']:
            labels = volume.get('Labels') or {}
            if (volume not in volumes or labels.get(key) != value
                    or labels.get('pgokf.creation-nonce') != proof['nonce']
                    or not volume.get('CreatedAt')):
                raise ValueError('volume creation identity or label changed')
            owned.append(volume)
    persist_exclusive(receipt, dict(container=container, volumes=volumes,
                                    owned=owned, ownership_label={key: value}))
    # Recheck after durable publication. Any inspect/write ambiguity preserves resources.
    current, = json.loads(docker('inspect', container_id))
    if identity(current) != identity(container):
        raise ValueError('container changed during receipt publication')
    for volume in owned:
        if json.loads(docker('volume', 'inspect', volume['Name'])) != [volume]:
            raise ValueError('volume changed during receipt publication')
    result = docker('rm', '-f', container_id)  # NEVER -v: inherited mounts are not ours.
    deleted = []
    for volume in owned:
        if json.loads(docker('volume', 'inspect', volume['Name'])) != [volume]:
            raise ValueError('volume changed after container removal')
        # No force: Docker refuses if another container still references the volume.
        deleted.append(docker('volume', 'rm', volume['Name']))
    persist_exclusive(str(receipt) + '.deleted.json',
                      dict(container_id=container_id, result=result, deleted_volumes=deleted))


if __name__ == '__main__':
    cleanup(*sys.argv[1:])
