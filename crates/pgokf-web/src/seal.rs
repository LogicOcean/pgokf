// SPDX-License-Identifier: AGPL-3.0-only
//! A secret at rest in the catalog, sealed under the site's own key.
//!
//! The one secret the web UI keeps that is not a one-way hash is an identity
//! provider's client secret (`pgokf_web.oidc.client_secret`): the site must
//! present it to the provider, so it must be able to read it back. It is
//! stored sealed - AES-256-GCM under a key derived (HKDF-SHA256) from the
//! session secret the operator set - so the catalog, and every writer
//! credential with it (the ingestion pipeline's included), holds ciphertext;
//! the table's own constraint refuses anything but the sealed form. Without
//! a configured session secret there is no stable key, and no secret can be
//! stored: the Admin page says so.

use std::fmt;

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use aws_lc_rs::hkdf::{HKDF_SHA256, Salt};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The sealed form's version, so a later scheme can be told apart.
const VERSION: &str = "v1";
/// Fixed HKDF inputs: the salt names this use of the session secret, the
/// info names the column, and both are bound into the ciphertext as
/// associated data - a sealed value moved to another column, or sealed for
/// another purpose, does not open.
const SALT: &[u8] = b"pgokf-web seal";
const INFO: &[u8] = b"pgokf_web.oidc.client_secret";

/// Seals and opens secrets under one derived key.
pub(crate) struct Sealer {
    key: LessSafeKey,
}

impl fmt::Debug for Sealer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Sealer(<redacted>)")
    }
}

impl Sealer {
    /// Derive the sealing key from the session secret.
    ///
    /// # Errors
    ///
    /// The key cannot be derived (a secret too short to be one).
    pub(crate) fn from_secret(secret: &[u8]) -> Result<Self> {
        if secret.len() < 16 {
            bail!("the session secret is too short to seal a secret with");
        }
        let pseudorandom = Salt::new(HKDF_SHA256, SALT).extract(secret);
        let okm = pseudorandom
            .expand(&[INFO], &AES_256_GCM)
            .map_err(|_| anyhow!("deriving the sealing key"))?;
        Ok(Self {
            key: LessSafeKey::new(UnboundKey::from(okm)),
        })
    }

    /// The sealed form of `plain`: `v1:<nonce>:<ciphertext>`, both
    /// base64url without padding, with a fresh nonce each time.
    ///
    /// # Errors
    ///
    /// The system random source or the cipher failing.
    pub(crate) fn seal(&self, plain: &str) -> Result<String> {
        let mut nonce = [0_u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|e| anyhow!("reading random bytes: {e}"))?;
        let mut sealed = plain.as_bytes().to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(INFO),
                &mut sealed,
            )
            .map_err(|_| anyhow!("sealing the secret"))?;
        Ok(format!(
            "{VERSION}:{}:{}",
            URL_SAFE_NO_PAD.encode(nonce),
            URL_SAFE_NO_PAD.encode(&sealed)
        ))
    }

    /// The secret a sealed form holds.
    ///
    /// # Errors
    ///
    /// The form is not one this sealer wrote, or it was sealed under
    /// another session secret - the usual reason being that
    /// `OKF_WEB_SESSION_SECRET` changed since.
    pub(crate) fn open(&self, sealed: &str) -> Result<String> {
        let mut parts = sealed.splitn(3, ':');
        let (version, nonce, sealed) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        if version != VERSION {
            bail!("the sealed secret is not in a form this build knows ({version:?})");
        }
        let nonce: [u8; NONCE_LEN] = URL_SAFE_NO_PAD
            .decode(nonce)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .context("the sealed secret's nonce is malformed")?;
        let mut buffer = URL_SAFE_NO_PAD
            .decode(sealed)
            .context("the sealed secret is malformed")?;
        let plain = self
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(INFO),
                &mut buffer,
            )
            .map_err(|_| {
                anyhow!(
                    "the stored client secret does not open under this session secret: was \
                     OKF_WEB_SESSION_SECRET changed since it was saved? Save the secret again."
                )
            })?;
        String::from_utf8(plain.to_vec()).context("the sealed secret is not text")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault(secret: &str) -> Sealer {
        Sealer::from_secret(secret.as_bytes()).expect("a sealer")
    }

    #[test]
    fn a_secret_round_trips_and_looks_like_nothing_in_between() {
        // Arrange
        let vault = vault("a session secret of at least thirty-two characters");

        // Act
        let sealed = vault.seal("hunter2-client-secret").expect("seals");
        let again = vault.seal("hunter2-client-secret").expect("seals");

        // Assert
        assert_eq!(vault.open(&sealed).expect("opens"), "hunter2-client-secret");
        assert!(sealed.starts_with("v1:"));
        assert!(!sealed.contains("hunter2"));
        assert_ne!(sealed, again, "a fresh nonce every time");
        let nonce = sealed.split(':').nth(1).expect("nonce");
        assert_eq!(nonce.len(), 16, "12 bytes of nonce, base64url");
    }

    #[test]
    fn a_sealed_secret_opens_under_its_own_session_secret_only() {
        // Arrange
        let sealed = vault("the first session secret, long enough to count")
            .seal("s3cret")
            .expect("seals");

        // Act
        let other = vault("another session secret, also long enough").open(&sealed);
        let same = vault("the first session secret, long enough to count").open(&sealed);

        // Assert
        assert!(other.is_err());
        assert!(
            other
                .expect_err("refused")
                .to_string()
                .contains("OKF_WEB_SESSION_SECRET")
        );
        assert_eq!(same.expect("opens"), "s3cret");
    }

    #[test]
    fn a_tampered_or_malformed_form_is_refused() {
        // Arrange
        let vault = vault("a session secret of at least thirty-two characters");
        let sealed = vault.seal("s3cret").expect("seals");
        let mut flipped = sealed.clone();
        let last = flipped.pop().expect("a character");
        flipped.push(if last == 'A' { 'B' } else { 'A' });

        // Act / Assert
        assert!(vault.open(&flipped).is_err(), "a changed ciphertext");
        assert!(vault.open("v2:abc:def").is_err(), "an unknown version");
        assert!(vault.open("hunter2").is_err(), "not sealed at all");
        assert!(vault.open("v1:short:xyz").is_err(), "a malformed nonce");
    }

    #[test]
    fn a_short_session_secret_seals_nothing() {
        // Arrange / Act / Assert
        assert!(Sealer::from_secret(b"short").is_err());
    }
}
