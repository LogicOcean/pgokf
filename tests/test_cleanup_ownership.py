"""Cleanup must fail closed before deletion and retain exact mount ownership."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('cleanup', Path(__file__).resolve().parents[1] / 'scripts/cleanup-owned-container.py')
cleanup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cleanup)


class CleanupOwnership(unittest.TestCase):
    def test_capture_before_delete_and_refusals(self):
        cid = 'a' * 64
        for mode in ('pass', 'wrong-label', 'occupied-receipt', 'write-failure'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                receipt = Path(directory) / 'ownership.json'
                if mode == 'occupied-receipt':
                    receipt.write_text('prior evidence')
                if mode == 'write-failure':
                    receipt = receipt / 'missing' / 'ownership.json'
                deleted = []

                def docker(*args):
                    if args[0] == 'inspect':
                        return json.dumps([dict(Id=cid, Created='today', Config={'Labels': {'run': 'other' if mode == 'wrong-label' else 'mine'}},
                                                Mounts=[dict(Type='volume', Name='exact-volume')])])
                    if args[:2] == ('volume', 'inspect'):
                        return json.dumps([dict(Name='exact-volume')])
                    self.assertEqual(args, ('rm', '-f', cid))
                    captured = json.loads(receipt.read_text())
                    self.assertEqual(captured['container']['Id'], cid)
                    self.assertEqual(captured['volumes'][0]['Name'], 'exact-volume')
                    deleted.append(cid)
                    return cid

                with patch.object(cleanup, 'docker', docker):
                    if mode == 'pass':
                        cleanup.cleanup(cid, 'run', 'mine', receipt)
                        self.assertEqual(deleted, [cid])
                    else:
                        with self.assertRaises((ValueError, OSError)):
                            cleanup.cleanup(cid, 'run', 'mine', receipt)
                        self.assertEqual(deleted, [])
                        if mode == 'occupied-receipt':
                            self.assertEqual(receipt.read_text(), 'prior evidence')

class CleanupRaces(unittest.TestCase):
    def test_inspect_and_publication_races_preserve_container(self):
        cid = 'b' * 64
        for mode in ('changed-container', 'inspect-failure', 'file-fsync', 'directory-fsync'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                calls = []
                inspections = []
                def docker(*args):
                    calls.append(args)
                    if args[0] != 'inspect':
                        self.fail('destruction after ambiguous capture')
                    inspections.append(1)
                    if mode == 'inspect-failure':
                        raise OSError('inspect unavailable')
                    return json.dumps([dict(Id=cid, Created='changed' if mode == 'changed-container' and len(inspections) > 1 else 'original',
                                            Config={'Labels': {'run': 'mine'}}, Mounts=[])])
                syncs = []
                def fsync(_):
                    syncs.append(1)
                    if mode == 'file-fsync' or mode == 'directory-fsync' and len(syncs) == 2:
                        raise OSError('durability failed')
                with patch.object(cleanup, 'docker', docker), patch.object(cleanup.os, 'fsync', fsync):
                    with self.assertRaises((ValueError, OSError)):
                        cleanup.cleanup(cid, 'run', 'mine', Path(directory) / 'receipt.json')
                self.assertTrue(all(c[0] == 'inspect' for c in calls))

    def test_invalid_nonce_and_duplicate_claims_refuse_before_deletion(self):
        cid = 'c' * 64
        container = dict(Id=cid, Created='original', Config={'Labels': {'run': 'mine'}},
                         Mounts=[dict(Type='volume', Name='owned')])
        volume = dict(Name='owned', CreatedAt='today', Labels={'run': 'mine'})
        for nonce, copies in [(None, 1), ('', 1), ('bad', 1), ('d' * 32, 2)]:
            with self.subTest(nonce=nonce, copies=copies), tempfile.TemporaryDirectory() as directory:
                proof = Path(directory) / 'proof.json'
                proof.write_text(json.dumps(dict(nonce=nonce, volumes=[volume] * copies,
                    container=cleanup.identity(container), ownership_label={'run': 'mine'})))
                def docker(*args):
                    if args[0] == 'inspect': return json.dumps([container])
                    if args[:2] == ('volume', 'inspect'): return json.dumps([volume])
                    self.fail('destruction with invalid creation proof')
                with patch.object(cleanup, 'docker', docker), self.assertRaises(ValueError):
                    cleanup.cleanup(cid, 'run', 'mine', Path(directory) / 'delete.json', proof)

    def test_docker_mount_order_is_not_a_creation_identity_change(self):
        cid = 'e' * 64
        mounts = [dict(Type='volume', Name='first'), dict(Type='volume', Name='second')]
        container = dict(Id=cid, Created='original', Config={'Labels': {'run': 'mine'}}, Mounts=mounts)
        with tempfile.TemporaryDirectory() as directory:
            proof = Path(directory) / 'proof.json'
            proof.write_text(json.dumps(dict(container=container, volumes=[], nonce='f' * 32,
                                             ownership_label={'run': 'mine'})))
            calls = []
            def docker(*args):
                calls.append(args)
                if args[0] == 'inspect':
                    return json.dumps([{**container, 'Mounts': list(reversed(mounts))}])
                if args[:2] == ('volume', 'inspect'):
                    return json.dumps([dict(Name=args[2])])
                self.assertEqual(args, ('rm', '-f', cid))
                return cid
            with patch.object(cleanup, 'docker', docker):
                cleanup.cleanup(cid, 'run', 'mine', Path(directory) / 'delete.json', proof)
            self.assertIn(('rm', '-f', cid), calls)


if __name__ == '__main__':
    unittest.main()
