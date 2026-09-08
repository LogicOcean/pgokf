// SPDX-License-Identifier: AGPL-3.0-only
//! The web UI's identity state, kept in the catalog rather than in files
//! beside it: the people a local sign-in knows (`pgokf_web.users`) and the
//! sessions the UI has issued and not yet ended (`pgokf_web.sessions`), and
//! the bearer tokens that may call `pgokf-mcp` over HTTP
//! (`pgokf_web.mcp_tokens`, minted on the UI's Admin page).
//!
//! The extension owns these tables but never reads them; `pgokf-web` does.
//! They live in the database for the reasons everything else does: a change
//! is transactional (ending a session is one `DELETE`, with no file to race
//! or half-write), every UI instance sees the same state, and `pg_dump`
//! carries them with the rest of the catalog. None of them is catalog
//! content, so none is tenant-scoped; a token may name the tenant its
//! endpoint serves, which is a label the MCP server checks, not a policy the
//! database enforces.
//!
//! Least privilege: `pgokf_writer` (and so `pgokf_admin`, which inherits it)
//! may read and write all three; `pgokf_reader` sees none of them, since it
//! must not learn password hashes, session identifiers, or which tokens
//! exist. The UI therefore reaches them through its writer connection,
//! which any identity mode that persists state requires. The one thing a
//! reader may ask is `pgokf.mcp_token_bearer(digest)`: the name and role
//! behind one digest it already holds, which is how the MCP server - a
//! reader - authenticates a request without the token ever reaching the
//! database.

use pgrx::extension_sql;

