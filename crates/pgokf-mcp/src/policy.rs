// SPDX-License-Identifier: AGPL-3.0-only
//! The one policy point (spec §2): which tools a principal may see and may
//! call, decided from the [`PrincipalContext`] and the registry - nothing
//! else. `tools/list` filters by the same answer `tools/call` enforces, so
//! a client is never shown a tool it cannot call.
//!
//! Two kinds of answer come out of here:
//!
//! - **Legacy JSON-RPC refusals** (`-32001` role, `-32602` host-only), the
//!   exact 0.3.1 semantics the golden fixtures pin. A tool the registry has
//!   never heard of is unknown, not forbidden: it is not refused here and
//!   the dispatch reports it in-band, exactly as 0.3.1 did.
//! - **Availability** (spec §4.5): a tool whose process connection this
//!   server does not hold is neither advertised nor callable. The write and
//!   admin tools need the writer connection; `build_workspace_plugin`
//!   needs the build connection. Calling one anyway fails with the stable
//!   `writer_unavailable` code and the 0.3.1 message.

use serde_json::Value;

use crate::registry::{self, Availability, ToolSpec};
use crate::rpc::INVALID_PARAMS;
use crate::service::{DomainError, PrincipalContext};

/// JSON-RPC error code for a call the caller's role does not allow. The
/// specification reserves -32000 to -32099 for the server's own errors.
pub(crate) const NOT_PERMITTED: i64 = -32001;

/// Why a write or admin tool cannot run on a server with no writer
/// connection. The 0.3.1 message, kept: the code changed, the tone did not.
pub(crate) const WRITER_UNAVAILABLE: &str = "this MCP server has no writer connection, so it \
     cannot change the catalog: start it with --writer-url (OKF_PG_WRITER_URL) naming a \
     pgokf_writer-capable role";

/// Why a build cannot run on a server with no build connection. Every
/// current process holds one, so this is defence in depth, not a path a
/// caller reaches.
const BUILD_UNAVAILABLE: &str = "this MCP server has no build connection, so it cannot build \
     workspace plugins";

/// Who may see and call what, given the connections this process holds.
pub(crate) struct PolicyService {
    build: bool,
    writer: bool,
}

impl PolicyService {
    /// Policy for a process holding (or not holding) each optional
    /// connection.
    #[must_use]
    pub(crate) const fn new(build: bool, writer: bool) -> Self {
        Self { build, writer }
    }

    /// Every metadata/read projection enters the same authorization boundary.
    #[allow(clippy::unused_self)]
    pub(crate) fn authorize_read(&self, principal: &PrincipalContext) -> Result<(), DomainError> {
        if principal.role < crate::tokens::Role::Reader {
            return Err(DomainError::PermissionDenied("read not permitted".into()));
        }
        Ok(())
    }

    /// Whether the connection a tool needs exists.
    #[must_use]
    fn available(&self, spec: &ToolSpec) -> bool {
        match spec.availability {
            Availability::Reader => true,
            Availability::Build => self.build,
            Availability::Writer => self.writer,
        }
    }

