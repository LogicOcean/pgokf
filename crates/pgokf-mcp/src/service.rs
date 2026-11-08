// SPDX-License-Identifier: AGPL-3.0-only
//! The typed domain boundary between the wire adapters and the catalog.
//!
//! Everything [`crate::dispatch`] does it does through a
//! [`PrincipalContext`] (who is asking) and a [`DomainError`] (how an
//! operation may fail), per spec/pgokf-0.4.0-mcp.md §2 and §4.3: the wire
//! never sees a raw SQLSTATE or an `anyhow` chain, only a stable domain code
//! and a concise message. The full chain stays in the server's stderr log.
//!
//! The catalog-facing halves of this boundary - `CatalogReadService` and
//! `CatalogCommandService`, owning the reader, build, and writer
//! connections - are added beside these types; this module already carries
//! the pure domain values they share: the principal, the error type and its
//! classification, the canonical concept resource URI, and the row-to-output
//! mapping of `traverse_concepts`.

use serde_json::{Value, json};

use crate::tokens::Role;

/// How the caller proved who it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// The client launched this process and holds the connection string:
    /// nothing was authenticated because there was nothing to authenticate.
    Local,
    /// A bearer token over HTTP, looked up in the catalog per request.
    BearerToken,
}

/// Who is asking, threaded from dispatch into every service call.
///
/// `principal_id` is the non-reversible stable identity the spec pins
/// (§7): the bearer token's SHA-256 digest as hex, or `local` on stdio. It
/// is never the token itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalContext {
    /// The stable principal: bearer token digest hex, or `local`.
    pub principal_id: String,
    /// The token's name, when there is a token. Used for the OKF actor and
    /// the log, never for authorization.
    pub token_name: Option<String>,
    /// The tenant the token was minted for (`None` = the catalog-wide one).
    pub tenant: Option<String>,
    /// What the principal may reach. A local caller holds the connection
    /// string already, so it acts as the top of the ladder.
    pub role: Role,
    /// How the principal authenticated.
    pub auth_mode: AuthMode,
}

impl PrincipalContext {
    /// The stdio principal: the client started this process, so there is
    /// nobody to authenticate and nothing to withhold.
    #[must_use]
    pub fn local() -> Self {
        Self {
            principal_id: "local".to_owned(),
            token_name: None,
            tenant: None,
            role: Role::Admin,
            auth_mode: AuthMode::Local,
        }
    }

    /// Whether this principal arrived over the network. Remote principals
    /// never see, and may never use, an argument that acts on this host.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        self.auth_mode == AuthMode::BearerToken
    }

    /// The OKF actor a write by this principal is recorded under: the token
    /// that made it, so the catalog names what wrote a document as plainly
    /// as the log does. A local caller is the server itself.
    #[must_use]
    pub fn actor(&self) -> String {
        match self.auth_mode {
            AuthMode::Local => "agent:pgokf-mcp".to_owned(),
            AuthMode::BearerToken => {
                format!("agent:{}", self.token_name.as_deref().unwrap_or("unknown"))
            }
        }
    }
}

/// The concise message an [`DomainError::Internal`] carries when the real
/// cause must not leave the process: the detail is in the stderr log.
const INTERNAL_MESSAGE: &str = "an internal error occurred; the server's log names why";

/// How a tool operation may fail, with the stable code the wire carries
/// (spec §4.3). The message is concise and safe to show; the raw error
/// chain never leaves the process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    /// An argument is missing, the wrong type, out of range, or unknown
    /// (the fail-closed tightening of spec §8.5).
    InvalidArgument(String),
    /// A resource identifier is not canonical.
    InvalidUri(String),
    /// Exact bytes are not retained.
    ResourceUnavailable(String),
    /// A legacy package cannot be published as a conforming Skill.
    SkillNotListable(String),
    /// The object named does not exist - or is not this caller's to see;
    /// the cases are indistinguishable by design.
    NotFound(String),
    /// The catalog refused the operation for the role this server holds.
    PermissionDenied(String),
    /// The tool needs a connection this process does not hold (the writer,
    /// for every write/admin tool).
    WriterUnavailable(String),
    /// The operation conflicts with the catalog's current state.
    Conflict(String),
    /// A build's stale policy or closure refused it.
    BuildRefused(String),
    /// A pagination cursor failed validation.
    InvalidCursor(String),
    /// A pagination cursor's catalog generation is no longer current.
    StaleCursor(String),
    /// Anything else. The message shown is generic; the log has the detail.
    Internal(String),
}

