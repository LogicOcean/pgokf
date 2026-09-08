// SPDX-License-Identifier: AGPL-3.0-only
//! The bearer tokens that let an MCP client call `pgokf-mcp` over HTTP:
//! their shape, their stored form, and the roles they carry.
//!
//! Two companions must agree on these. `pgokf-web` mints a token (its Admin
//! page, or `pgokf-web mcp-token mint`) and stores only the SHA-256 digest
//! in the catalog (`pgokf_web.mcp_tokens`); `pgokf-mcp` hashes the token a
//! request presents and asks the catalog whose digest that is. A token is
//! 256 random bits behind a recognizable prefix, so a fast hash is the right
//! way to store it and no slow one makes a request expensive - a trade that
//! is sound only while the token really is unguessable, which is why
//! [`is_minted_token`] admits nothing but the shape [`new_token`] mints.

use std::fmt;
use std::fmt::Write as _;

use anyhow::{Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// The prefix every token carries, so one is recognizable in a
/// configuration file or a leak report.
pub const TOKEN_PREFIX: &str = "pgokf_";

/// How much randomness a token carries. 32 bytes is 256 bits, so guessing
/// one is not a threat and a fast hash is the right way to store it.
const TOKEN_BYTES: usize = 32;

/// How many base64url characters [`TOKEN_BYTES`] become without padding.
const TOKEN_CHARS: usize = TOKEN_BYTES.div_ceil(3) * 4 - 1;

/// The longest a token name may be. It is written into the log beside every
/// call the token makes; the catalog's `mcp_tokens_name_check` says the same.
pub const NAME_MAX: usize = 128;

/// What a token may do.
///
/// **The declaration order is the privilege order**: a role holds everything
/// the roles before it hold, which is what `pgokf-mcp` relies on when it
/// decides a tool. The catalog's `mcp_tokens_role_check` names the same two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// Search the catalog and read concepts and skills.
    Reader,
    /// Everything a reader may do, and build workspace plugins.
    Builder,
}

impl Role {
    /// Every role, least privileged first.
    #[must_use]
    pub const fn all() -> &'static [Role] {
        &[Role::Reader, Role::Builder]
    }

    /// The role as the catalog stores it and the operator names it.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Role::Reader => "reader",
            Role::Builder => "builder",
        }
    }

    /// The role an operator named, if it is one.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        let id = id.trim();
        Self::all().iter().copied().find(|role| role.id() == id)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// A fresh token: a recognizable prefix and 32 random bytes.
///
/// # Errors
///
/// The system random source failing.
pub fn new_token() -> Result<String> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("reading random bytes: {e}"))?;
    Ok(format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

/// The stored form of a token: its SHA-256, as 64 lower-case hex characters
/// (the catalog's `mcp_tokens_digest_check`).
#[must_use]
pub fn digest_of(token: &str) -> String {
    let digest = Sha256::digest(token.trim().as_bytes());
    digest
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Whether a presented string has the shape [`new_token`] mints. Anything
/// else cannot be a token a catalog issued, so a server refuses it before
/// it is hashed - a hand-chosen token is never silently protected by a fast
/// hash.
#[must_use]
pub fn is_minted_token(presented: &str) -> bool {
    let Some(random) = presented.strip_prefix(TOKEN_PREFIX) else {
        return false;
    };
    random.len() == TOKEN_CHARS
        && random
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Check a token name: one bounded plain word, since it appears in the log
/// beside every call the token makes.
///
/// # Errors
///
/// The name is empty, too long, or holds a character that is not a letter,
/// a digit, or one of `. _ - @ +`.
pub fn valid_token_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("a token name cannot be empty");
    }
    if name.chars().count() > NAME_MAX {
        bail!("a token name may be at most {NAME_MAX} characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '+'))
    {
        bail!("{name:?} is not a valid token name (letters, digits, . _ - @ +)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_role_ladder_is_declared_least_privileged_first() {
        // Arrange / Act / Assert: `pgokf-mcp` compares roles with `>=`, so
        // a role declared out of order would silently inherit the wrong
        // authority.
        assert!(Role::all().is_sorted(), "Role::all() must be a ladder");
        assert_eq!(Role::all().first(), Some(&Role::Reader));
        for role in Role::all() {
            assert_eq!(Role::parse(role.id()), Some(*role));
            assert_eq!(role.to_string(), role.id());
        }
        assert_eq!(Role::parse(" builder "), Some(Role::Builder), "trimmed");
        assert_eq!(Role::parse("admin"), None);
    }

    #[test]
    fn a_token_is_recognizable_random_and_stored_only_as_a_digest() {
        // Arrange / Act
        let token = new_token().expect("random");
        let again = new_token().expect("random");
        let digest = digest_of(&token);

        // Assert
        assert!(token.starts_with(TOKEN_PREFIX));
        assert_eq!(TOKEN_CHARS, 43, "32 bytes, base64url, unpadded");
        assert_eq!(token.len(), TOKEN_PREFIX.len() + TOKEN_CHARS);
        assert_ne!(token, again);
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(digest, digest_of(&format!("  {token}  ")), "trimmed");
        assert_ne!(digest, digest_of(&again));
    }

    #[test]
    fn only_a_minted_token_has_the_shape() {
        // Arrange
        let token = new_token().expect("random");

        // Act / Assert
        assert!(is_minted_token(&token));
        assert!(!is_minted_token("hunter2"), "a chosen password is not one");
        assert!(!is_minted_token(&token[..token.len() - 1]), "too short");
        assert!(!is_minted_token(&format!("{token}x")), "too long");
        assert!(!is_minted_token(&token[TOKEN_PREFIX.len()..]), "no prefix");
        assert!(
            !is_minted_token(&format!("{TOKEN_PREFIX}{}", "!".repeat(TOKEN_CHARS))),
            "base64url characters only"
        );
    }

    #[test]
    fn a_token_name_is_one_bounded_plain_word() {
        // Arrange / Act / Assert
        assert!(valid_token_name("fleet-1@example.com").is_ok());
        assert!(valid_token_name("").is_err());
        assert!(valid_token_name("bad name").is_err());
        assert!(valid_token_name("bad:name").is_err());
        assert!(valid_token_name(&"n".repeat(NAME_MAX)).is_ok());
        assert!(valid_token_name(&"n".repeat(NAME_MAX + 1)).is_err());
    }
}
