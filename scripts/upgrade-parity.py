#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Compare installed pgokf upgrade paths in an isolated, disposable cluster.

Targets PostgreSQL 18. Requires a current cargo pgrx install and an authentic
0.2.0 install SQL file
in pg_config's extension directory. No production connection is accepted.
"""
import argparse
import difflib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / 'tests' / 'upgrade'
ADOPT = ('pgokf.list_scheduled_refreshes()',
         'pgokf.registry_set_status(uuid, text)',
         'pgokf.registry_set_poll_interval(uuid, integer)')


def run(*args, input=None):
    result = subprocess.run(args, input=input, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if result.returncode:
        raise RuntimeError(f'{args[0]} failed ({result.returncode}):\n{result.stderr}')
    return result.stdout


def compare(expected, actual, label):
    if expected != actual:
        delta = '\n'.join(difflib.unified_diff(expected.splitlines(), actual.splitlines(),
                                              fromfile='fresh', tofile=label))
        raise AssertionError(f'{label}: catalog drift\n{delta}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--pg-config', default='pg_config')
    args = parser.parse_args()
    bindir = Path(run(args.pg_config, '--bindir').strip())
    extension = Path(run(args.pg_config, '--sharedir').strip()) / 'extension'
    control = (ROOT / 'crates/extension/pgokf.control').read_text()
    version = re.search(r"default_version\s*=\s*'([^']+)'", control)[1]
    if (extension / 'pgokf.control').read_text() != control:
        raise AssertionError('installed control differs from checkout; run cargo pgrx install')
    for source in (ROOT / 'crates/extension/sql').glob('pgokf--*--*.sql'):
        if (extension / source.name).read_bytes() != source.read_bytes():
            raise AssertionError(f'installed upgrade differs from checkout: {source.name}')
    if not (extension / 'pgokf--0.2.0.sql').is_file():
        raise AssertionError('install the authentic 0.2.0 install SQL first (see release checklist)')
    # Short socket path; private permissions and no TCP listener. Ignore ambient
    # libpq service/options variables so this cannot select an external server.
    for key in list(os.environ):
        if key.startswith('PG'):
            del os.environ[key]
    with tempfile.TemporaryDirectory(prefix='pgokf-parity-', dir='/tmp') as work:
        data = str(Path(work) / 'data')
        run(str(bindir / 'initdb'), '-D', data, '-U', 'postgres', '--auth=trust', '--no-locale')
        started = False
        try:
            run(str(bindir / 'pg_ctl'), '-D', data, '-l', str(Path(work) / 'server.log'),
                '-o', f"-c listen_addresses='' -k {work}", '-w', 'start')
            started = True

            def sql(db, statement):
                return run(str(bindir / 'psql'), '-X', '-qAt', '-v', 'ON_ERROR_STOP=1',
                           '-h', work, '-U', 'postgres', '-d', db, input=statement).strip()

            inventory = (FIXTURES / 'inventory.sql').read_text()
            seed = (FIXTURES / 'seed.sql').read_text()
            sql('postgres', 'CREATE DATABASE fresh;')
            sql('fresh', 'CREATE EXTENSION pgokf;')
            assert sql('fresh', 'SELECT pgokf.version();') == version
            expected = sql('fresh', inventory)
            print(f'Fresh {version}: {len(expected.splitlines())} inventory rows', flush=True)

            def drop_catalog(db):
                sql('postgres', f'DROP DATABASE {db};')
                # Roles are cluster-global: never let a fresh installation
                # supply roles that an upgrade forgot to create.
                sql('postgres', 'DROP ROLE pgokf_admin, pgokf_writer, pgokf_reader, pgokf_dispatcher;')

            drop_catalog('fresh')
            for route in ('normal', 'missing', 'unowned'):
                sql('postgres', f'CREATE DATABASE {route};')
                sql(route, "CREATE EXTENSION pgokf VERSION '0.2.0';" + seed)
                # Pin the old column names: additive new columns are allowed;
                # every old value in every populated table must survive.
                columns = json.loads(sql(route, """
                    SELECT json_object_agg(table_name, cols) FROM (
                      SELECT c.table_name, string_agg(quote_ident(c.column_name), ',' ORDER BY c.ordinal_position) cols
                      FROM information_schema.columns c
                      WHERE c.table_schema = 'pgokf' AND c.table_name IN
                        ('bundles','concepts','concept_source','concept_metadata') GROUP BY c.table_name
                    ) s;"""))
                fingerprint = '\nUNION ALL\n'.join(
                    f"SELECT '{table}:' || row_to_json(r)::text FROM (SELECT {cols} FROM pgokf.{table}) r"
                    for table, cols in sorted(columns.items()))
                fingerprint = f'SELECT * FROM ({fingerprint}) rows ORDER BY 1;'
                before = sql(route, fingerprint)
                sql(route, "ALTER EXTENSION pgokf UPDATE TO '0.3.0-dev';")
                if route != 'normal':
                    for signature in ADOPT:
                        sql(route, f'ALTER EXTENSION pgokf DROP FUNCTION {signature};')
                        if route == 'missing':
                            sql(route, f'DROP FUNCTION {signature};')
                sql(route, 'ALTER EXTENSION pgokf UPDATE;')
                assert sql(route, "SELECT extversion FROM pg_extension WHERE extname='pgokf';") == version
                compare(before, sql(route, fingerprint), route + ' data')
                assert sql(route, "SELECT state || ':' || array_to_string(reason_codes, ',') FROM pgokf.bundle_freshness;") == 'stale:legacy_pre_0.3.0'
                compare(expected, sql(route, inventory), route)
                evidence = sql(route, 'SELECT last_reconciled_at IS NULL FROM pgokf.bundle_freshness;')
                sql(route, "SELECT pgokf.register_bundle_content('upgrade-fixture', ARRAY['entry.md'], "
                    "ARRAY[convert_to('---\ntype: Concept\ntitle: Refreshed fixture\n---\nBody', 'UTF8')]);")
                assert sql(route, 'SELECT state FROM pgokf.bundle_freshness;') == 'stale'
                assert sql(route, 'SELECT last_reconciled_at IS NULL FROM pgokf.bundle_freshness;') == evidence == 't'
                print(f'{route}: parity, data preservation, refresh preserves staleness passed', flush=True)
                drop_catalog(route)
            # A conflicting unowned signature must fail atomically, never
            # silently adopt an incompatible function and advance the receipt.
            sql('postgres', 'CREATE DATABASE incompatible;')
            sql('incompatible', "CREATE EXTENSION pgokf VERSION '0.2.0';"
                "ALTER EXTENSION pgokf UPDATE TO '0.3.0-dev';"
                "ALTER EXTENSION pgokf DROP FUNCTION pgokf.list_scheduled_refreshes();"
                "DROP FUNCTION pgokf.list_scheduled_refreshes();"
                "CREATE FUNCTION pgokf.list_scheduled_refreshes() RETURNS integer "
                "LANGUAGE sql AS 'SELECT 1';")
            before = sql('incompatible', inventory)
            try:
                sql('incompatible', 'ALTER EXTENSION pgokf UPDATE;')
            except RuntimeError as error:
                if 'cannot change return type' not in str(error):
                    raise
            else:
                raise AssertionError('incompatible function was silently adopted')
            compare(before, sql('incompatible', inventory), 'failed adoption rollback')
            assert sql('incompatible', "SELECT extversion FROM pg_extension WHERE extname='pgokf';") == '0.3.0-dev'
            print('incompatible adoption: rejected with catalog and version unchanged', flush=True)
            drop_catalog('incompatible')
            # A detector that always returns equal is no gate. Each mutation
            # runs in a rolled-back transaction and must alter the inventory.
            sql('postgres', 'CREATE DATABASE fresh;')
            sql('fresh', 'CREATE EXTENSION pgokf;')
            compare(expected, sql('fresh', inventory), 'fresh repeat')
            probes = {
                'owner': 'ALTER FUNCTION pgokf.version() OWNER TO pgokf_admin;',
                'role': 'GRANT pgokf_reader TO pgokf_dispatcher;',
                'policy': 'DROP POLICY concepts_tenant_isolation ON pgokf.concepts;',
                'comment': "COMMENT ON FUNCTION pgokf.version() IS 'mutation';",
                'acl': 'GRANT EXECUTE ON FUNCTION pgokf.mark_stale(bigint,text[],text,text) TO pgokf_reader;',
                'membership': 'ALTER EXTENSION pgokf DROP FUNCTION pgokf.version();',
                'column': 'ALTER TABLE pgokf.bundles ALTER COLUMN enabled DROP NOT NULL;',
                'security': 'ALTER FUNCTION pgokf.mark_stale(bigint,text[],text,text) SECURITY INVOKER;',
                'dump': "UPDATE pg_extension SET extconfig = '{}'::oid[], extcondition = '{}'::text[] WHERE extname = 'pgokf';",
            }
            for name, mutation in probes.items():
                actual = sql('fresh', 'BEGIN;\n' + mutation + '\n' + inventory + '\nROLLBACK;')
                try:
                    compare(expected, actual, name)
                except AssertionError:
                    print(f'mutation {name}: rejected', flush=True)
                else:
                    raise AssertionError(f'mutation {name}: detector accepted drift')
            compare(expected, sql('fresh', inventory), 'probe rollback')
        finally:
            if started:
                run(str(bindir / 'pg_ctl'), '-D', data, '-m', 'immediate', '-w', 'stop')


if __name__ == '__main__':
    main()
