#!/usr/bin/env python3
"""Regression tests for SQL inventory completeness and parity cleanup."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]

def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'scripts' / (name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

sql = load('compare-install-sql')
parity = load('upgrade-parity')

def block(text):
    return sql.BEGIN + '\n' + text + '\n' + sql.END

class Inventory(unittest.TestCase):
    def test_outside_sql_is_not_lost(self):
        original = block('SELECT 1;')
        self.assertNotEqual(sql.blocks(original), sql.blocks(original + '\nDELETE FROM pgokf.bundles;'))

    def test_literal_whitespace_is_significant(self):
        self.assertNotEqual(sql.blocks(block("SELECT 'a  b';")), sql.blocks(block("SELECT 'a b';")))

    def test_order_and_source_comments_are_immaterial(self):
        self.assertEqual(sql.blocks(block('-- path:1\nSELECT 1;') + block('SELECT 2;')),
                         sql.blocks(block('SELECT 2;') + block('-- path:9\n SELECT   1;')))

    def test_incomplete_block_fails_closed(self):
        with self.assertRaises(ValueError):
            sql.blocks(block('SELECT 1;') + sql.BEGIN + 'SELECT 2;')

class Cleanup(unittest.TestCase):
    def test_standalone_always_cleans_and_restores(self):
        for failure in (None, 'create', 'inventory', 'version', 'cleanup'):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as work:
                installed = Path(work) / 'installed.sql'
                committed = Path(work) / 'committed.sql'
                installed.write_text('generated')
                committed.write_text('committed')
                calls = []
                def execute(db, statement):
                    calls.append((db, statement))
                    if statement == 'CREATE EXTENSION pgokf;' and failure == 'create':
                        raise RuntimeError('create')
                    if statement == 'inventory':
                        return 'bad' if failure == 'inventory' else 'expected'
                    if statement == 'SELECT pgokf.version();':
                        return 'bad' if failure == 'version' else '0.3.0'
                    if statement.startswith('DROP DATABASE') and failure == 'cleanup':
                        raise RuntimeError('cleanup')
                    return ''
                if failure:
                    with self.assertRaises((RuntimeError, AssertionError)):
                        parity.check_committed_install(execute, installed, committed, 'expected', 'inventory', '0.3.0')
                else:
                    parity.check_committed_install(execute, installed, committed, 'expected', 'inventory', '0.3.0')
                self.assertEqual(installed.read_text(), 'generated')
                self.assertTrue(any(s.startswith('DROP DATABASE') for _, s in calls))
                if failure != 'cleanup':
                    self.assertTrue(any(s.startswith('DROP ROLE') for _, s in calls))

if __name__ == '__main__':
    unittest.main()
