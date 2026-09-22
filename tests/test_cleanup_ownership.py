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
                        return json.dumps([dict(Id=cid, Config={'Labels': {'run': 'other' if mode == 'wrong-label' else 'mine'}},
                                                Mounts=[dict(Type='volume', Name='exact-volume')])])
                    if args[:2] == ('volume', 'inspect'):
                        return json.dumps([dict(Name='exact-volume')])
                    self.assertEqual(args, ('rm', '-fv', cid))
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


if __name__ == '__main__':
    unittest.main()
