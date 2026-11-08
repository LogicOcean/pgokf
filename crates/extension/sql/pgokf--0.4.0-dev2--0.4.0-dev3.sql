-- SPDX-License-Identifier: AGPL-3.0-only
-- pgokf extension upgrade: 0.4.0-dev2 -> 0.4.0-dev3
--
-- Web identity metadata reconciliation. The field catalog that carried the
-- search-function drift reconciled by pgokf--0.4.0-dev1--0.4.0-dev2.sql was
-- created by an early unpublished development build and hand-migrated when
-- pgokf_web.users gained display_name/provider and pgokf_web.sessions gained
-- provider. Compared with a fresh install it also carries:
--
--   * seven superseded comments, from before identity providers could be
--     many: the pgokf_web schema, the pgokf_web.users table, and its name,
--     role, password_hash, created_at and updated_at columns;
--   * the appended physical column order of pgokf_web.users (display_name,
--     provider after updated_at) and pgokf_web.sessions (provider after
--     created_at), and so the attribute order of their row types.
--
-- The comments are restored here, verbatim from the web_identity_tables
-- block of src/catalog/web_identity.rs (api_stability keeps the two
-- identical). On a factory catalog every statement is a no-op.
--
-- The column order is NOT changed. Reordering would mean rebuilding the
-- tables that hold the site's people and live sessions inside the update,
-- for no behavioral gain: every statement pgokf ships names its columns, and
-- nothing depends on either row type. It is instead the one declared catalog
-- exception (tests/upgrade/catalog-exceptions.json, "Declared catalog
-- exceptions" in docs/api-stability.md): exactly these two tables, exactly
-- the fresh or the recorded appended order, every other column property
-- (name, type, default, nullability, collation, generated, identity, ACL,
-- comment) equal to a fresh install. The pgokf-verifier positional-check gate
-- refuses positional access to either table or row type in shipped code, and
-- the pgokf-verifier parity gate replays the product's own statements against
-- both orders.
--
-- The 0.4.0-dev2 edge's claim that a drifted catalog ends object-for-object
-- equal to a fresh install holds for the search functions it reconciles;
-- with this step, every object of a catalog in either state ends equal to a
-- fresh 0.4.0-dev3 install except for that declared column order.
--
-- Never DROP, TRUNCATE, DELETE, or rewrite existing catalog data in an upgrade
-- script: doing so would break the no-data-loss guarantee asserted by the
-- api_stability upgrade tests. Comments carry no data.

COMMENT ON SCHEMA pgokf_web IS
    'The web UI''s identity state: the people a local sign-in knows, the sessions the UI has issued, the bearer tokens the MCP server accepts over HTTP, and the identity providers an admin set up. Owned by the extension so it is transactional, shared by every UI instance, and dumped with the catalog; read and written by pgokf_writer only, and never by the extension itself.';
COMMENT ON TABLE pgokf_web.users IS
    'People the web UI''s users identity mode signs in: one row per person with their role on the viewer < uploader < editor < approver < admin ladder and an Argon2id hash of their password - or, for a person an identity provider signed in, no password and the provider that brought them, their row appearing at their first sign-in. A name belongs to exactly one way in. Managed by pgokf-web (its user add / set-password commands and the Admin page); pgokf_writer only, so a reader never sees a hash.';
COMMENT ON COLUMN pgokf_web.users.name IS
    'The sign-in name: one plain token of letters, digits, and . _ @ + - (at most 128), which is also the person''s OKF actor (human:<name>). For a person an identity provider signed in, the identity claim the provider was set up with (sub, login, email...).';
COMMENT ON COLUMN pgokf_web.users.role IS
    'The role the UI grants on every request: viewer, uploader, editor, approver, or admin (each holds everything below it). A session cookie carries no role, so a change here takes effect at once. A person an identity provider signed in holds the higher of this role and the one their groups map to.';
COMMENT ON COLUMN pgokf_web.users.password_hash IS
    'An Argon2id PHC string of the password, or NULL for a person an identity provider signed in: they have no password here, a password sign-in under their name is refused, and their row exists so an admin can see them and set their role. A fingerprint of the hash is bound into every session a password opens, so a changed password ends the sessions opened before it.';
COMMENT ON COLUMN pgokf_web.users.created_at IS 'When the person was added, or first signed in.';
COMMENT ON COLUMN pgokf_web.users.updated_at IS 'When the role, password, or name last changed.';

-- Last, so any relation a script adds is registered for pg_dump (none here;
-- the rule holds for every upgrade script).
SELECT pgokf_private.register_dump_relations();
