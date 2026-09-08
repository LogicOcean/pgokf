// SPDX-License-Identifier: AGPL-3.0-only
//! The web UI's identity state, kept in the catalog rather than in files
//! beside it: the people a local sign-in knows (`pgokf_web.users`) and the
//! sessions the UI has issued and not yet ended (`pgokf_web.sessions`).
//!
//! The extension owns these tables but never reads them; `pgokf-web` does.
//! They live in the database for the reasons everything else does: a change
//! is transactional (ending a session is one `DELETE`, with no file to race
//! or half-write), every UI instance sees the same state, and `pg_dump`
//! carries them with the rest of the catalog. Neither is catalog content,
//! so neither is tenant-scoped.
//!
//! Least privilege: `pgokf_writer` (and so `pgokf_admin`, which inherits it)
//! may read and write both; `pgokf_reader` sees neither, since it must not
//! learn password hashes or session identifiers. The UI therefore reaches
//! them through its writer connection, which any identity mode that
//! persists state requires.

use pgrx::extension_sql;

extension_sql!(
    r"
CREATE SCHEMA pgokf_web;
REVOKE ALL ON SCHEMA pgokf_web FROM PUBLIC;
GRANT USAGE ON SCHEMA pgokf_web TO pgokf_writer;
COMMENT ON SCHEMA pgokf_web IS
    'The web UI''s identity state: the people a local sign-in knows and the sessions the UI has issued. Owned by the extension so it is transactional, shared by every UI instance, and dumped with the catalog; read and written by pgokf_writer only, and never by the extension itself.';

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
",
    name = "web_identity_tables",
    requires = ["catalog_tables"]
);
