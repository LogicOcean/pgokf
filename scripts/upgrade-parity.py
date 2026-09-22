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


def drop_catalog(sql, db):
    # Database removal must precede role removal: ACL/owner dependencies are
    # database-local. Missing roles are expected after a failed bootstrap.
    assert re.fullmatch(r'[a-z_][a-z_0-9]*', db)
    sql('postgres', f'DROP DATABASE IF EXISTS {db};')
    sql('postgres', 'DROP ROLE IF EXISTS pgokf_admin, pgokf_writer, pgokf_reader, pgokf_dispatcher;')


def check_committed_install(sql, installed, committed, expected, inventory, version):
    backup = installed.read_bytes()
    try:
        installed.write_bytes(committed.read_bytes())
        try:
            sql('postgres', 'CREATE DATABASE fresh_committed;')
            sql('fresh_committed', 'CREATE EXTENSION pgokf;')
            compare(expected, sql('fresh_committed', inventory), 'committed install script')
            assert sql('fresh_committed', 'SELECT pgokf.version();') == version
        finally:
            drop_catalog(sql, 'fresh_committed')
    finally:
        # Also restore when database/role cleanup itself fails.
        installed.write_bytes(backup)


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
    # A clean release commits its generated fresh install script. pgrx's
    # entity ordering is unstable across invocations, so the committed
    # snapshot cannot byte-match a regeneration; it is validated semantically
    # below (a catalog created through it must inventory-equal fresh).
    committed_install = ROOT / 'crates/extension/sql' / f'pgokf--{version}.sql'
    if '-' not in version and not committed_install.is_file():
        raise AssertionError('final release requires a committed install script')
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

            # Drop the generated catalog and all extension roles on both
            # release and development paths, before any standalone bootstrap.
            drop_catalog(sql, 'fresh')
            if committed_install.is_file():
                check_committed_install(sql, extension / committed_install.name,
                                        committed_install, expected, inventory, version)
                print('committed install script: standalone bootstrap, role creation, '
                      'owner/ACL/membership catalog parity passed', flush=True)

            failures = []
            for index, signature in enumerate(ADOPT):
                expected_role = 'pgokf_reader' if index == 0 else 'pgokf_admin'
                cases = {
                    'owner': f'ALTER FUNCTION {signature} OWNER TO pgokf_reader;',
                    'writer': f'GRANT EXECUTE ON FUNCTION {signature} TO pgokf_writer;',
                    'other': f'GRANT EXECUTE ON FUNCTION {signature} TO hostile_other;',
                    'missing': f'REVOKE EXECUTE ON FUNCTION {signature} FROM {expected_role};',
                    'grant_option': f'GRANT EXECUTE ON FUNCTION {signature} TO {expected_role} WITH GRANT OPTION;',
                    'public': f'GRANT EXECUTE ON FUNCTION {signature} TO PUBLIC;',
                }
                if index:
                    cases['reader'] = f'GRANT EXECUTE ON FUNCTION {signature} TO pgokf_reader;'
                for detached in (False, True):
                    for name, mutation in cases.items():
                        label = f'adoption {index} {detached=} {name}'
                        sql('postgres', 'CREATE DATABASE hostile; CREATE ROLE hostile_other;')
                        sql('hostile', "CREATE EXTENSION pgokf VERSION '0.2.0'; ALTER EXTENSION pgokf UPDATE TO '0.3.0-dev';")
                        if detached:
                            sql('hostile', f'ALTER EXTENSION pgokf DROP FUNCTION {signature};')
                        sql('hostile', mutation)
                        before = sql('hostile', inventory)
                        try:
                            sql('hostile', 'ALTER EXTENSION pgokf UPDATE;')
                        except RuntimeError as error:
                            assert 'security metadata' in str(error), str(error)
                            compare(before, sql('hostile', inventory), label + ' rollback')
                            assert sql('hostile', "SELECT extversion FROM pg_extension WHERE extname='pgokf';") == '0.3.0-dev'
                            print(label + ': refused atomically', flush=True)
                        else:
                            failures.append(label)
                            print(label + ': UNSAFE ACCEPTANCE', flush=True)
                        drop_catalog(sql, 'hostile')
                        sql('postgres', 'DROP ROLE hostile_other;')
            def fingerprint(db):
                # Pin the old column names: additive new columns are allowed;
                # every old value in every populated table must survive.
                columns = json.loads(sql(db, """
                    SELECT json_object_agg(table_name, cols) FROM (
                      SELECT c.table_name, string_agg(quote_ident(c.column_name), ',' ORDER BY c.ordinal_position) cols
                      FROM information_schema.columns c
                      WHERE c.table_schema = 'pgokf' AND c.table_name IN
                        ('bundles','concepts','concept_source','concept_metadata') GROUP BY c.table_name
                    ) s;"""))
                fp = '\nUNION ALL\n'.join(
                    f"SELECT '{table}:' || row_to_json(r)::text FROM (SELECT {cols} FROM pgokf.{table}) r"
                    for table, cols in sorted(columns.items()))
                return f'SELECT * FROM ({fp}) rows ORDER BY 1;'

            def check_upgrade_invariants(route, before, fp):
                assert sql(route, "SELECT extversion FROM pg_extension WHERE extname='pgokf';") == version
                compare(before, sql(route, fp), route + ' data')
                assert sql(route, "SELECT state || ':' || array_to_string(reason_codes, ',') FROM pgokf.bundle_freshness;") == 'stale:legacy_pre_0.3.0'
                compare(expected, sql(route, inventory), route)
                evidence = sql(route, 'SELECT last_reconciled_at IS NULL FROM pgokf.bundle_freshness;')
                sql(route, "SELECT pgokf.register_bundle_content('upgrade-fixture', ARRAY['entry.md'], "
                    "ARRAY[convert_to('---\ntype: Concept\ntitle: Refreshed fixture\n---\nBody', 'UTF8')]);")
                assert sql(route, 'SELECT state FROM pgokf.bundle_freshness;') == 'stale'
                assert sql(route, 'SELECT last_reconciled_at IS NULL FROM pgokf.bundle_freshness;') == evidence == 't'

            for route in ('normal', 'missing', 'unowned', 'body', 'member_body', 'acl_order'):
                sql('postgres', f'CREATE DATABASE {route};')
                sql(route, "CREATE EXTENSION pgokf VERSION '0.2.0';" + seed)
                fp = fingerprint(route)
                before = sql(route, fp)
                sql(route, "ALTER EXTENSION pgokf UPDATE TO '0.3.0-dev';")
                if route != 'normal':
                    for signature in ADOPT:
                        if route in ('missing', 'unowned', 'body'):
                            sql(route, f'ALTER EXTENSION pgokf DROP FUNCTION {signature};')
                        if route == 'acl_order':
                            sql(route, f'REVOKE EXECUTE ON FUNCTION {signature} FROM postgres; GRANT EXECUTE ON FUNCTION {signature} TO postgres;')
                        if route in ('body', 'member_body'):
                            definition = sql(route, f"SELECT pg_get_functiondef('{signature}'::regprocedure);")
                            definition = re.sub(r'AS \$function\$.*\$function\$', "AS $function$ BEGIN RAISE EXCEPTION 'hostile body'; END $function$", definition, flags=re.S)
                            sql(route, definition)
                        if route == 'missing':
                            sql(route, f'DROP FUNCTION {signature};')
                sql(route, 'ALTER EXTENSION pgokf UPDATE;')
                check_upgrade_invariants(route, before, fp)
                print(f'{route}: parity, data preservation, refresh preserves staleness passed', flush=True)
                drop_catalog(sql, route)
            # Every deployed development point version reaches the final
            # release through ordinary bare UPDATE and converges with fresh.
            for point in ('0.3.0-dev', '0.3.0-dev1', '0.3.0-dev2', '0.3.0-dev3'):
                route = 'point_' + point.rsplit('-', 1)[1]
                sql('postgres', f'CREATE DATABASE {route};')
                sql(route, "CREATE EXTENSION pgokf VERSION '0.2.0';" + seed)
                fp = fingerprint(route)
                before = sql(route, fp)
                sql(route, f"ALTER EXTENSION pgokf UPDATE TO '{point}';")
                assert sql(route, "SELECT extversion FROM pg_extension WHERE extname='pgokf';") == point
                sql(route, 'ALTER EXTENSION pgokf UPDATE;')
                check_upgrade_invariants(route, before, fp)
                print(f'{route}: {point} reaches {version} with parity', flush=True)
                drop_catalog(sql, route)
            # Security-canonical conflicts must still fail at PostgreSQL's
            # signature/membership checks, with the entire update rolled back.
            for index, signature in enumerate(ADOPT):
                for conflict in ('return', 'arguments', 'other_extension'):
                    sql('postgres', 'CREATE DATABASE incompatible;')
                    sql('incompatible', "CREATE EXTENSION pgokf VERSION '0.2.0';"
                        "ALTER EXTENSION pgokf UPDATE TO '0.3.0-dev';"
                        f"ALTER EXTENSION pgokf DROP FUNCTION {signature};")
                    if conflict in ('return', 'arguments'):
                        role = 'pgokf_reader' if index == 0 else 'pgokf_admin'
                        declaration = f"{signature} RETURNS integer LANGUAGE sql AS 'SELECT 1'"
                        if conflict == 'arguments':
                            if index == 0:
                                declaration = f"{signature} RETURNS TABLE (hostile_id bigint, schedule text) LANGUAGE plpgsql AS 'BEGIN RETURN; END'"
                            else:
                                declaration = signature.replace('(uuid,', '(hostile_id uuid,') + " RETURNS void LANGUAGE plpgsql AS 'BEGIN RETURN; END'"
                        sql('incompatible', f"DROP FUNCTION {signature};"
                            f"CREATE FUNCTION {declaration};"
                            f"REVOKE ALL ON FUNCTION {signature} FROM PUBLIC;"
                            f"GRANT EXECUTE ON FUNCTION {signature} TO {role};")
                        diagnostic = 'cannot change name of input parameter' if conflict == 'arguments' and index else 'cannot change return type'
                    else:
                        sql('incompatible', f'ALTER EXTENSION plpgsql ADD FUNCTION {signature};')
                        diagnostic = 'already a member of extension'
                    membership = f"SELECT e.extname FROM pg_depend d JOIN pg_extension e ON e.oid=d.refobjid WHERE d.classid='pg_proc'::regclass AND d.refclassid='pg_extension'::regclass AND d.deptype='e' AND d.objid='{signature}'::regprocedure;"
                    before = sql('incompatible', inventory + membership)
                    try:
                        sql('incompatible', 'ALTER EXTENSION pgokf UPDATE;')
                    except RuntimeError as error:
                        assert diagnostic in str(error), str(error)
                    else:
                        raise AssertionError(f'{signature} {conflict} was silently adopted')
                    compare(before, sql('incompatible', inventory + membership), 'failed adoption rollback')
                    assert sql('incompatible', "SELECT extversion FROM pg_extension WHERE extname='pgokf';") == '0.3.0-dev'
                    print(f'{signature} {conflict}: refused atomically', flush=True)
                    drop_catalog(sql, 'incompatible')
            # A detector that always returns equal is no gate. Each mutation
            # runs in a rolled-back transaction and must alter the inventory.
            sql('postgres', 'CREATE DATABASE fresh;')
            sql('fresh', 'CREATE EXTENSION pgokf;')
            compare(expected, sql('fresh', inventory), 'fresh repeat')
            # Prove the FK mutation affects behavior, not merely a catalog bit.
            orphan = "INSERT INTO pgokf.concepts (bundle_id,id,path,title,file_hash,body_text) VALUES (987654321,'orphan','orphan.md','orphan','hash','body');"
            try:
                sql('fresh', 'BEGIN;' + orphan + 'ROLLBACK;')
            except RuntimeError as error:
                assert 'foreign key constraint' in str(error), str(error)
            else:
                raise AssertionError('canonical catalog permits an orphan')
            assert sql('fresh', 'BEGIN; ALTER TABLE pgokf.concepts DISABLE TRIGGER ALL;' + orphan +
                       'SELECT count(*) FROM pgokf.concepts WHERE bundle_id=987654321; ROLLBACK;') == '1'
            probes = {
                'fk enforcement': 'ALTER TABLE pgokf.concepts DISABLE TRIGGER ALL;',
                'rule': 'CREATE RULE hostile_no_insert AS ON INSERT TO pgokf.concept_metadata DO INSTEAD NOTHING;',
                'constraint comment': "COMMENT ON CONSTRAINT bundles_pkey ON pgokf.bundles IS 'hostile';",
                'default acl': 'ALTER DEFAULT PRIVILEGES IN SCHEMA pgokf GRANT ALL ON TABLES TO pgokf_reader;',
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
                    failures.append('mutation ' + name)
                    print(f'mutation {name}: UNSAFE ACCEPTANCE', flush=True)
            compare(expected, sql('fresh', inventory), 'probe rollback')
            definition_probes = {
                'external enum labels': (
                    "CREATE TYPE public.hostile_enum AS ENUM ('a'); ALTER EXTENSION pgokf ADD TYPE public.hostile_enum;",
                    "ALTER TYPE public.hostile_enum ADD VALUE 'b';"),
                'external domain constraint': (
                    "CREATE DOMAIN public.hostile_domain AS integer; ALTER EXTENSION pgokf ADD TYPE public.hostile_domain;",
                    "ALTER DOMAIN public.hostile_domain ADD CONSTRAINT positive CHECK (VALUE > 0);"),
                'domain base': (
                    "CREATE DOMAIN pgokf.hostile_domain AS integer; ALTER EXTENSION pgokf ADD TYPE pgokf.hostile_domain;",
                    "ALTER EXTENSION pgokf DROP TYPE pgokf.hostile_domain; DROP DOMAIN pgokf.hostile_domain; CREATE DOMAIN pgokf.hostile_domain AS bigint; ALTER EXTENSION pgokf ADD TYPE pgokf.hostile_domain;"),
                'domain collation': (
                    'CREATE DOMAIN pgokf.hostile_domain AS text COLLATE "C"; ALTER EXTENSION pgokf ADD TYPE pgokf.hostile_domain;',
                    'ALTER EXTENSION pgokf DROP TYPE pgokf.hostile_domain; DROP DOMAIN pgokf.hostile_domain; CREATE DOMAIN pgokf.hostile_domain AS text COLLATE "POSIX"; ALTER EXTENSION pgokf ADD TYPE pgokf.hostile_domain;'),
                'enum order': (
                    "CREATE TYPE pgokf.hostile_enum AS ENUM ('a','b'); ALTER EXTENSION pgokf ADD TYPE pgokf.hostile_enum;",
                    "ALTER EXTENSION pgokf DROP TYPE pgokf.hostile_enum; DROP TYPE pgokf.hostile_enum; CREATE TYPE pgokf.hostile_enum AS ENUM ('b','a'); ALTER EXTENSION pgokf ADD TYPE pgokf.hostile_enum;"),
                'enum labels': (
                    "CREATE TYPE pgokf.hostile_enum AS ENUM ('a','b'); ALTER EXTENSION pgokf ADD TYPE pgokf.hostile_enum;",
                    "ALTER TYPE pgokf.hostile_enum ADD VALUE 'c' BEFORE 'b';"),
                'domain default': (
                    "CREATE DOMAIN pgokf.hostile_domain AS integer DEFAULT 1; ALTER EXTENSION pgokf ADD TYPE pgokf.hostile_domain;",
                    "ALTER DOMAIN pgokf.hostile_domain SET DEFAULT 2;"),
                'domain nullability': (
                    "CREATE DOMAIN pgokf.hostile_domain AS integer; ALTER EXTENSION pgokf ADD TYPE pgokf.hostile_domain;",
                    "ALTER DOMAIN pgokf.hostile_domain SET NOT NULL;"),
                'domain constraint': (
                    "CREATE DOMAIN pgokf.hostile_domain AS integer; ALTER EXTENSION pgokf ADD TYPE pgokf.hostile_domain;",
                    "ALTER DOMAIN pgokf.hostile_domain ADD CONSTRAINT positive CHECK (VALUE > 0);"),
                'domain constraint comment': (
                    "CREATE DOMAIN pgokf.hostile_domain AS integer CONSTRAINT positive CHECK (VALUE > 0); ALTER EXTENSION pgokf ADD TYPE pgokf.hostile_domain;",
                    "COMMENT ON CONSTRAINT positive ON DOMAIN pgokf.hostile_domain IS 'hostile';"),
                'rule comment': (
                    "CREATE RULE hostile AS ON INSERT TO pgokf.concept_metadata DO INSTEAD NOTHING;",
                    "COMMENT ON RULE hostile ON pgokf.concept_metadata IS 'hostile';"),
                'trigger comment': (
                    "CREATE FUNCTION pgokf.hostile_trigger() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END'; CREATE TRIGGER hostile BEFORE INSERT ON pgokf.concept_metadata FOR EACH ROW EXECUTE FUNCTION pgokf.hostile_trigger();",
                    "COMMENT ON TRIGGER hostile ON pgokf.concept_metadata IS 'hostile';"),
                'policy comment': ('', "COMMENT ON POLICY concepts_tenant_isolation ON pgokf.concepts IS 'hostile';"),
                'extension comment': ('', "COMMENT ON EXTENSION pgokf IS 'hostile';"),
                'global default acl': ('', 'ALTER DEFAULT PRIVILEGES GRANT ALL ON TABLES TO pgokf_reader;'),
            }
            for name, (setup, mutation) in definition_probes.items():
                baseline = sql('fresh', 'BEGIN;\n' + setup + '\n' + inventory + '\nROLLBACK;')
                actual = sql('fresh', 'BEGIN;\n' + setup + '\n' + mutation + '\n' + inventory + '\nROLLBACK;')
                if baseline == actual:
                    failures.append(name)
                    print(f'mutation {name}: UNSAFE ACCEPTANCE', flush=True)
                else:
                    print(f'mutation {name}: rejected', flush=True)
            assert not failures, 'Unsafe acceptances: ' + ', '.join(failures)
        finally:
            if started:
                run(str(bindir / 'pg_ctl'), '-D', data, '-m', 'immediate', '-w', 'stop')


if __name__ == '__main__':
    main()
