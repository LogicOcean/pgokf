// SPDX-License-Identifier: AGPL-3.0-only
//! Catalog Resources and Skills over stored metadata and audited exact bytes.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_postgres::{GenericClient, types::ToSql};

use crate::service::DomainError;
use crate::uri::ResourceUri;

const MAX_BYTES: u64 = 16 * 1024 * 1024;
const PAGE_SIZE: usize = 50;
const TENANT: &str = "(((nullif(current_setting('pgokf.tenant', true), '') IS NULL) AND NOT pgokf.tenant_required()) OR b.tenant_id = current_setting('pgokf.tenant', true))";
const ACTIVE: &str = "b.enabled AND b.retired_at IS NULL";

/// A page and its private keyset position. The caller authenticates the key.
pub(crate) struct Page {
    pub result: Value,
    pub next: Option<Value>,
}

fn missing() -> DomainError {
    DomainError::NotFound("resource not found".into())
}
fn unavailable() -> DomainError {
    DomainError::ResourceUnavailable(
        "stored resource bytes are unavailable or fail integrity verification".into(),
    )
}
fn invalid_uri() -> DomainError {
    DomainError::InvalidUri("a canonical catalog URI is required".into())
}
fn sql_error(error: tokio_postgres::Error) -> DomainError {
    crate::service::fail("resource catalog query", &error.into())
}
fn digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}
fn render(uri: &ResourceUri) -> Result<String, DomainError> {
    uri.render().map_err(|_| invalid_uri())
}

async fn query_json(
    client: &(impl GenericClient + Sync),
    sql: &str,
    args: &[&(dyn ToSql + Sync)],
) -> Result<Value, DomainError> {
    client
        .query_opt(sql, args)
        .await
        .map_err(sql_error)?
        .ok_or_else(missing)?
        .try_get(0)
        .map_err(sql_error)
}

/// Snapshot coordinate; all tenant-visible bundles participate, including state changes.
pub(crate) async fn generation(
    client: &(impl GenericClient + Sync),
) -> Result<String, DomainError> {
    let sql = format!(
        "SELECT encode(sha256(convert_to(coalesce(string_agg(
        jsonb_build_array(b.id,b.catalog_generation,b.enabled,b.retired_at,b.sync_hash)::text,
        ',' ORDER BY b.id), ''), 'UTF8')), 'hex') FROM pgokf.bundles b WHERE {TENANT}"
    );
    client
        .query_one(&sql, &[])
        .await
        .map_err(sql_error)?
        .try_get(0)
        .map_err(sql_error)
}

pub(crate) async fn call(
    client: &(impl GenericClient + Sync),
    method: &str,
    params: &Value,
) -> Result<Value, DomainError> {
    match method {
        "resources/templates/list" => Ok(templates()),
        "resources/read" => {
            let raw = params
                .get("uri")
                .and_then(Value::as_str)
                .ok_or_else(invalid_uri)?;
            let uri = ResourceUri::parse(raw).map_err(|_| invalid_uri())?;
            let value = read(client, raw, uri).await?;
            Ok(json!({"contents": [value]}))
        }
        "skills/get" => {
            let raw = params
                .get("uri")
                .and_then(Value::as_str)
                .ok_or_else(invalid_uri)?;
            let ResourceUri::SkillFile {
                bundle_id,
                concept_id,
                skill_name,
                member_path,
            } = ResourceUri::parse(raw).map_err(|_| invalid_uri())?
            else {
                return Err(invalid_uri());
            };
            if member_path != "SKILL.md" {
                return Err(invalid_uri());
            }
            let package = package(client, bundle_id, &concept_id).await?;
            if package.name != skill_name {
                return Err(missing());
            }
            Ok(json!({"skill": package.entry()?}))
        }
        _ => Err(DomainError::InvalidArgument(
            "unknown resource operation".into(),
        )),
    }
}

