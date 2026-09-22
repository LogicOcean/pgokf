#!/usr/bin/env python3
"""Real Docker hostile ownership regressions, with durable evidence and donor bytes."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import uuid

ROOT = Path(__file__).resolve().parents[1]

def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'scripts' / (name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

cleanup = load('cleanup-owned-container')
creator = load('create-owned-container')
docker = cleanup.docker
out = Path(sys.argv[1]).resolve()
out.mkdir(parents=True, exist_ok=False)
run = uuid.uuid4().hex
containers, volumes = [], []


def create(*args):
    cid = docker('create', '--label', 'test=' + run, *args).strip()
    containers.append(cid)
    return cid


def exists(kind, name):
    return subprocess.run(['docker', *kind, 'inspect', name], capture_output=True).returncode == 0


def reject(call):
    try:
        call()
    except (ValueError, OSError, subprocess.CalledProcessError):
        return
    raise AssertionError('destructive request unexpectedly accepted')


try:
    donor = create('-v', '/data', 'alpine:3.21', 'sh', '-c', 'echo donor-sentinel >/data/proof')
    docker('start', '-a', donor)
    info, = json.loads(docker('inspect', donor))
    borrowed = info['Mounts'][0]['Name']
    volumes.append(borrowed)
    cleanup.persist_exclusive(out / 'donor.json', info)
    borrower = create('--volumes-from', donor, 'alpine:3.21', 'true')
    docker('rm', donor)
    cleanup.cleanup(borrower, 'test', run, out / 'borrowed.json')
    assert exists(['volume'], borrowed)
    # Mixed mount: one independently created owned volume plus the donor's data.
    proof = out / 'mixed-create.json'
    mixed = creator.create('test', run, proof, 'postgres:18', '--mount',
                           f'type=volume,source={borrowed},target=/borrowed', '--', 'true')
    containers.append(mixed)
    owned = json.loads(proof.read_text())['volumes']
    volumes.extend(v['Name'] for v in owned)
    cleanup.cleanup(mixed, 'test', run, out / 'mixed-delete.json', proof)
    assert all(not exists(['volume'], v['Name']) for v in owned)
    assert exists(['volume'], borrowed)
    reader = create('--mount', f'type=volume,source={borrowed},target=/data', 'alpine:3.21', 'cat', '/data/proof')
    assert docker('start', '-a', reader).strip() == 'donor-sentinel'
    cleanup.cleanup(reader, 'test', run, out / 'reader.json')
    # Arbitrary named volumes remain preserved without creation proof.
    named = docker('volume', 'create', '--label', 'test=' + run, 'pgokf-test-' + uuid.uuid4().hex).strip()
    volumes.append(named)
    cid = create('-v', named + ':/data', 'alpine:3.21', 'true')
    reject(lambda: cleanup.cleanup(cid, 'test', 'wrong', out / 'wrong-container.json'))
    assert exists([], cid)
    (out / 'collision.json').write_text('prior evidence')
    reject(lambda: cleanup.cleanup(cid, 'test', run, out / 'collision.json'))
    assert exists([], cid)
    cleanup.cleanup(cid, 'test', run, out / 'named.json')
    assert exists(['volume'], named)
    # Forged claim for a volume with mismatched ownership must fail closed.
    proof = out / 'normal-create.json'
    cid = creator.create('test', run, proof, 'postgres:18', '--', 'true')
    containers.append(cid)
    record = json.loads(proof.read_text())
    volumes.extend(v['Name'] for v in record['volumes'])
    wrong = json.loads(proof.read_text())
    wrong['volumes'][0]['Labels']['test'] = 'other'
    cleanup.persist_exclusive(out / 'wrong-proof.json', wrong)
    reject(lambda: cleanup.cleanup(cid, 'test', run, out / 'wrong-volume.json', out / 'wrong-proof.json'))
    assert exists([], cid)
    before = docker('ps', '-aq')
    reject(lambda: creator.create('test', run, proof, 'postgres:18', '--', 'true'))
    assert docker('ps', '-aq') == before
    cleanup.cleanup(cid, 'test', run, out / 'normal-delete.json', proof)
    assert all(not exists(['volume'], v['Name']) for v in record['volumes'])
    cleanup.persist_exclusive(out / 'result.json', dict(result='PASS', donor_data='donor-sentinel',
        cases=['borrowed anonymous', 'mixed owned and borrowed', 'wrong container label', 'wrong volume label',
               'receipt collision', 'creation collision', 'named preservation', 'normal owned cleanup']))
    print('PASS: eight real Docker controls; donor data survives')
finally:
    # These exact IDs/names were created above; never touch a pre-existing resource.
    for cid in containers:
        if exists([], cid):
            cleanup.cleanup(cid, 'test', run, out / (cid + '.final.json'))
    for volume in volumes:
        if exists(['volume'], volume):
            cleanup.persist_exclusive(out / (volume + '.final.json'), json.loads(docker('volume', 'inspect', volume)))
            docker('volume', 'rm', volume)