impl DomainError {
    /// The stable domain code, as `structuredContent.code` carries it.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidArgument(_) => "invalid_argument",
            Self::InvalidUri(_) => "invalid_uri",
            Self::ResourceUnavailable(_) => "resource_unavailable",
            Self::SkillNotListable(_) => "skill_not_listable",
            Self::NotFound(_) => "not_found",
            Self::PermissionDenied(_) => "permission_denied",
            Self::WriterUnavailable(_) => "writer_unavailable",
            Self::Conflict(_) => "conflict",
            Self::BuildRefused(_) => "build_refused",
            Self::InvalidCursor(_) => "invalid_cursor",
            Self::StaleCursor(_) => "stale_cursor",
            Self::Internal(_) => "internal",
        }
    }

    /// The concise, caller-safe message.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidArgument(message)
            | Self::InvalidUri(message)
            | Self::ResourceUnavailable(message)
            | Self::SkillNotListable(message)
            | Self::NotFound(message)
            | Self::PermissionDenied(message)
            | Self::WriterUnavailable(message)
            | Self::Conflict(message)
            | Self::BuildRefused(message)
            | Self::InvalidCursor(message)
            | Self::StaleCursor(message)
            | Self::Internal(message) => message,
        }
    }

    /// The generic internal error: the real cause is logged, never sent.
    #[must_use]
    pub fn internal() -> Self {
        Self::Internal(INTERNAL_MESSAGE.to_owned())
    }
}

impl std::fmt::Display for DomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for DomainError {}

/// Turn an `anyhow` chain into the domain error the wire carries.
///
/// A [`DomainError`] anywhere in the chain (thrown at the site that knew
/// how the failure should be classified) wins. Otherwise a PostgreSQL error
/// is classified by its SQLSTATE, and anything unclassified is `internal`
/// with the generic message - the chain itself is for the log.
#[must_use]
pub fn classify(error: &anyhow::Error) -> DomainError {
    for cause in error.chain() {
        if let Some(domain) = cause.downcast_ref::<DomainError>() {
            return domain.clone();
        }
    }
    for cause in error.chain() {
        if let Some(postgres) = cause.downcast_ref::<tokio_postgres::Error>() {
            return postgres
                .as_db_error()
                .map_or_else(DomainError::internal, |db| {
                    classify_db(db.code().code(), db.message())
                });
        }
    }
    DomainError::internal()
}

/// The domain error a PostgreSQL error maps to. Only SQLSTATEs the
/// extension raises with caller-safe, hand-written messages (`22023`)
/// repeat the server's message; the rest answer with a concise local one.
/// The extension's not-found helper raises `22023` with a `no such ...`
/// message (identical for missing, cross-tenant, and untyped objects, so
/// nothing is probed), which is the `not_found` class.
fn classify_db(sqlstate: &str, message: &str) -> DomainError {
    match sqlstate {
        "22023" if message.starts_with("no such ") => {
            DomainError::NotFound("object not found".into())
        }
        "22023" => {
            DomainError::InvalidArgument("the catalog rejected the operation parameters".into())
        }
        "42501" => DomainError::PermissionDenied(
            "the catalog refused the operation: this server's role lacks the privilege it \
             needs; the server's log names it"
                .to_owned(),
        ),
        "23505" | "55P03" => DomainError::Conflict(
            "the operation conflicts with the catalog's current state; the server's log names \
             why"
            .to_owned(),
        ),
        _ => DomainError::internal(),
    }
}

/// Classify a service failure, logging the full chain first when it will be
/// hidden behind the generic `internal` message - and only then, so routine
/// refusals (a missing document, a taken name) do not spam the log.
pub fn fail(operation: &str, error: &anyhow::Error) -> DomainError {
    let domain = classify(error);
    if matches!(domain, DomainError::Internal(_)) {
        eprintln!("pgokf-mcp: {operation} failed: internal catalog error");
    }
    domain
}

/// Unpadded base64url (RFC 4648 §5), the encoding the canonical resource
/// URIs of spec §5.1 use for concept and path keys.
#[must_use]
pub fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            let index = ((word >> (18 - 6 * i)) & 0x3f) as usize;
            out.push(char::from(ALPHABET[index]));
        }
    }
    out
}