pub(crate) async fn list(
    client: &(impl GenericClient + Sync),
    method: &str,
    after: &Value,
) -> Result<Page, DomainError> {
    match method {
        "resources/list" => list_resources(client, after).await,
        "skills/list" => list_skills(client, after).await,
        _ => Err(DomainError::InvalidArgument(
            "unknown list operation".into(),
        )),
    }
}

fn position(after: &Value, skills: bool) -> Result<(i64, String), DomainError> {
    if after.is_null() {
        return Ok((0, String::new()));
    }
    let fail = || DomainError::InvalidCursor("invalid cursor position".into());
    let array = after.as_array().ok_or_else(fail)?;
    if array.len() != if skills { 2 } else { 1 } {
        return Err(fail());
    }
    let id = array[0].as_i64().filter(|n| *n > 0).ok_or_else(fail)?;
    let name = if skills {
        array[1].as_str().ok_or_else(fail)?.to_owned()
    } else {
        String::new()
    };
    Ok((id, name))
}

async fn list_resources(
    client: &(impl GenericClient + Sync),
    after: &Value,
) -> Result<Page, DomainError> {
    let (id, _) = position(after, false)?;
    let mut resources = Vec::new();
    if after.is_null() {
        resources.push(json!({"uri":"okf://catalog/manifest", "name":"pgokf catalog", "mimeType":"application/vnd.pgokf.catalog+json"}));
    }
    let available = PAGE_SIZE - resources.len();
    let limit = i64::try_from(available + 1).map_err(|_| DomainError::internal())?;
    let sql = format!(
        "SELECT b.id, b.name FROM pgokf.bundles b WHERE {TENANT} AND {ACTIVE}
                       AND b.id > $1 ORDER BY b.id LIMIT $2"
    );
    let rows = client
        .query(&sql, &[&id, &limit])
        .await
        .map_err(sql_error)?;
    let more = rows.len() > available;
    let mut last = id;
    for row in rows.iter().take(available) {
        last = row.try_get(0).map_err(sql_error)?;
        let name: Option<String> = row.try_get(1).map_err(sql_error)?;
        let info = bundle(client, last).await?;
        resources.push(
            json!({"uri": render(&ResourceUri::Bundle {bundle_id:last})?,
            "name":name.unwrap_or_else(|| format!("Bundle {last}")),
            "mimeType":"application/vnd.pgokf.bundle+json", "_meta":evidence(&info)}),
        );
    }
    Ok(Page {
        result: json!({"resources":resources}),
        next: more.then(|| json!([last])),
    })
}

