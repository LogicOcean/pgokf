-- SPDX-License-Identifier: AGPL-3.0-only
-- Representative web identity state, by named columns, with fixed times: an
-- identity provider, a password person with a display name, one without, a
-- person that provider signed in, an MCP token, and live users-mode and
-- provider sessions beside an expired one. The pgokf-verifier parity gate seeds it
-- identically into a fresh catalog and into each drifted one before the drift.
INSERT INTO pgokf_web.identity_providers
    (id, enabled, kind, issuer, client_id, redirect_url, provider_name, updated_by, created_at, updated_at)
VALUES ('gh', true, 'github', 'https://github.com', 'upgrade-client', 'https://ui.example/auth/callback',
        'GitHub', 'upgrade-local', '2026-01-02 03:04:05+00', '2026-01-02 03:04:05+00');
INSERT INTO pgokf_web.users (name, role, password_hash, display_name, provider, created_at, updated_at)
VALUES ('upgrade-local', 'admin', '$argon2id$v=19$m=19456,t=2,p=1$bG9jYWw$bG9jYWxoYXNo', 'Local Admin', NULL,
        '2026-01-02 03:04:05+00', '2026-01-03 03:04:05+00'),
       ('upgrade-plain', 'viewer', '$argon2id$v=19$m=19456,t=2,p=1$cGxhaW4$cGxhaW5oYXNo', NULL, NULL,
        '2026-01-02 03:04:05+00', '2026-01-02 03:04:05+00'),
       ('upgrade-sso', 'editor', NULL, 'Octo Cat', 'gh',
        '2026-01-04 03:04:05+00', '2026-01-04 03:04:05+00');
INSERT INTO pgokf_web.sessions (nonce, subject, mode, provider, expires_at, created_at)
VALUES ('nonce-local', 'upgrade-local', 'users', NULL, '2100-01-01 00:00:00+00', '2026-01-05 03:04:05+00'),
       ('nonce-sso', 'upgrade-sso', 'oidc', 'gh', '2100-01-01 00:00:00+00', '2026-01-05 03:04:05+00'),
       ('nonce-expired', 'upgrade-local', 'users', NULL, '2000-01-01 00:00:00+00', '2026-01-05 03:04:05+00');
INSERT INTO pgokf_web.mcp_tokens (name, role, tenant, digest, created_by, created_at)
VALUES ('upgrade-token', 'reader', NULL, repeat('c', 64), 'upgrade-local', '2026-01-02 03:04:05+00');
