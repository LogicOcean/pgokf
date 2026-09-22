#!/usr/bin/env python3
"""Persist ownership before removing one explicitly labelled test container.

Usage: cleanup-owned-container.py FULL_ID LABEL_KEY LABEL_VALUE NEW_RECEIPT
Only Docker's container-associated anonymous volumes are removed (-v); named
volumes are retained. No name prefixes, event guesses, force volume rm or prune.
"""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


def docker(*args):
    return subprocess.check_output(['docker', *args], text=True)


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


def cleanup(container_id, key, value, receipt):
    if not re.fullmatch(r'[a-f0-9]{64}', container_id) or not key or not value:
        raise ValueError('full immutable container ID and nonempty ownership label required')
    container, = json.loads(docker('inspect', container_id))
    if container['Id'] != container_id or container['Config'].get('Labels', {}).get(key) != value:
        raise ValueError('container ownership does not match')
    volumes = [json.loads(docker('volume', 'inspect', mount['Name']))[0]
               for mount in container['Mounts'] if mount['Type'] == 'volume']
    persist_exclusive(receipt, dict(container=container, volumes=volumes,
                                    ownership_label={key: value}))
    # If inspect, ownership verification or durable publication fails, no deletion.
    result = docker('rm', '-fv', container_id)
    persist_exclusive(str(receipt) + '.deleted.json', dict(container_id=container_id, result=result))


if __name__ == '__main__':
    cleanup(*sys.argv[1:])
