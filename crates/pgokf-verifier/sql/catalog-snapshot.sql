-- SPDX-License-Identifier: AGPL-3.0-only
-- PostgreSQL 15+ typed catalog snapshot. Provider resolved from durable
-- configuration and installed extensions. Execute in a read-only transaction.
SET search_path = pg_catalog;
SET timezone = 'UTC';
WITH configuration AS (SELECT pgokf.get_config() AS value), resolved AS (
 SELECT CASE
 WHEN value->>'search_backend' = 'native' THEN 'plain'
 WHEN value->>'search_backend' = 'bm25' THEN CASE
 WHEN value->>'bm25_provider' IN ('auto','pg_textsearch') AND EXISTS(SELECT 1 FROM pg_extension WHERE extname='pg_textsearch') THEN 'pg_textsearch'
 WHEN value->>'bm25_provider' IN ('auto','pg_search') AND EXISTS(SELECT 1 FROM pg_extension WHERE extname='pg_search') THEN 'pg_search'
 ELSE 'unresolved' END ELSE 'unresolved' END AS provider FROM configuration
), web AS (
 SELECT oid, reltype FROM pg_class WHERE oid IN ('pgokf_web.users'::regclass,'pgokf_web.sessions'::regclass)
), web_types AS (
 SELECT reltype AS oid FROM web UNION SELECT t.oid FROM pg_type t JOIN web ON t.typelem=web.reltype
), runtime AS (
 SELECT c.*, i.*, am.amname, n.nspname
 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
 JOIN pg_index i ON i.indexrelid=c.oid JOIN pg_am am ON am.oid=c.relam
 WHERE n.nspname IN ('pgokf','pgokf_private','pgokf_web')
), indexes AS (
 SELECT format('%I.%I',nspname,relname) AS identity, jsonb_build_object(
  'name',format('%I.%I',nspname,relname), 'relation',indrelid::regclass::text,'access_method',amname,
  'keys',(SELECT coalesce(jsonb_agg(jsonb_build_object(
    'expression',pg_get_indexdef(indexrelid,k+1,true),
    'opclass',format('%I.%I',ns.nspname,oc.opcname),
    'collation',CASE WHEN indcollation[k]=0 THEN NULL ELSE indcollation[k]::regcollation::text END,
    'options',indoption[k]) ORDER BY k),'[]'::jsonb)
   FROM generate_series(0,indnkeyatts-1) k JOIN pg_opclass oc ON oc.oid=indclass[k] JOIN pg_namespace ns ON ns.oid=oc.opcnamespace),
  'included_columns',(SELECT coalesce(jsonb_agg(pg_get_indexdef(indexrelid,k+1,true) ORDER BY k),'[]'::jsonb) FROM generate_series(indnkeyatts,indnatts-1) k),
  'predicate',pg_get_expr(indpred,indrelid), 'valid',indisvalid,'ready',indisready,'live',indislive,
  'unique',indisunique,'primary',indisprimary,'exclusion',indisexclusion,'immediate',indimmediate,
  'clustered',indisclustered,'replica_identity',indisreplident,'nulls_not_distinct',indnullsnotdistinct,
  'owner',pg_get_userbyid(relowner),
  'acl',(SELECT coalesce(jsonb_agg(a::text ORDER BY a::text),'[]'::jsonb) FROM unnest(relacl) a),
  'comment',obj_description(indexrelid,'pg_class'),
  'security_labels',(SELECT coalesce(jsonb_object_agg(provider,label ORDER BY provider),'{}'::jsonb) FROM pg_seclabel WHERE classoid='pg_class'::regclass AND objoid=indexrelid AND objsubid=0),
  'options',(SELECT coalesce(jsonb_object_agg(split_part(o,'=',1),substr(o,strpos(o,'=')+1) ORDER BY o),'{}'::jsonb) FROM unnest(reloptions) o),
  'tablespace',(SELECT spcname FROM pg_tablespace WHERE oid=CASE WHEN reltablespace=0 THEN (SELECT dattablespace FROM pg_database WHERE datname=current_database()) ELSE reltablespace END),
  'persistence',relpersistence::text,
  'extension_member',EXISTS(SELECT 1 FROM pg_depend WHERE classid='pg_class'::regclass AND objid=indexrelid AND refclassid='pg_extension'::regclass AND deptype='e')
 ) AS value FROM runtime
), roles AS (
 SELECT rolname AS identity,jsonb_build_object('name',rolname,'superuser',rolsuper,'inherit',rolinherit,
  'create_role',rolcreaterole,'create_db',rolcreatedb,'login',rolcanlogin,'replication',rolreplication,
  'bypass_rls',rolbypassrls,'connection_limit',rolconnlimit,'valid_until',rolvaliduntil::text,
  'config',(SELECT coalesce(jsonb_object_agg(split_part(c,'=',1),substr(c,strpos(c,'=')+1) ORDER BY c),'{}'::jsonb) FROM unnest(rolconfig) c),
  'database_config',(SELECT coalesce(jsonb_object_agg(db.datname,settings.value ORDER BY db.datname),'{}'::jsonb) FROM pg_db_role_setting st JOIN pg_database db ON db.oid=st.setdatabase CROSS JOIN LATERAL (SELECT jsonb_object_agg(split_part(c,'=',1),substr(c,strpos(c,'=')+1) ORDER BY c) value FROM unnest(st.setconfig) c) settings WHERE st.setrole=pg_roles.oid),
  'comment',shobj_description(oid,'pg_authid'),
  'security_labels',(SELECT coalesce(jsonb_object_agg(provider,label ORDER BY provider),'{}'::jsonb) FROM pg_shseclabel WHERE classoid='pg_authid'::regclass AND objoid=pg_roles.oid)
 ) AS value FROM pg_roles
), memberships AS (
 SELECT pg_get_userbyid(roleid) AS role,pg_get_userbyid(member) AS member,pg_get_userbyid(grantor) AS grantor,
 jsonb_build_object('role',pg_get_userbyid(roleid),'member',pg_get_userbyid(member),'grantor',pg_get_userbyid(grantor),
 'admin',admin_option,'inherit',coalesce((to_jsonb(pg_auth_members)->>'inherit_option')::boolean,true),'set',coalesce((to_jsonb(pg_auth_members)->>'set_option')::boolean,true)) AS value FROM pg_auth_members
), dependents AS (
 SELECT DISTINCT pg_describe_object(d.classid,d.objid,d.objsubid) AS identity FROM pg_depend d
 WHERE ((d.refclassid='pg_class'::regclass AND d.refobjid IN (SELECT oid FROM web)) OR (d.refclassid='pg_type'::regclass AND d.refobjid IN (SELECT oid FROM web_types)))
 AND NOT (d.refclassid='pg_class'::regclass AND d.refobjid IN (SELECT oid FROM web) AND (
   (d.classid='pg_constraint'::regclass AND EXISTS(SELECT 1 FROM pg_constraint WHERE oid=d.objid AND conrelid=d.refobjid))
   OR (d.classid='pg_attrdef'::regclass AND EXISTS(SELECT 1 FROM pg_attrdef WHERE oid=d.objid AND adrelid=d.refobjid))
   OR (d.classid='pg_policy'::regclass AND EXISTS(SELECT 1 FROM pg_policy WHERE oid=d.objid AND polrelid=d.refobjid))
   OR (d.classid='pg_type'::regclass AND d.deptype='i' AND d.objid IN (SELECT reltype FROM web WHERE oid=d.refobjid))
   OR (d.classid='pg_class'::regclass AND d.deptype IN ('a','i') AND (SELECT relkind FROM pg_class WHERE oid=d.objid) IN ('i','t'))
 ))
 AND NOT (d.refclassid='pg_type'::regclass AND d.classid='pg_type'::regclass AND d.deptype='i'
   AND EXISTS(SELECT 1 FROM pg_type WHERE oid=d.objid AND typelem=d.refobjid))
)
SELECT jsonb_build_object('version',1,'provider',(SELECT provider FROM resolved),
 'provider_context',jsonb_build_object('provider_schema',(SELECT n.nspname FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace WHERE e.extname=(SELECT provider FROM resolved)),
 'search_opclasses',(SELECT coalesce(jsonb_agg(format('%I.%I',ns.nspname,oc.opcname) ORDER BY k),'[]'::jsonb) FROM unnest(ARRAY['id','title','description','body_text','type']) WITH ORDINALITY cols(name,k) JOIN pg_attribute a ON a.attrelid='pgokf.concepts'::regclass AND a.attname=cols.name JOIN pg_opclass oc ON oc.opcintype IN (a.atttypid,'anyelement'::regtype) AND oc.opcdefault JOIN pg_namespace ns ON ns.oid=oc.opcnamespace JOIN pg_am am ON am.oid=oc.opcmethod AND am.amname='bm25' JOIN pg_depend d ON d.classid='pg_opclass'::regclass AND d.objid=oc.oid AND d.deptype='e' JOIN pg_extension e ON d.refclassid='pg_extension'::regclass AND e.oid=d.refobjid AND e.extname='pg_search'),
 'text_config',(SELECT value->>'default_text_search_config' FROM configuration),
 'vector_schema',(SELECT n.nspname FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace WHERE e.extname='vector'),
 'embedding_dimensions',(SELECT (value->>'embedding_dim')::integer FROM configuration)),
 'extension_owner',(SELECT pg_get_userbyid(extowner) FROM pg_extension WHERE extname='pgokf'),
 'embedding_rows_present',EXISTS(SELECT 1 FROM pgokf.concept_embedding),
 'indexes',(SELECT coalesce(jsonb_agg(value ORDER BY identity),'[]'::jsonb) FROM indexes WHERE identity IN ('pgokf.concepts_bm25_idx','pgokf.concept_embedding_hnsw_idx')),
 'relation_indexes',(SELECT coalesce(jsonb_agg(value ORDER BY identity),'[]'::jsonb) FROM indexes WHERE identity NOT IN ('pgokf.concepts_bm25_idx','pgokf.concept_embedding_hnsw_idx')),
 'roles',(SELECT coalesce(jsonb_agg(value ORDER BY identity),'[]'::jsonb) FROM roles),
 'memberships',(SELECT coalesce(jsonb_agg(value ORDER BY role,member,grantor),'[]'::jsonb) FROM memberships),
 'rowtype_dependents',(SELECT coalesce(jsonb_agg(identity ORDER BY identity),'[]'::jsonb) FROM dependents)
);