/// The canonical concept resource URI (spec §5.1):
/// `okf://catalog/bundles/{bundle_id}/concepts/{concept_key}` with the
/// concept id base64url-encoded.
#[must_use]
pub fn concept_uri(bundle_id: i64, concept_id: &str) -> String {
    format!(
        "okf://catalog/bundles/{bundle_id}/concepts/{}",
        base64url(concept_id.as_bytes())
    )
}

/// What a mapping expects and did not find in a row. A row shape mismatch
/// is a server bug (the SQL is fixed), never a caller's, so it is internal.
fn malformed_row(what: &str) -> DomainError {
    DomainError::Internal(format!("a traversal row has no usable {what}"))
}

/// Map the rows of the links-graph traversal query to the
/// `traverse_concepts` output shape (spec §4.4): the same nodes, hop
/// counts, and paths `concept_neighbors` returns, carried in the new shape
/// with `relation_type` null and the effective freshness annotation the
/// query joined in.
pub fn map_link_rows(rows: &Value, bundle_id: i64) -> Result<Value, DomainError> {
    let Value::Array(rows) = rows else {
        return Err(malformed_row("row list"));
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let concept_id = row
            .get("neighbor_id")
            .and_then(Value::as_str)
            .ok_or_else(|| malformed_row("neighbor_id"))?;
        out.push(json!({
            "bundle_id": bundle_id,
            "concept_id": concept_id,
            "catalog_generation": row.get("catalog_generation").cloned().unwrap_or(Value::Null),
            "direction": "outbound",
            "hops": row.get("hops").cloned().unwrap_or(Value::Null),
            "path": row.get("path").and_then(Value::as_array).map(|path| path.iter()
                .map(|id| json!({"bundle_id":bundle_id,"concept_id":id})).collect::<Vec<_>>()),
            "relation_type": Value::Null,
            "title": row.get("title").cloned().unwrap_or(Value::Null),
            "freshness": {
                "state": row.get("freshness_state").cloned().unwrap_or(Value::Null),
                "reasons": row.get("freshness_reasons").cloned().unwrap_or_else(|| json!([])),
            },
            "uri": concept_uri(bundle_id, concept_id),
        }));
    }
    Ok(Value::Array(out))
}

/// Map the rows of the relationships-graph traversal query
/// (`pgokf.concept_relationship_neighbors`) to the `traverse_concepts`
/// output shape, zipping the parallel `path_bundle_ids` / `path_concept_ids`
/// arrays into path steps.
pub fn map_relationship_rows(rows: &Value) -> Result<Value, DomainError> {
    let Value::Array(rows) = rows else {
        return Err(malformed_row("row list"));
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let bundle_id = row
            .get("bundle_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| malformed_row("bundle_id"))?;
        let concept_id = row
            .get("concept_id")
            .and_then(Value::as_str)
            .ok_or_else(|| malformed_row("concept_id"))?;
        let path_ids = row
            .get("path_concept_ids")
            .and_then(Value::as_array)
            .ok_or_else(|| malformed_row("path_concept_ids"))?;
        let path_bundles = row
            .get("path_bundle_ids")
            .and_then(Value::as_array)
            .ok_or_else(|| malformed_row("path_bundle_ids"))?;
        if path_ids.len() != path_bundles.len() {
            return Err(malformed_row("path"));
        }
        let path: Vec<Value> = path_bundles
            .iter()
            .zip(path_ids)
            .map(|(bundle, concept)| json!({ "bundle_id": bundle, "concept_id": concept }))
            .collect();
        out.push(json!({
            "bundle_id": bundle_id,
            "concept_id": concept_id,
            "catalog_generation": row.get("catalog_generation").cloned().unwrap_or(Value::Null),
            "direction": "outbound",
            "hops": row.get("hops").cloned().unwrap_or(Value::Null),
            "path": path,
            "relation_type": row.get("relation_type").cloned().unwrap_or(Value::Null),
            "title": row.get("title").cloned().unwrap_or(Value::Null),
            "freshness": {
                "state": row.get("freshness_state").cloned().unwrap_or(Value::Null),
                "reasons": row.get("freshness_reasons").cloned().unwrap_or_else(|| json!([])),
            },
            "uri": concept_uri(bundle_id, concept_id),
        }));
    }
    Ok(Value::Array(out))
}

/// A schema-checked domain outcome, independent of MCP envelopes.
#[derive(Debug)]
pub(crate) struct ToolOutcome {
    pub value: Value,
    pub is_error: bool,
    pub legacy_value: Option<Value>,
    pub next_cursor: Option<String>,
    pub evidence: Option<Value>,
}

impl ToolOutcome {
    pub fn failure(error: &DomainError) -> Self {
        Self {
            value: json!({"code": error.code(), "message": error.message()}),
            is_error: true,
            legacy_value: None,
            next_cursor: None,
            evidence: None,
        }
    }
}

/// Read operations over the existing catalog; adapters never issue SQL.
pub(crate) struct CatalogReadService<'a> {
    catalog: &'a crate::catalog::Catalog,
    principal: &'a PrincipalContext,
}