async fn list_skills(
    client: &(impl GenericClient + Sync),
    after: &Value,
) -> Result<Page, DomainError> {
    let (id, key) = position(after, true)?;
    let sql = format!(
        "SELECT s.bundle_id, s.concept_id FROM pgokf.skills s
        JOIN pgokf.bundles b ON b.id=s.bundle_id WHERE {TENANT} AND {ACTIVE}
        AND (s.bundle_id,s.concept_id COLLATE \"C\") > ($1,$2::text COLLATE \"C\")
        ORDER BY s.bundle_id,s.concept_id COLLATE \"C\" LIMIT 51"
    );
    let rows = client.query(&sql, &[&id, &key]).await.map_err(sql_error)?;
    let more = rows.len() > PAGE_SIZE;
    let mut entries = Vec::new();
    let mut last = Value::Null;
    for row in rows.iter().take(PAGE_SIZE) {
        let bundle_id: i64 = row.try_get(0).map_err(sql_error)?;
        let concept_id: String = row.try_get(1).map_err(sql_error)?;
        last = json!([bundle_id, concept_id]);
        let result = match package(client, bundle_id, &concept_id).await {
            Ok(package) => package.entry(),
            Err(error) => Err(error),
        };
        match result {
            Ok(entry) => entries.push(entry),
            Err(DomainError::SkillNotListable(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Page {
        result: json!({"skills":entries}),
        next: more.then_some(last),
    })
}

fn templates() -> Value {
    let entries = [
        (
            "okf://catalog/bundles/{bundle_id}",
            "Bundle descriptor",
            "application/vnd.pgokf.bundle+json",
        ),
        (
            "okf://catalog/bundles/{bundle_id}/concepts/{concept_key}",
            "Concept metadata",
            "application/vnd.pgokf.concept+json",
        ),
        (
            "okf://catalog/bundles/{bundle_id}/documents/{path_key}",
            "Exact stored document",
            "text/markdown; charset=utf-8",
        ),
        (
            "skill://pgokf/b/{bundle_id}/c/{concept_key}/{skill_name}/SKILL.md",
            "Skill manifest",
            "text/markdown",
        ),
        (
            "skill://pgokf/b/{bundle_id}/c/{concept_key}/{skill_name}/{+member_path}",
            "Skill package member",
            "application/octet-stream",
        ),
    ];
    json!({"resourceTemplates":entries.map(|(uri,name,mime)| json!({"uriTemplate":uri,"name":name,"mimeType":mime}))})
}

fn evidence(value: &Value) -> Value {
    let mut meta = json!({});
    for (source, target) in [
        ("bundle_id", "bundleId"),
        ("concept_id", "conceptId"),
        ("file_hash", "fileHash"),
        ("package_hash", "packageHash"),
        ("sync_hash", "syncHash"),
        ("catalog_generation", "catalogGeneration"),
        ("freshness", "freshness"),
    ] {
        if let Some(field) = value.get(source) {
            meta[format!("io.logicocean.pgokf/{target}")] = field.clone();
        }
    }
    meta
}

async fn bundle(client: &(impl GenericClient + Sync), id: i64) -> Result<Value, DomainError> {
    let sql = format!("SELECT jsonb_build_object('schema','pgokf.bundle.v1','bundle_id',b.id,
        'name',b.name,'source_type',b.source_type,'state','active','file_count',b.file_count,
        'sync_hash',b.sync_hash,'catalog_generation',b.catalog_generation,
        'freshness',jsonb_build_object('state',coalesce(f.state,'unknown'),'reasons',coalesce(f.reasons,'{{}}'::text[])))
        FROM pgokf.bundles b LEFT JOIN pgokf.effective_freshness f ON f.bundle_id=b.id AND f.scope_kind='bundle'
        WHERE b.id=$1 AND {TENANT} AND {ACTIVE}");
    query_json(client, &sql, &[&id]).await
}

/// Bounded identity projection for compatibility Tools. Ambiguous legacy
/// neighbor IDs retain all visible composite identities rather than guessing.
pub(crate) async fn tool_evidence(
    client: &(impl GenericClient + Sync),
    identities: &Value,
) -> Result<Value, DomainError> {
    let sql = format!("SELECT coalesce(jsonb_agg(item ORDER BY bundle_id, concept_id),'[]'::jsonb) FROM (
        SELECT DISTINCT c.bundle_id, c.id AS concept_id,
        jsonb_build_object('bundle_id',c.bundle_id,'concept_id',c.id,
          'catalog_generation',b.catalog_generation,'sync_hash',b.sync_hash,
          'freshness',jsonb_build_object('state',coalesce(fc.state,fp.state,fb.state,'unknown'),
          'reasons',coalesce(fc.reasons,fp.reasons,fb.reasons,'{{}}'::text[]))) AS item
        FROM jsonb_to_recordset($1::jsonb) AS i(bundle_id bigint,concept_id text)
        JOIN pgokf.concepts c ON c.id=i.concept_id AND (i.bundle_id IS NULL OR c.bundle_id=i.bundle_id)
        JOIN pgokf.bundles b ON b.id=c.bundle_id
        LEFT JOIN pgokf.effective_freshness fc ON fc.bundle_id=c.bundle_id AND fc.scope_kind='concept' AND fc.scope_key=c.id
        LEFT JOIN pgokf.effective_freshness fp ON fp.bundle_id=c.bundle_id AND fp.scope_kind='path' AND fp.scope_key=c.path
        LEFT JOIN pgokf.effective_freshness fb ON fb.bundle_id=c.bundle_id AND fb.scope_kind='bundle'
        WHERE {TENANT} AND {ACTIVE}) observed");
    let mut rows = query_json(client, &sql, &[identities]).await?;
    for row in rows.as_array_mut().ok_or_else(DomainError::internal)? {
        row["uri"] = json!(render(&ResourceUri::Concept {
            bundle_id: row["bundle_id"]
                .as_i64()
                .ok_or_else(DomainError::internal)?,
            concept_id: row["concept_id"]
                .as_str()
                .ok_or_else(DomainError::internal)?
                .into(),
        })?);
    }
    let typed: Vec<crate::domain::CatalogEvidence> =
        serde_json::from_value(rows).map_err(|_| DomainError::internal())?;
    serde_json::to_value(typed).map_err(|_| DomainError::internal())
}

async fn concept(
    client: &(impl GenericClient + Sync),
    id: i64,
    key: &str,
) -> Result<Value, DomainError> {
    let sql = format!("SELECT jsonb_build_object('schema','pgokf.concept.v1','bundle_id',c.bundle_id,
        'concept_id',c.id,'path',c.path,'type',c.type,'title',c.title,'description',c.description,
        'tags',c.tags,'resource',c.resource,'body_text',c.body_text,'modified_at',c.modified_at,
        'file_hash',c.file_hash,'status',p.status,'trust_tier',p.trust_tier,
        'sync_hash',b.sync_hash,'catalog_generation',b.catalog_generation,
        'retained',EXISTS(SELECT 1 FROM pgokf.concept_source s WHERE s.bundle_id=c.bundle_id AND s.concept_id=c.id),
        'freshness',jsonb_build_object('state',coalesce(fc.state,fp.state,fb.state,'unknown'),
          'reasons',coalesce(fc.reasons,fp.reasons,fb.reasons,'{{}}'::text[])))
        FROM pgokf.concepts c JOIN pgokf.bundles b ON b.id=c.bundle_id
        LEFT JOIN pgokf.concept_provenance p ON p.bundle_id=c.bundle_id AND p.concept_id=c.id
        LEFT JOIN pgokf.effective_freshness fc ON fc.bundle_id=c.bundle_id AND fc.scope_kind='concept' AND fc.scope_key=c.id
        LEFT JOIN pgokf.effective_freshness fp ON fp.bundle_id=c.bundle_id AND fp.scope_kind='path' AND fp.scope_key=c.path
        LEFT JOIN pgokf.effective_freshness fb ON fb.bundle_id=c.bundle_id AND fb.scope_kind='bundle'
        WHERE c.bundle_id=$1 AND c.id=$2 AND {TENANT} AND {ACTIVE}");
    let mut value = query_json(client, &sql, &[&id, &key]).await?;
    if value["retained"] == true {
        let path = value["path"].as_str().ok_or_else(DomainError::internal)?;
        value["document_uri"] = json!(render(&ResourceUri::Document {
            bundle_id: id,
            path: path.into()
        })?);
    }
    value
        .as_object_mut()
        .ok_or_else(DomainError::internal)?
        .remove("retained");
    Ok(value)
}

fn content(
    uri: &str,
    mime: &str,
    bytes: &[u8],
    expected: Option<(&str, u64)>,
    mut meta: Value,
) -> Result<Value, DomainError> {
    let size = u64::try_from(bytes.len()).map_err(|_| unavailable())?;
    if size > MAX_BYTES {
        return Err(unavailable());
    }
    let hash = digest(bytes);
    if let Some((expected_hash, expected_size)) = expected
        && (hash != expected_hash || size != expected_size)
    {
        return Err(unavailable());
    }
    meta["io.logicocean.pgokf/sha256"] = json!(format!("sha256:{hash}"));
    meta["io.logicocean.pgokf/byteSize"] = json!(size);
    let mut value = json!({"uri":uri,"mimeType":mime,"_meta":meta});
    let text_mime = mime.starts_with("text/")
        || mime.contains("json")
        || mime.contains("xml")
        || mime.contains("yaml")
        || mime.contains("javascript");
    if let Ok(text) = std::str::from_utf8(bytes)
        && text_mime
    {
        value["text"] = json!(text);
    } else {
        value["blob"] = json!(STANDARD.encode(bytes));
    }
    Ok(value)
}

async fn read(
    client: &(impl GenericClient + Sync),
    raw: &str,
    uri: ResourceUri,
) -> Result<Value, DomainError> {
    match uri {
        ResourceUri::Catalog => {
            let mut value = query_json(client,"SELECT jsonb_build_object('schema','pgokf.catalog.v1','extension_version',pgokf.version(),'capabilities',pgokf.capabilities())",&[]).await?;
            value["server_version"] = json!(env!("CARGO_PKG_VERSION"));
            value["catalog_generation"] = json!(generation(client).await?);
            content(
                raw,
                "application/vnd.pgokf.catalog+json",
                &serde_json::to_vec(&value).map_err(|_| DomainError::internal())?,
                None,
                evidence(&value),
            )
        }
        ResourceUri::Bundle { bundle_id } => {
            let value = bundle(client, bundle_id).await?;
            content(
                raw,
                "application/vnd.pgokf.bundle+json",
                &serde_json::to_vec(&value).map_err(|_| DomainError::internal())?,
                None,
                evidence(&value),
            )
        }
        ResourceUri::Concept {
            bundle_id,
            concept_id,
        } => {
            let value = concept(client, bundle_id, &concept_id).await?;
            content(
                raw,
                "application/vnd.pgokf.concept+json",
                &serde_json::to_vec(&value).map_err(|_| DomainError::internal())?,
                None,
                evidence(&value),
            )
        }
        ResourceUri::Document { bundle_id, path } => {
            read_document(client, raw, bundle_id, &path).await
        }
        ResourceUri::SkillFile {
            bundle_id,
            concept_id,
            skill_name,
            member_path,
        } => {
            read_skill(
                client,
                raw,
                bundle_id,
                &concept_id,
                &skill_name,
                &member_path,
            )
            .await
        }
    }
}

async fn read_document(
    client: &(impl GenericClient + Sync),
    raw: &str,
    id: i64,
    path: &str,
) -> Result<Value, DomainError> {
    let sql = format!(
        "SELECT c.id,s.byte_size::bigint FROM pgokf.concepts c
        JOIN pgokf.bundles b ON b.id=c.bundle_id
        JOIN pgokf.concept_source s ON s.bundle_id=c.bundle_id AND s.concept_id=c.id
        WHERE c.bundle_id=$1 AND c.path=$2 AND {TENANT} AND {ACTIVE}"
    );
    let row = client
        .query_opt(&sql, &[&id, &path])
        .await
        .map_err(sql_error)?
        .ok_or_else(missing)?;
    let key: String = row.try_get(0).map_err(sql_error)?;
    let size: i64 = row.try_get(1).map_err(sql_error)?;
    if !(0..=i64::try_from(MAX_BYTES).map_err(|_| unavailable())?).contains(&size) {
        return Err(unavailable());
    }
    let meta = evidence(&concept(client, id, &key).await?);
    let bytes: Vec<u8> = client
        .query_one("SELECT pgokf.get_concept_source($1,$2)", &[&id, &key])
        .await
        .map_err(sql_error)?
        .try_get(0)
        .map_err(sql_error)?;
    if u64::try_from(size).ok() != u64::try_from(bytes.len()).ok() {
        return Err(unavailable());
    }
    let mime = if std::str::from_utf8(&bytes).is_ok() && !bytes.contains(&0) {
        "text/markdown; charset=utf-8"
    } else {
        "application/octet-stream"
    };
    content(raw, mime, &bytes, None, meta)
}

struct Member {
    concept_id: String,
    path: String,
    hash: String,
    size: u64,
    mime: String,
    script: bool,
}

struct Package {
    bundle_id: i64,
    concept_id: String,
    name: String,
    root: String,
    frontmatter: Value,
    hash: String,
    size: u64,
    members: Vec<Member>,
    meta: Value,
}

fn listable_name(name: &str, root: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && root.rsplit('/').next() == Some(name)
}

fn not_listable(reason: &str) -> DomainError {
    DomainError::SkillNotListable(format!("skill cannot be listed: {reason}"))
}

fn file_descriptor(uri: &str, hash: &str, size: u64) -> Result<Value, DomainError> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(unavailable());
    }
    Ok(json!({"uri":uri,"digest":format!("sha256:{hash}"),"size":size}))
}

impl Package {
    fn uri(&self, path: &str) -> Result<String, DomainError> {
        render(&ResourceUri::SkillFile {
            bundle_id: self.bundle_id,
            concept_id: self.concept_id.clone(),
            skill_name: self.name.clone(),
            member_path: path.into(),
        })
        .map_err(|_| not_listable("a package member cannot be represented by a canonical URI"))
    }

    fn entry(&self) -> Result<Value, DomainError> {
        if !listable_name(&self.name, &self.root) {
            return Err(not_listable(
                "name must match the package directory and Agent Skills naming rules",
            ));
        }
        if self
            .frontmatter
            .get("description")
            .and_then(Value::as_str)
            .is_none_or(|text| text.trim().is_empty() || text.chars().count() > 1024)
        {
            return Err(not_listable(
                "frontmatter description must contain 1 to 1024 characters",
            ));
        }
        if self.members.len() >= 512 {
            return Err(not_listable("512-file limit exceeded"));
        }
        let total = self
            .members
            .iter()
            .try_fold(self.size, |size, member| size.checked_add(member.size));
        if total.is_none_or(|size| size > MAX_BYTES) {
            return Err(not_listable("16 MiB package limit exceeded"));
        }
        let uri = self.uri("SKILL.md")?;
        let mut files = vec![file_descriptor(&uri, &self.hash, self.size)?];
        let mut seen = std::collections::BTreeSet::from(["SKILL.md"]);
        for member in &self.members {
            if !seen.insert(member.path.as_str()) {
                return Err(not_listable("duplicate package member path"));
            }
            files.push(file_descriptor(
                &self.uri(&member.path)?,
                &member.hash,
                member.size,
            )?);
        }
        Ok(json!({"uri":uri,"frontmatter":self.frontmatter,"resources":files,"_meta":self.meta}))
    }
}

async fn package(
    client: &(impl GenericClient + Sync),
    id: i64,
    key: &str,
) -> Result<Package, DomainError> {
    let sql = format!("SELECT s.agent_skill,s.package_root,s.skill_md_sha256,s.skill_md_byte_size,
        jsonb_build_object('bundle_id',s.bundle_id,'concept_id',s.concept_id,
          'file_hash',s.source_file_hash,'package_hash',s.package_hash,'sync_hash',b.sync_hash,
          'catalog_generation',b.catalog_generation,
          'freshness',jsonb_build_object('state',coalesce(fc.state,fp.state,fb.state,'unknown'),
          'reasons',coalesce(fc.reasons,fp.reasons,fb.reasons,'{{}}'::text[])))
        FROM pgokf.skills s JOIN pgokf.bundles b ON b.id=s.bundle_id
        JOIN pgokf.concepts c ON c.bundle_id=s.bundle_id AND c.id=s.concept_id
        LEFT JOIN pgokf.effective_freshness fc ON fc.bundle_id=c.bundle_id AND fc.scope_kind='concept' AND fc.scope_key=c.id
        LEFT JOIN pgokf.effective_freshness fp ON fp.bundle_id=c.bundle_id AND fp.scope_kind='path' AND fp.scope_key=c.path
        LEFT JOIN pgokf.effective_freshness fb ON fb.bundle_id=c.bundle_id AND fb.scope_kind='bundle'
        WHERE s.bundle_id=$1 AND s.concept_id=$2 AND {TENANT} AND {ACTIVE}");
    let row = client
        .query_opt(&sql, &[&id, &key])
        .await
        .map_err(sql_error)?
        .ok_or_else(missing)?;
    let frontmatter: Value = row.try_get(0).map_err(sql_error)?;
    let name = frontmatter
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let size: i64 = row.try_get(3).map_err(sql_error)?;
    let meta: Value = row.try_get(4).map_err(sql_error)?;
    let members = client
        .query(
            "SELECT concept_id,source_path,executable_sha256,byte_size,
         CASE language WHEN 'python' THEN 'text/x-python; charset=utf-8'
           WHEN 'bash' THEN 'text/x-shellscript; charset=utf-8'
           WHEN 'shell' THEN 'text/x-shellscript; charset=utf-8'
           WHEN 'javascript' THEN 'text/javascript; charset=utf-8'
           ELSE 'text/plain; charset=utf-8' END,true
         FROM pgokf.scripts WHERE bundle_id=$1 AND package_concept_id=$2
         UNION ALL
         SELECT concept_id,source_path,content_sha256,byte_size,media_type,false
         FROM pgokf.reference_documents WHERE bundle_id=$1 AND package_concept_id=$2
         ORDER BY 2 LIMIT 512",
            &[&id, &key],
        )
        .await
        .map_err(sql_error)?;
    let members = members
        .iter()
        .map(|row| {
            let size: i64 = row.try_get(3).map_err(sql_error)?;
            Ok(Member {
                concept_id: row.try_get(0).map_err(sql_error)?,
                path: row.try_get(1).map_err(sql_error)?,
                hash: row.try_get(2).map_err(sql_error)?,
                size: u64::try_from(size).map_err(|_| unavailable())?,
                mime: row.try_get(4).map_err(sql_error)?,
                script: row.try_get(5).map_err(sql_error)?,
            })
        })
        .collect::<Result<Vec<_>, DomainError>>()?;
    Ok(Package {
        bundle_id: id,
        concept_id: key.into(),
        name,
        frontmatter,
        root: row.try_get(1).map_err(sql_error)?,
        hash: row.try_get(2).map_err(sql_error)?,
        size: u64::try_from(size).map_err(|_| unavailable())?,
        members,
        meta: evidence(&meta),
    })
}

async fn read_skill(
    client: &(impl GenericClient + Sync),
    raw: &str,
    id: i64,
    key: &str,
    name: &str,
    path: &str,
) -> Result<Value, DomainError> {
    let package = package(client, id, key).await?;
    if package.name != name {
        return Err(missing());
    }
    // Validate the same complete manifest and interoperability bounds as discovery.
    package.entry()?;
    let mut meta = package.meta.clone();
    let (bytes, hash, size, mime) = if path == "SKILL.md" {
        let row = client
            .query_one("SELECT (pgokf.get_skill($1,$2)).skill_md", &[&id, &key])
            .await
            .map_err(sql_error)?;
        (
            row.try_get::<_, Vec<u8>>(0).map_err(sql_error)?,
            package.hash.as_str(),
            package.size,
            "text/markdown",
        )
    } else {
        let member = package
            .members
            .iter()
            .find(|member| member.path == path)
            .ok_or_else(missing)?;
        let member_meta = evidence(&concept(client, id, &member.concept_id).await?);
        meta.as_object_mut()
            .ok_or_else(DomainError::internal)?
            .extend(
                member_meta
                    .as_object()
                    .ok_or_else(DomainError::internal)?
                    .clone(),
            );
        let sql = if member.script {
            "SELECT (pgokf.get_script($1,$2)).exact_bytes"
        } else {
            "SELECT (pgokf.get_reference($1,$2,true)).exact_bytes"
        };
        let row = client
            .query_one(sql, &[&id, &member.concept_id])
            .await
            .map_err(sql_error)?;
        (
            row.try_get::<_, Vec<u8>>(0).map_err(sql_error)?,
            member.hash.as_str(),
            member.size,
            member.mime.as_str(),
        )
    };
    content(raw, mime, &bytes, Some((hash, size)), meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_preserves_sha256_hex_vectors() {
        assert_eq!(
            digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn skill_name_invariant_matches_package_directory() {
        assert!(listable_name("deploy", "skills/deploy"));
        for (name, root) in [
            ("Deploy", "Deploy"),
            ("a--b", "a--b"),
            ("-a", "-a"),
            ("a-", "a-"),
            ("deploy", "skills/other"),
            ("deploy", ""),
        ] {
            assert!(!listable_name(name, root), "{name} / {root}");
        }
        assert!(!listable_name(&"a".repeat(65), &"a".repeat(65)));
    }

    #[test]
    fn content_round_trips_unicode_and_binary_and_rejects_digest_drift() {
        for bytes in ["café\r\n".as_bytes(), &[0, 255, 128]] {
            let hash = digest(bytes);
            let value = content(
                "okf://catalog/manifest",
                "application/octet-stream",
                bytes,
                Some((&hash, bytes.len() as u64)),
                json!({}),
            )
            .unwrap();
            let actual = if let Some(text) = value["text"].as_str() {
                text.as_bytes().to_vec()
            } else {
                STANDARD.decode(value["blob"].as_str().unwrap()).unwrap()
            };
            assert_eq!(actual, bytes);
            assert!(
                content(
                    "u",
                    "m",
                    bytes,
                    Some((&"0".repeat(64), bytes.len() as u64)),
                    json!({})
                )
                .is_err()
            );
            assert!(content("u", "m", bytes, Some((&hash, 100)), json!({})).is_err());
        }
    }

    #[test]
    fn skill_entry_preserves_frontmatter_and_enforces_complete_manifest_bounds() {
        let frontmatter =
            json!({"name":"deploy","description":"Deploy café", "unknown":{"verbatim":true}});
        let mut package = Package {
            bundle_id: 1,
            concept_id: "skills/deploy/SKILL".into(),
            name: "deploy".into(),
            root: "skills/deploy".into(),
            frontmatter: frontmatter.clone(),
            hash: digest(b"skill"),
            size: 5,
            members: vec![],
            meta: json!({}),
        };
        let entry = package.entry().unwrap();
        assert_eq!(entry["frontmatter"], frontmatter);
        assert_eq!(entry["resources"].as_array().unwrap().len(), 1);
        assert_eq!(entry["resources"][0]["uri"], entry["uri"]);
        package.size = MAX_BYTES + 1;
        assert_eq!(package.entry().unwrap_err().code(), "skill_not_listable");
        package.size = 5;
        package
            .frontmatter
            .as_object_mut()
            .unwrap()
            .remove("description");
        assert_eq!(package.entry().unwrap_err().code(), "skill_not_listable");
        package.frontmatter = frontmatter;
        for index in 0..512 {
            package.members.push(Member {
                concept_id: format!("assets/{index}"),
                path: format!("assets/{index}"),
                hash: digest(b""),
                size: 0,
                mime: "application/octet-stream".into(),
                script: false,
            });
        }
        assert_eq!(package.entry().unwrap_err().code(), "skill_not_listable");
        package.members.pop();
        assert_eq!(
            package.entry().unwrap()["resources"]
                .as_array()
                .unwrap()
                .len(),
            512
        );
    }

    #[test]
    fn binary_media_stays_blob_even_when_bytes_are_valid_utf8() {
        let value = content("u", "application/octet-stream", b"ascii", None, json!({})).unwrap();
        assert!(value.get("text").is_none());
        assert_eq!(value["blob"], STANDARD.encode(b"ascii"));
        let text = content(
            "u",
            "text/plain; charset=utf-8",
            "café".as_bytes(),
            None,
            json!({}),
        )
        .unwrap();
        assert_eq!(text["text"], "café");
    }

    #[test]
    fn templates_are_the_five_frozen_classes_without_directory_reads() {
        let templates = templates();
        let entries = templates["resourceTemplates"].as_array().unwrap();
        assert_eq!(entries.len(), 5);
        assert!(entries.iter().any(|v| {
            v["uriTemplate"]
                .as_str()
                .unwrap()
                .ends_with("/{+member_path}")
        }));
    }
}