    /// The tools this principal may call: at or above their minimum role,
    /// and only what this process could actually run (spec §4.5 -
    /// advertised capability equals executable capability).
    #[must_use]
    pub(crate) fn visible_tools(&self, principal: &PrincipalContext) -> Vec<&'static ToolSpec> {
        registry::all()
            .iter()
            .filter(|spec| principal.role >= spec.minimum_role && self.available(spec))
            .collect()
    }

    /// Authorize invocation at the domain boundary, including callers that
    /// bypass the wire adapter. PostgreSQL remains the second policy layer.
    pub(crate) fn authorize_tool(
        &self,
        principal: &PrincipalContext,
        spec: &'static ToolSpec,
        arguments: &Value,
    ) -> Result<(), DomainError> {
        if principal.role < spec.minimum_role {
            return Err(DomainError::PermissionDenied(
                "this role may not call this tool".into(),
            ));
        }
        if let Some((_, message)) = self.legacy_refusal(principal, spec.name, arguments) {
            return Err(DomainError::PermissionDenied(message));
        }
        self.ensure_available(spec)
    }

    /// That the tool may run here, or the stable refusal.
    ///
    /// # Errors
    ///
    /// `writer_unavailable` when the connection the tool needs does not
    /// exist (the write/admin tools without `--writer-url`).
    pub(crate) fn ensure_available(&self, spec: &'static ToolSpec) -> Result<(), DomainError> {
        if self.available(spec) {
            return Ok(());
        }
        Err(match spec.availability {
            Availability::Writer => DomainError::WriterUnavailable(WRITER_UNAVAILABLE.to_owned()),
            Availability::Build => DomainError::WriterUnavailable(BUILD_UNAVAILABLE.to_owned()),
            Availability::Reader => unreachable!("the reader connection always exists"),
        })
    }

    /// Why this principal may not make this call, if it may not: the
    /// JSON-RPC code and the message to send back, in the exact 0.3.1
    /// semantics. A local principal is never refused. A tool the registry
    /// does not know is unknown rather than forbidden and is not refused
    /// here; the dispatch reports it in-band, as 0.3.1 did.
    #[must_use]
    #[allow(clippy::unused_self)] // Policy entrypoint; availability is enforced separately.
    pub(crate) fn legacy_refusal(
        &self,
        principal: &PrincipalContext,
        tool: &str,
        arguments: &Value,
    ) -> Option<(i64, String)> {
        if !principal.is_remote() {
            return None;
        }
        if let Some(spec) = registry::by_name(tool)
            && principal.role < spec.minimum_role
        {
            return Some((
                NOT_PERMITTED,
                format!(
                    "the {} role may not call {}; a token with the {} role can",
                    principal.role,
                    registry::echo(tool),
                    spec.minimum_role
                ),
            ));
        }
        let refused = registry::host_only_arguments()
            .iter()
            .find(|name| arguments.get(name.as_str()).is_some_and(|v| !v.is_null()))?;
        Some((
            INVALID_PARAMS,
            format!(
                "'{refused}' acts on this server's own filesystem, not yours, and is refused \
                 over HTTP; leave it out and the files come back in the result"
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::AuthMode;
    use crate::tokens::Role;
    use serde_json::json;

    fn principal(role: Role) -> PrincipalContext {
        PrincipalContext {
            principal_id: "ab".repeat(32),
            token_name: Some("fleet".to_owned()),
            tenant: None,
            role,
            auth_mode: AuthMode::BearerToken,
        }
    }

    fn names(tools: &[&'static ToolSpec]) -> Vec<&'static str> {
        tools.iter().map(|spec| spec.name).collect()
    }

    /// The full matrix, with every connection held: the 0.3.1 role ladder
    /// for the 14 legacy tools, plus `traverse_concepts` at reader level.
    #[test]
    fn visibility_is_the_0_3_1_role_ladder_with_traverse_concepts_a_reader_tool() {
        // Arrange
        let policy = PolicyService::new(true, true);
        let reader = names(&policy.visible_tools(&principal(Role::Reader)));
        let builder = names(&policy.visible_tools(&principal(Role::Builder)));
        let writer = names(&policy.visible_tools(&principal(Role::Writer)));
        let admin = names(&policy.visible_tools(&principal(Role::Admin)));

        // Assert: reader sees the six legacy reads and traverse_concepts.
        for tool in [
            "concept_search",
            "find_similar",
            "concept_neighbors",
            "traverse_concepts",
            "get_concept",
            "get_skill",
            "check_workspace_plugin_freshness",
        ] {
            assert!(reader.contains(&tool), "reader: {tool}");
        }
        assert_eq!(reader.len(), 7);
        for tool in ["list_plugin_targets", "build_workspace_plugin"] {
            assert!(!reader.contains(&tool), "reader: {tool}");
            assert!(builder.contains(&tool), "builder: {tool}");
        }
        for tool in ["list_bundles", "put_document", "delete_document"] {
            assert!(!builder.contains(&tool), "builder: {tool}");
            assert!(writer.contains(&tool), "writer: {tool}");
        }
        for tool in [
            "create_content_bundle",
            "refresh_bundle",
            "set_bundle_state",
        ] {
            assert!(!writer.contains(&tool), "writer: {tool}");
            assert!(admin.contains(&tool), "admin: {tool}");
        }
        assert_eq!(builder.len(), 9);
        assert_eq!(writer.len(), 12);
        assert_eq!(admin.len(), 15, "the top of the ladder sees every tool");
        assert!(
            admin.contains(&"concept_search") && writer.contains(&"get_skill"),
            "a ladder: the higher roles read too"
        );
    }

    #[test]
    fn the_local_principal_sees_what_the_process_can_run() {
        // Arrange / Act
        let full = PolicyService::new(true, true).visible_tools(&PrincipalContext::local());
        let read_only = PolicyService::new(true, false).visible_tools(&PrincipalContext::local());

        // Assert
        assert_eq!(full.len(), 15);
        assert_eq!(read_only.len(), 10, "the five writer tools drop out");
    }

    #[test]
    fn without_a_writer_connection_the_write_tools_are_neither_shown_nor_callable() {
        // Arrange
        let policy = PolicyService::new(true, false);
        let admin = principal(Role::Admin);
        let shown = names(&policy.visible_tools(&admin));

        // Act / Assert: hidden from the listing...
        for tool in [
            "put_document",
            "delete_document",
            "create_content_bundle",
            "refresh_bundle",
            "set_bundle_state",
        ] {
            assert!(
                !shown.contains(&tool),
                "advertised without a writer: {tool}"
            );
            let error = policy
                .ensure_available(registry::by_name(tool).expect("registered"))
                .expect_err("not callable");
            assert_eq!(error.code(), "writer_unavailable", "{tool}");
            assert_eq!(error.message(), WRITER_UNAVAILABLE, "the 0.3.1 tone");
        }
        // ...but list_bundles runs on the reader connection (0.3.1 policy).
        assert!(shown.contains(&"list_bundles"));
        assert!(
            policy
                .ensure_available(registry::by_name("list_bundles").expect("registered"))
                .is_ok()
        );
        // The build connection gates only build_workspace_plugin.
        let no_build = PolicyService::new(false, true);
        assert!(
            no_build
                .ensure_available(registry::by_name("build_workspace_plugin").expect("registered"))
                .is_err()
        );
    }

    #[test]
    fn a_role_that_may_not_call_a_tool_is_told_which_one_can() {
        // Arrange / Act
        let refusal = PolicyService::new(true, true).legacy_refusal(
            &principal(Role::Reader),
            "build_workspace_plugin",
            &json!({}),
        );

        // Assert
        let (code, message) = refusal.expect("refused");
        assert_eq!(code, NOT_PERMITTED);
        assert!(
            message.contains("the reader role may not call"),
            "{message}"
        );
        assert!(message.contains("the builder role can"), "{message}");
    }

    #[test]
    fn a_tool_nobody_has_heard_of_is_not_reported_as_a_role_refusal() {
        // Arrange / Act: it is unknown, not forbidden, and the dispatch
        // says so in-band on both transports, as 0.3.1 did.
        let policy = PolicyService::new(true, true);
        let reader = policy.legacy_refusal(&principal(Role::Reader), "drop_everything", &json!({}));
        let builder =
            policy.legacy_refusal(&principal(Role::Builder), "drop_everything", &json!({}));

        // Assert
        assert!(reader.is_none() && builder.is_none());
        assert!(registry::by_name("drop_everything").is_none());
    }

    #[test]
    fn an_argument_that_would_write_on_this_host_is_named_and_refused() {
        // Arrange
        let policy = PolicyService::new(true, true);
        let builder = principal(Role::Builder);

        // Act
        let with_dir = policy.legacy_refusal(
            &builder,
            "build_workspace_plugin",
            &json!({ "target": "claude-code", "output_dir": "/etc" }),
        );
        let with_overwrite = policy.legacy_refusal(
            &builder,
            "build_workspace_plugin",
            &json!({ "overwrite": true }),
        );
        let plain = policy.legacy_refusal(
            &builder,
            "build_workspace_plugin",
            &json!({ "target": "claude-code" }),
        );
        let null = policy.legacy_refusal(
            &builder,
            "build_workspace_plugin",
            &json!({ "output_dir": null }),
        );
        let locally = PolicyService::new(true, true).legacy_refusal(
            &PrincipalContext::local(),
            "build_workspace_plugin",
            &json!({ "output_dir": "/tmp/here" }),
        );

        // Assert
        assert!(
            with_dir
                .is_some_and(|(code, why)| code == INVALID_PARAMS && why.contains("'output_dir'"))
        );
        assert!(with_overwrite.is_some_and(|(_, why)| why.contains("'overwrite'")));
        assert!(plain.is_none() && null.is_none());
        assert!(
            locally.is_none(),
            "stdio writes on the client's own machine"
        );
    }
}