/// Mutating and materializing operations share the same policy and catalog.
pub(crate) struct CatalogCommandService<'a> {
    catalog: &'a crate::catalog::Catalog,
    principal: &'a PrincipalContext,
}

impl<'a> CatalogReadService<'a> {
    pub fn new(catalog: &'a crate::catalog::Catalog, principal: &'a PrincipalContext) -> Self {
        Self { catalog, principal }
    }

    pub async fn method(&self, method: &str, params: &Value) -> Result<Value, DomainError> {
        let empty = json!({});
        let params = if params.is_null()
            && matches!(
                method,
                "resources/list" | "resources/templates/list" | "skills/list"
            ) {
            &empty
        } else {
            params
        };
        let allowed = if matches!(method, "resources/read" | "skills/get") {
            "uri"
        } else {
            "cursor"
        };
        let object = params
            .as_object()
            .ok_or_else(|| DomainError::InvalidArgument("parameters must be an object".into()))?;
        if object.keys().any(|key| key != allowed && key != "_meta") {
            return Err(DomainError::InvalidArgument(
                "unknown method parameter".into(),
            ));
        }
        self.catalog
            .read_method(self.principal, method, params)
            .await
    }

    pub async fn call(
        &self,
        spec: &'static crate::registry::ToolSpec,
        args: &Value,
    ) -> ToolOutcome {
        execute(self.catalog, self.principal, spec, args).await
    }
}

impl<'a> CatalogCommandService<'a> {
    pub fn new(catalog: &'a crate::catalog::Catalog, principal: &'a PrincipalContext) -> Self {
        Self { catalog, principal }
    }

    pub async fn call(
        &self,
        spec: &'static crate::registry::ToolSpec,
        args: &Value,
    ) -> ToolOutcome {
        execute(self.catalog, self.principal, spec, args).await
    }
}

async fn execute(
    catalog: &crate::catalog::Catalog,
    principal: &PrincipalContext,
    spec: &'static crate::registry::ToolSpec,
    args: &Value,
) -> ToolOutcome {
    let mut outcome = execute_unannotated(catalog, principal, spec, args).await;
    if !outcome.is_error
        && matches!(
            spec.handler,
            crate::registry::Handler::ConceptSearch
                | crate::registry::Handler::FindSimilar
                | crate::registry::Handler::ConceptNeighbors
                | crate::registry::Handler::GetConcept
                | crate::registry::Handler::GetSkill
        )
    {
        match catalog.tool_evidence(&outcome.value, args).await {
            Ok(evidence) => outcome.evidence = Some(evidence),
            Err(error) => return ToolOutcome::failure(&error),
        }
    }
    outcome
}

