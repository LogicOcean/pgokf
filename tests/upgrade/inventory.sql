-- SPDX-License-Identifier: AGPL-3.0-only
-- Stable identities, never database-local OIDs. Keep comments, ACLs, column
-- order, security attributes, membership and dump registration in the comparison.
SET search_path = pg_catalog;
WITH namespaces AS (
    SELECT oid FROM pg_namespace WHERE nspname IN ('pgokf', 'pgokf_private', 'pgokf_web')
), owned_types AS (
    SELECT d.objid FROM pg_depend d JOIN pg_extension e ON e.oid = d.refobjid
    WHERE e.extname = 'pgokf' AND d.refclassid = 'pg_extension'::regclass
      AND d.classid = 'pg_type'::regclass AND d.deptype = 'e'
), inventory AS (
    SELECT 'role' AS kind, rolname::text AS identity,
           jsonb_build_array(rolsuper, rolinherit, rolcreaterole, rolcreatedb,
                            rolcanlogin, rolreplication, rolbypassrls,
                            shobj_description(oid, 'pg_authid')) AS definition
    FROM pg_roles WHERE rolname IN ('pgokf_reader', 'pgokf_writer', 'pgokf_admin', 'pgokf_dispatcher')
    UNION ALL
    SELECT 'role membership', roleid::regrole::text || ':' || member::regrole::text,
           jsonb_build_array(admin_option, inherit_option, set_option)
    FROM pg_auth_members WHERE roleid::regrole::text LIKE 'pgokf_%' OR member::regrole::text LIKE 'pgokf_%'
    UNION ALL
    SELECT 'function' AS kind, p.oid::regprocedure::text AS identity,
           jsonb_build_array(pg_get_functiondef(p.oid), p.proowner::regrole::text, (SELECT array_agg(acl::text ORDER BY acl::text) FROM unnest(p.proacl) acl),
                             obj_description(p.oid, 'pg_proc')) AS definition
    FROM pg_proc p WHERE pronamespace IN (SELECT oid FROM namespaces)
    UNION ALL
    SELECT 'relation', c.oid::regclass::text,
           jsonb_build_array(c.relkind, c.relpersistence, c.relrowsecurity,
               c.relforcerowsecurity, c.relreplident, c.reloptions, c.relowner::regrole::text, (SELECT array_agg(acl::text ORDER BY acl::text) FROM unnest(c.relacl) acl),
               obj_description(c.oid, 'pg_class'),
               CASE WHEN c.relkind = 'v' THEN pg_get_viewdef(c.oid) END,
               CASE WHEN c.relkind = 'i' THEN pg_get_indexdef(c.oid) END)
    FROM pg_class c WHERE relnamespace IN (SELECT oid FROM namespaces)
    UNION ALL
    SELECT 'column', a.attrelid::regclass::text || '.' || a.attname,
           jsonb_build_array(a.attnum, format_type(a.atttypid, a.atttypmod),
               a.attnotnull, a.attidentity, a.attgenerated, a.attstorage,
               a.attcompression, a.attcollation::regcollation::text, (SELECT array_agg(acl::text ORDER BY acl::text) FROM unnest(a.attacl) acl),
               pg_get_expr(d.adbin, d.adrelid), col_description(a.attrelid, a.attnum))
    FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid
    LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
    WHERE c.relnamespace IN (SELECT oid FROM namespaces) AND a.attnum > 0 AND NOT a.attisdropped
    UNION ALL
    SELECT 'type', t.oid::regtype::text,
           jsonb_build_array(t.typtype, format_type(t.typbasetype, t.typtypmod),
               t.typnotnull, t.typcollation::regcollation::text,
               pg_get_expr(t.typdefaultbin, 0), t.typdefault,
               (SELECT array_agg(e.enumlabel ORDER BY e.enumsortorder) FROM pg_enum e WHERE e.enumtypid = t.oid),
               t.typowner::regrole::text, (SELECT array_agg(acl::text ORDER BY acl::text) FROM unnest(t.typacl) acl), obj_description(t.oid, 'pg_type'))
    FROM pg_type t WHERE typnamespace IN (SELECT oid FROM namespaces) OR t.oid IN (SELECT objid FROM owned_types)
    UNION ALL
    SELECT 'constraint', CASE WHEN conrelid = 0 THEN contypid::regtype::text ELSE conrelid::regclass::text END || '.' || conname,
           jsonb_build_array(pg_get_constraintdef(oid), convalidated, condeferrable, condeferred, obj_description(oid, 'pg_constraint'))
    FROM pg_constraint WHERE connamespace IN (SELECT oid FROM namespaces) OR contypid IN (SELECT objid FROM owned_types)
    UNION ALL
    SELECT 'trigger', tgrelid::regclass::text || '.' || tgname,
           jsonb_build_array(pg_get_triggerdef(t.oid), t.tgenabled, obj_description(t.oid, 'pg_trigger'))
    FROM pg_trigger t JOIN pg_class c ON c.oid = tgrelid
    WHERE c.relnamespace IN (SELECT oid FROM namespaces) AND NOT t.tgisinternal
    UNION ALL
    SELECT 'fk trigger', k.conrelid::regclass::text || '.' || k.conname || ':' ||
               t.tgrelid::regclass::text || ':' || t.tgfoid::regprocedure::text,
           jsonb_build_array(t.tgenabled, t.tgtype, t.tgdeferrable, t.tginitdeferred,
                            obj_description(t.oid, 'pg_trigger'))
    FROM pg_trigger t JOIN pg_constraint k ON k.oid = t.tgconstraint
    WHERE t.tgisinternal AND k.contype = 'f'
      AND (k.connamespace IN (SELECT oid FROM namespaces)
           OR t.tgrelid IN (SELECT oid FROM pg_class WHERE relnamespace IN (SELECT oid FROM namespaces)))
    UNION ALL
    SELECT 'rule', r.ev_class::regclass::text || '.' || r.rulename,
           jsonb_build_array(pg_get_ruledef(r.oid), r.ev_enabled, obj_description(r.oid, 'pg_rewrite'))
    FROM pg_rewrite r JOIN pg_class c ON c.oid = r.ev_class
    WHERE c.relnamespace IN (SELECT oid FROM namespaces)
    UNION ALL
    SELECT 'default acl', d.defaclrole::regrole::text || ':' ||
               CASE WHEN d.defaclnamespace = 0 THEN '*' ELSE n.nspname END || ':' || d.defaclobjtype::text,
           (SELECT to_jsonb(array_agg(acl::text ORDER BY acl::text)) FROM unnest(d.defaclacl) acl)
    FROM pg_default_acl d LEFT JOIN pg_namespace n ON n.oid = d.defaclnamespace
    WHERE d.defaclnamespace IN (SELECT oid FROM namespaces) OR d.defaclnamespace = 0
    UNION ALL
    SELECT 'extension', extname, jsonb_build_array(extversion, extowner::regrole::text,
               obj_description(oid, 'pg_extension'))
    FROM pg_extension WHERE extname = 'pgokf'
    UNION ALL
    SELECT 'policy', polrelid::regclass::text || '.' || polname,
           jsonb_build_array(polcmd, polpermissive,
               (SELECT array_agg(CASE WHEN r = 0 THEN 'public' ELSE r::regrole::text END ORDER BY r::regrole::text)
                FROM unnest(polroles) r), pg_get_expr(polqual, polrelid), pg_get_expr(polwithcheck, polrelid), obj_description(p.oid, 'pg_policy'))
    FROM pg_policy p JOIN pg_class c ON c.oid = polrelid
    WHERE c.relnamespace IN (SELECT oid FROM namespaces)
    UNION ALL
    SELECT 'schema', nspname, jsonb_build_array(nspowner::regrole::text, (SELECT array_agg(acl::text ORDER BY acl::text) FROM unnest(nspacl) acl), obj_description(oid, 'pg_namespace'))
    FROM pg_namespace WHERE oid IN (SELECT oid FROM namespaces)
    UNION ALL
    SELECT 'membership', pg_describe_object(d.classid, d.objid, d.objsubid), to_jsonb(d.deptype)
    FROM pg_depend d JOIN pg_extension e ON e.oid = d.refobjid
    WHERE e.extname = 'pgokf' AND d.refclassid = 'pg_extension'::regclass AND d.deptype = 'e'
    UNION ALL
    SELECT 'dump', x.rel::regclass::text, to_jsonb(x.condition)
    FROM pg_extension e, unnest(e.extconfig, e.extcondition) x(rel, condition)
    WHERE e.extname = 'pgokf'
    UNION ALL
    SELECT 'initial privileges', pg_describe_object(classoid, objoid, objsubid),
           jsonb_build_array(privtype, (SELECT array_agg(acl::text ORDER BY acl::text) FROM unnest(initprivs) acl))
    FROM pg_init_privs
    WHERE (pg_identify_object(classoid, objoid, objsubid)).schema IN ('pgokf', 'pgokf_private', 'pgokf_web')
       OR (classoid = 'pg_namespace'::regclass AND objoid IN (SELECT oid FROM namespaces))
       OR (classoid = 'pg_type'::regclass AND objoid IN (SELECT objid FROM owned_types))
    UNION ALL
    SELECT 'index state', i.indexrelid::regclass::text,
           jsonb_build_array(i.indisvalid, i.indisready, i.indislive, i.indisreplident)
    FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
    WHERE c.relnamespace IN (SELECT oid FROM namespaces)
    UNION ALL
    SELECT 'sequence', seqrelid::regclass::text,
           to_jsonb(s) - 'seqrelid' - 'seqtypid' || jsonb_build_object('type', seqtypid::regtype::text)
    FROM pg_sequence s JOIN pg_class c ON c.oid = seqrelid
    WHERE c.relnamespace IN (SELECT oid FROM namespaces)
)
SELECT jsonb_build_array(kind, identity, definition)::text FROM inventory ORDER BY kind, identity, definition::text;