extension_sql!(
    r"
CREATE SCHEMA pgokf_web;
REVOKE ALL ON SCHEMA pgokf_web FROM PUBLIC;
GRANT USAGE ON SCHEMA pgokf_web TO pgokf_writer;
COMMENT ON SCHEMA pgokf_web IS
    'The web UI''s identity state: the people a local sign-in knows, the sessions the UI has issued, and the bearer tokens the MCP server accepts over HTTP. Owned by the extension so it is transactional, shared by every UI instance, and dumped with the catalog; read and written by pgokf_writer only, and never by the extension itself.';

CREATE TABLE pgokf_web.users (
    name          text        NOT NULL,
    role          text        NOT NULL
        CONSTRAINT users_role_check
        CHECK (role IN ('viewer', 'uploader', 'editor', 'approver', 'admin')),
    password_hash text        NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT users_pkey PRIMARY KEY (name),
    CONSTRAINT users_name_check CHECK (name ~ '^[A-Za-z0-9._@+-]{1,128}$')
);

CREATE TABLE pgokf_web.sessions (
    nonce      text        NOT NULL,
    subject    text        NOT NULL,
    mode       text        NOT NULL
        CONSTRAINT sessions_mode_check CHECK (mode IN ('users', 'oidc')),
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT sessions_pkey PRIMARY KEY (nonce)
);
CREATE INDEX sessions_subject_idx ON pgokf_web.sessions (subject);
CREATE INDEX sessions_expires_at_idx ON pgokf_web.sessions (expires_at);

REVOKE ALL ON TABLE pgokf_web.users, pgokf_web.sessions FROM PUBLIC;
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE pgokf_web.users, pgokf_web.sessions TO pgokf_writer;

COMMENT ON TABLE pgokf_web.users IS
    'People the web UI''s users identity mode signs in: one row per person with their role on the viewer < uploader < editor < approver < admin ladder and an Argon2id hash of their password. Managed by pgokf-web (its user add / set-password commands and the Admin page); pgokf_writer only, so a reader never sees a hash.';
COMMENT ON COLUMN pgokf_web.users.name IS
    'The sign-in name: one plain token of letters, digits, and . _ @ + - (at most 128), which is also the person''s OKF actor (human:<name>).';
COMMENT ON COLUMN pgokf_web.users.role IS
    'The role the UI grants on every request: viewer, uploader, editor, approver, or admin (each holds everything below it). A session cookie carries no role, so a change here takes effect at once.';
COMMENT ON COLUMN pgokf_web.users.password_hash IS
    'An Argon2id PHC string of the password. A fingerprint of it is bound into every session the person opens, so a changed password ends the sessions opened before it.';
COMMENT ON COLUMN pgokf_web.users.created_at IS 'When the person was added.';
COMMENT ON COLUMN pgokf_web.users.updated_at IS 'When the role or password last changed.';

COMMENT ON TABLE pgokf_web.sessions IS
    'The sessions the web UI has issued and not yet ended, in either local identity mode (users or oidc). A session cookie is signed, so the UI could always verify one but never forget one; this table is its memory: a cookie whose nonce is not here is refused, so signing out, sign out everywhere, or an admin ending someone''s sessions takes effect on every device at once. pgokf_writer only: a session identifier is not for readers.';
COMMENT ON COLUMN pgokf_web.sessions.nonce IS 'The random session identifier the signed cookie carries.';
COMMENT ON COLUMN pgokf_web.sessions.subject IS 'Whose session it is: the users-mode name or the provider''s subject claim.';
COMMENT ON COLUMN pgokf_web.sessions.mode IS 'The identity mode that opened it (users or oidc); a mode never honours the other''s sessions.';
COMMENT ON COLUMN pgokf_web.sessions.expires_at IS 'When the session ends by itself; expired rows are pruned as new sessions are opened.';
COMMENT ON COLUMN pgokf_web.sessions.created_at IS 'When the person signed in.';

CREATE TABLE pgokf_web.mcp_tokens (
    name       text        NOT NULL,
    role       text        NOT NULL
        CONSTRAINT mcp_tokens_role_check CHECK (role IN ('reader', 'builder')),
    tenant     text
        CONSTRAINT mcp_tokens_tenant_check CHECK (tenant ~ '^[^[:cntrl:]]{1,128}$'),
    digest     text        NOT NULL
        CONSTRAINT mcp_tokens_digest_check CHECK (digest ~ '^[0-9a-f]{64}$'),
    created_by text        NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT mcp_tokens_pkey PRIMARY KEY (digest),
    CONSTRAINT mcp_tokens_name_key UNIQUE NULLS NOT DISTINCT (tenant, name),
    CONSTRAINT mcp_tokens_name_check CHECK (name ~ '^[A-Za-z0-9._@+-]{1,128}$')
);
REVOKE ALL ON TABLE pgokf_web.mcp_tokens FROM PUBLIC;
GRANT SELECT, INSERT, DELETE ON TABLE pgokf_web.mcp_tokens TO pgokf_writer;

COMMENT ON TABLE pgokf_web.mcp_tokens IS
    'The bearer tokens that may call pgokf-mcp over HTTP: one row per token with what to call it in the log, its role (reader searches and reads; builder may also build workspace plugins), the tenant it was minted for, and the SHA-256 digest of the token - never the token, which is shown once when it is minted. The digest is the token''s identity; a name is unique within its tenant. Minted and revoked by pgokf-web (the Admin page, or its mcp-token command), each UI seeing its own tenant''s tokens; pgokf_writer only. A reader learns the bearer of one digest through pgokf.mcp_token_bearer(), and nothing else.';
COMMENT ON COLUMN pgokf_web.mcp_tokens.name IS
    'What the token is called in the log beside every call it makes: one plain token of letters, digits, and . _ @ + - (at most 128), unique within its tenant.';
COMMENT ON COLUMN pgokf_web.mcp_tokens.role IS
    'What the token may do: reader (search and read the catalog) or builder (also build workspace plugins). The MCP server decides per tool from this.';
COMMENT ON COLUMN pgokf_web.mcp_tokens.tenant IS
    'The tenant the token was minted for - the pgokf.tenant scope of the UI or command that minted it - or NULL for a catalog served without one; one to 128 printable characters. An MCP endpoint accepts only tokens minted for its own tenant, so one process serves one tenant with tokens of its own; this is a label the server checks, not a policy the database enforces.';
COMMENT ON COLUMN pgokf_web.mcp_tokens.digest IS
    'The SHA-256 of the token, as 64 lower-case hex characters, and the row''s identity. A token is 256 random bits, so a fast hash is the right way to store it; the token itself is never kept.';
COMMENT ON COLUMN pgokf_web.mcp_tokens.created_by IS 'Who minted it: the admin''s sign-in name or subject, or cli.';
COMMENT ON COLUMN pgokf_web.mcp_tokens.created_at IS 'When it was minted.';

CREATE FUNCTION pgokf.mcp_token_bearer(digest text)
RETURNS TABLE (name text, role text, tenant text)
LANGUAGE sql STABLE STRICT
SECURITY DEFINER SET search_path = pg_catalog, pg_temp
AS $mcp_token_bearer$
    SELECT t.name, t.role, t.tenant
    FROM pgokf_web.mcp_tokens AS t
    WHERE t.digest = mcp_token_bearer.digest
$mcp_token_bearer$;
REVOKE ALL ON FUNCTION pgokf.mcp_token_bearer(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.mcp_token_bearer(text) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.mcp_token_bearer(text) IS
    'The name, role, and tenant of the MCP token whose SHA-256 digest this is, or no row; the server accepts only a token minted for its own tenant. How pgokf-mcp, which connects as a reader, authenticates a request over HTTP: it hashes the presented token itself and asks for that digest, so the token never travels to the database and a reader learns the bearer of a digest it holds and nothing about any other. SECURITY DEFINER over pgokf_web.mcp_tokens, which no reader may see; STABLE, STRICT, executable by pgokf_reader. A revoked token is refused with the very next request: nothing is cached.';
",
    name = "web_identity_tables",
    requires = ["catalog_tables"]
);