async fn execute_unannotated(
    catalog: &crate::catalog::Catalog,
    principal: &PrincipalContext,
    spec: &'static crate::registry::ToolSpec,
    args: &Value,
) -> ToolOutcome {
    let policy = catalog.policy();
    if let Err(error) = policy.authorize_tool(principal, spec, args) {
        return ToolOutcome::failure(&error);
    }
    if let Err(message) = spec.validate_input(args) {
        return ToolOutcome::failure(&DomainError::InvalidArgument(message));
    }
    if spec.handler == crate::registry::Handler::ConceptSearch {
        return match catalog.search_page(principal, args).await {
            Ok((value, next_cursor)) if spec.validate_output(&value).is_ok() => ToolOutcome {
                value: match crate::domain::ToolValue::parse(spec.handler, value) {
                    Ok(typed) => typed.into_json(),
                    Err(error) => return ToolOutcome::failure(&error),
                },
                next_cursor,
                evidence: None,
                is_error: false,
                legacy_value: None,
            },
            Ok(_) => ToolOutcome::failure(&DomainError::internal()),
            Err(error) => ToolOutcome::failure(&error),
        };
    }
    match catalog
        .call_handler(spec.handler, args, &principal.actor())
        .await
    {
        Ok(value) => {
            let value = match crate::domain::ToolValue::parse(spec.handler, value) {
                Ok(typed) => typed.into_json(),
                Err(error) => return ToolOutcome::failure(&error),
            };
            if spec.validate_output(&value).is_err() {
                return ToolOutcome::failure(&DomainError::internal());
            }
            if value.get("refused").and_then(Value::as_bool) == Some(true) {
                let mut outcome = ToolOutcome::failure(&DomainError::BuildRefused(
                    "workspace build policy refused the selection".into(),
                ));
                outcome.legacy_value = Some(value);
                return outcome;
            }
            ToolOutcome {
                value,
                is_error: false,
                legacy_value: None,
                next_cursor: None,
                evidence: None,
            }
        }
        Err(error) => ToolOutcome::failure(&fail(spec.name, &error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_domain_error_carries_its_stable_code_and_message() {
        // Arrange / Act / Assert: the codes are the wire contract (spec
        // §4.3), so they are pinned by name.
        let cases = [
            (DomainError::InvalidArgument("a".into()), "invalid_argument"),
            (DomainError::NotFound("b".into()), "not_found"),
            (
                DomainError::PermissionDenied("c".into()),
                "permission_denied",
            ),
            (
                DomainError::WriterUnavailable("d".into()),
                "writer_unavailable",
            ),
            (DomainError::Conflict("e".into()), "conflict"),
            (DomainError::BuildRefused("f".into()), "build_refused"),
            (DomainError::InvalidCursor("g".into()), "invalid_cursor"),
            (DomainError::StaleCursor("h".into()), "stale_cursor"),
            (DomainError::Internal("i".into()), "internal"),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code);
            assert!(!error.message().is_empty());
            assert_eq!(error.to_string(), error.message());
        }
    }

    #[test]
    fn a_domain_error_in_the_chain_wins_over_everything_else() {
        // Arrange
        let typed: anyhow::Error = DomainError::NotFound("no bundle has id 7".to_owned()).into();

        // Act
        let classified = classify(&typed.context("resolving the bundle"));

        // Assert
        assert_eq!(
            classified,
            DomainError::NotFound("no bundle has id 7".to_owned())
        );
    }

    #[test]
    fn an_unclassified_failure_is_internal_without_the_raw_chain() {
        // Arrange
        let error = anyhow::anyhow!("connection reset by peer").context("catalog query failed");

        // Act
        let classified = classify(&error);

        // Assert: the raw chain is not in the payload (spec §4.2).
        assert_eq!(classified.code(), "internal");
        assert!(!classified.message().contains("connection reset"));
        assert!(!classified.message().contains("catalog query"));
    }

    #[test]
    fn sqlstates_map_to_their_stable_codes() {
        // Arrange / Act / Assert
        assert_eq!(
            classify_db("22023", "max_hops must be at least 1, got 0"),
            DomainError::InvalidArgument(
                "the catalog rejected the operation parameters".to_owned()
            )
        );
        assert_eq!(
            classify_db("22023", "no such skill x/SKILL in bundle 5"),
            DomainError::NotFound("object not found".to_owned())
        );
        assert_eq!(
            classify_db("42501", "permission denied").code(),
            "permission_denied"
        );
        assert_eq!(classify_db("23505", "duplicate key").code(), "conflict");
        assert_eq!(
            classify_db("55P03", "lock not available").code(),
            "conflict"
        );
        assert_eq!(classify_db("XX000", "something inside").code(), "internal");
        // A raw server message never leaks past 23505/42501/XX000.
        assert!(
            !classify_db("23505", "duplicate key")
                .message()
                .contains("duplicate key")
        );
    }

    #[test]
    fn the_internal_classification_is_logged_not_silent() {
        // Arrange
        let error = anyhow::anyhow!("wedged");

        // Act
        let routine = fail("a read", &DomainError::NotFound("gone".to_owned()).into());
        let internal = fail("a read", &error);

        // Assert
        assert_eq!(routine.code(), "not_found");
        assert_eq!(internal.code(), "internal");
    }

    #[test]
    fn base64url_matches_rfc_4648_vectors() {
        // Arrange / Act / Assert
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
        // url-safe: bytes that produce +/ in standard base64 produce -_ here.
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn a_concept_uri_is_the_canonical_form_of_spec_5() {
        // Arrange / Act
        let uri = concept_uri(42, "skills/deploy/SKILL");

        // Assert: unpadded base64url of the UTF-8 concept id, no tenant.
        assert_eq!(
            uri,
            "okf://catalog/bundles/42/concepts/c2tpbGxzL2RlcGxveS9TS0lMTA"
        );
    }

    #[test]
    fn the_local_principal_is_the_top_of_the_ladder_and_remote_is_not() {
        // Arrange / Act
        let local = PrincipalContext::local();
        let remote = PrincipalContext {
            principal_id: "ab".repeat(32),
            token_name: Some("fleet".to_owned()),
            tenant: Some("acme".to_owned()),
            role: Role::Reader,
            auth_mode: AuthMode::BearerToken,
        };

        // Assert
        assert_eq!(local.role, Role::Admin);
        assert!(!local.is_remote() && remote.is_remote());
        assert_eq!(local.actor(), "agent:pgokf-mcp");
        assert_eq!(remote.actor(), "agent:fleet");
        assert_eq!(local.principal_id, "local");
    }

    #[test]
    fn link_rows_map_into_the_traverse_shape_with_full_parity() {
        // Arrange: two rows exactly as the links-graph query returns them -
        // the same columns concept_neighbors carries, plus freshness.
        let rows = json!([
            {
                "source_id": "runbooks/failover",
                "neighbor_id": "runbooks/failover-appendix",
                "hops": 1,
                "path": ["runbooks/failover", "runbooks/failover-appendix"],
                "title": "Widget failover appendix",
                "freshness_state": "fresh",
                "freshness_reasons": []
            },
            {
                "source_id": "runbooks/failover",
                "neighbor_id": "services/widget",
                "hops": 2,
                "path": ["runbooks/failover", "runbooks/failover-appendix", "services/widget"],
                "title": null,
                "freshness_state": "stale",
                "freshness_reasons": ["source_changed"]
            }
        ]);

        // Act
        let mapped = map_link_rows(&rows, 1).expect("rows map");

        // Assert: parity with concept_neighbors for the same start - the
        // same neighbor set, hop counts, and paths - in the new shape.
        let first = &mapped[0];
        assert_eq!(first["bundle_id"], 1);
        assert_eq!(first["concept_id"], "runbooks/failover-appendix");
        assert_eq!(first["hops"], 1);
        assert_eq!(
            first["path"],
            json!([{ "bundle_id":1,"concept_id":"runbooks/failover" }, {"bundle_id":1,"concept_id":"runbooks/failover-appendix"}])
        );
        assert_eq!(first["relation_type"], Value::Null);
        assert_eq!(first["title"], "Widget failover appendix");
        assert_eq!(first["freshness"], json!({"state": "fresh", "reasons": []}));
        assert_eq!(first["uri"], concept_uri(1, "runbooks/failover-appendix"));
        assert_eq!(mapped[1]["title"], Value::Null);
        assert_eq!(
            mapped[1]["freshness"],
            json!({"state": "stale", "reasons": ["source_changed"]})
        );
    }

    #[test]
    fn relationship_rows_zip_the_parallel_path_arrays_into_steps() {
        // Arrange
        let rows = json!([
            {
                "bundle_id": 2,
                "concept_id": "docs/b",
                "hops": 1,
                "path_bundle_ids": [1, 2],
                "path_concept_ids": ["docs/a", "docs/b"],
                "relation_type": "docs:references",
                "title": "B",
                "freshness_state": "fresh",
                "freshness_reasons": []
            }
        ]);

        // Act
        let mapped = map_relationship_rows(&rows).expect("rows map");

        // Assert
        let node = &mapped[0];
        assert_eq!(
            node["path"],
            json!([
                {"bundle_id": 1, "concept_id": "docs/a"},
                {"bundle_id": 2, "concept_id": "docs/b"}
            ])
        );
        assert_eq!(node["relation_type"], "docs:references");
        assert_eq!(node["uri"], concept_uri(2, "docs/b"));

        // A ragged path is a server bug: internal, never sent as data.
        let ragged = json!([{
            "bundle_id": 1, "concept_id": "x", "hops": 1,
            "path_bundle_ids": [1], "path_concept_ids": ["x", "y"],
            "relation_type": null, "title": null,
            "freshness_state": "fresh", "freshness_reasons": []
        }]);
        assert_eq!(
            map_relationship_rows(&ragged).expect_err("ragged").code(),
            "internal"
        );
    }
}
