//! Authenticated, scope-bound keyset cursors. Tokens are never authorization.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use std::fmt;

pub const CURSOR_TTL_SECONDS: u64 = 900;
pub const MAX_CURSOR_BYTES: usize = 1024;

/// All fields must come from authenticated request context and normalized
/// server-side filters, never from the unverified token. `principal` is the
/// token digest (`local` for stdio); `role` is its currently authorized role.
#[derive(Clone, Copy, Debug)]
pub struct CursorBinding<'a> {
    pub method: &'a str,
    pub tenant: &'a str,
    pub principal: &'a str,
    pub role: &'a str,
    pub filter: &'a str,
    pub generation: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorError {
    Invalid,
    Stale,
}
impl CursorError {
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Invalid => "invalid_cursor",
            Self::Stale => "stale_cursor",
        }
    }
}
impl fmt::Display for CursorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for CursorError {}

/// Deliberately does not implement Debug: key bytes must never enter logs.
pub struct CursorCodec {
    key: [u8; 32],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    v: u8,
    m: String,
    t: String,
    p: String,
    f: String,
    k: Value,
    g: String,
    exp: u64,
}

impl CursorCodec {
    #[must_use]
    pub fn new(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// Issue a token for a bounded scalar keyset.
    ///
    /// # Errors
    /// Returns `Invalid` for malformed positions, overflowing expiry, or oversized tokens.
    pub fn issue(
        &self,
        binding: &CursorBinding<'_>,
        position: Value,
        now: u64,
    ) -> Result<String, CursorError> {
        validate_position(&position)?;
        let payload = Payload {
            v: 1,
            m: binding.method.into(),
            t: binding.tenant.into(),
            p: principal_scope(binding),
            f: binding.filter.into(),
            k: position,
            g: binding.generation.into(),
            exp: now
                .checked_add(CURSOR_TTL_SECONDS)
                .ok_or(CursorError::Invalid)?,
        };
        self.sign(&canonical_payload(&payload)?)
    }

    /// Authenticate a token against the current request and catalog coordinates.
    ///
    /// # Errors
    /// Returns `Invalid` for malformed, expired, tampered, or out-of-scope tokens;
    /// `Stale` only for authenticated tokens from an earlier catalog generation.
    pub fn validate(
        &self,
        token: &str,
        binding: &CursorBinding<'_>,
        now: u64,
    ) -> Result<Value, CursorError> {
        if token.len() > MAX_CURSOR_BYTES {
            return Err(CursorError::Invalid);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|_| CursorError::Invalid)?;
        if bytes.len() <= 32 || URL_SAFE_NO_PAD.encode(&bytes) != token {
            return Err(CursorError::Invalid);
        }
        let (json, tag) = bytes.split_at(bytes.len() - 32);
        // Authenticate before parsing and before distinguishing stale cursors.
        let mut mac = self.mac();
        mac.update(json);
        mac.verify_slice(tag).map_err(|_| CursorError::Invalid)?;
        let payload: Payload = serde_json::from_slice(json).map_err(|_| CursorError::Invalid)?;
        if payload.v != 1
            || payload.m != binding.method
            || payload.t != binding.tenant
            || payload.p != principal_scope(binding)
            || payload.f != binding.filter
            || payload.exp <= now
            || payload.exp.saturating_sub(now) > CURSOR_TTL_SECONDS
            || canonical_payload(&payload)? != json
        {
            return Err(CursorError::Invalid);
        }
        validate_position(&payload.k)?;
        if payload.g != binding.generation {
            return Err(CursorError::Stale);
        }
        Ok(payload.k)
    }

    fn mac(&self) -> Hmac<Sha256> {
        // HMAC accepts keys of every length; this constructor has no error for
        // the fixed 32-byte key enforced by this type.
        Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepts a 32-byte key")
    }
    fn sign(&self, json: &[u8]) -> Result<String, CursorError> {
        if json.len() > MAX_CURSOR_BYTES {
            return Err(CursorError::Invalid);
        }
        let mut mac = self.mac();
        mac.update(json);
        let mut bytes = json.to_vec();
        bytes.extend_from_slice(&mac.finalize().into_bytes());
        let token = URL_SAFE_NO_PAD.encode(bytes);
        if token.len() > MAX_CURSOR_BYTES {
            return Err(CursorError::Invalid);
        }
        Ok(token)
    }
}

fn principal_scope(binding: &CursorBinding<'_>) -> String {
    format!("{}/{}", binding.role, binding.principal)
}
fn canonical_payload(payload: &Payload) -> Result<Vec<u8>, CursorError> {
    // Value's default map representation sorts keys. Keyset positions contain
    // only scalars, so this also fixes the complete nested serialization.
    let value = serde_json::to_value(payload).map_err(|_| CursorError::Invalid)?;
    serde_json::to_vec(&value).map_err(|_| CursorError::Invalid)
}
fn validate_position(value: &Value) -> Result<(), CursorError> {
    let parts = value.as_array().ok_or(CursorError::Invalid)?;
    if parts.is_empty()
        || parts.len() > 8
        || parts
            .iter()
            .any(|part| !part.is_string() && !part.is_number())
    {
        return Err(CursorError::Invalid);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn binding() -> CursorBinding<'static> {
        CursorBinding {
            method: "skills/list",
            tenant: "acme",
            principal: "token-digest",
            role: "reader",
            filter: "filter-sha256",
            generation: "generation-sha256",
        }
    }
    #[test]
    fn issue_validate_and_expiry() {
        let codec = CursorCodec::new([42; 32]);
        let binding = binding();
        let token = codec.issue(&binding, json!(["a", 42]), 100).unwrap();
        assert_eq!(
            codec.validate(&token, &binding, 100).unwrap(),
            json!(["a", 42])
        );
        assert!(codec.validate(&token, &binding, 999).is_ok());
        assert_eq!(
            codec.validate(&token, &binding, 1000),
            Err(CursorError::Invalid)
        );
        assert_eq!(
            codec.validate(&token, &binding, 0),
            Err(CursorError::Invalid)
        );
        assert!(!token.contains('='));
    }
    #[test]
    fn every_scope_is_bound_and_generation_is_stale_only_after_authentication() {
        let codec = CursorCodec::new([1; 32]);
        let original = binding();
        let token = codec.issue(&original, json!([42]), 1).unwrap();
        for index in 0..6 {
            let mut changed = original;
            match index {
                0 => changed.method = "resources/list",
                1 => changed.tenant = "other",
                2 => changed.principal = "other-token",
                3 => changed.role = "admin",
                4 => changed.filter = "different-filter",
                _ => changed.generation = "next-generation",
            }
            assert_eq!(
                codec.validate(&token, &changed, 2),
                Err(if index == 5 {
                    CursorError::Stale
                } else {
                    CursorError::Invalid
                })
            );
        }
        let mut other = original;
        other.generation = "next";
        other.tenant = "other";
        assert_eq!(codec.validate(&token, &other, 2), Err(CursorError::Invalid));
        assert_eq!(
            CursorCodec::new([2; 32]).validate(&token, &original, 2),
            Err(CursorError::Invalid)
        );
    }
    #[test]
    fn every_token_byte_is_authenticated() {
        let codec = CursorCodec::new([3; 32]);
        let binding = binding();
        let token = codec.issue(&binding, json!([1]), 0).unwrap();
        for i in 0..token.len() {
            let mut mutated = token.as_bytes().to_vec();
            mutated[i] = if mutated[i] == b'A' { b'B' } else { b'A' };
            assert_eq!(
                codec.validate(std::str::from_utf8(&mutated).unwrap(), &binding, 1),
                Err(CursorError::Invalid)
            );
        }
        for malformed in ["", "a", "!!!!", "AA==", &"A".repeat(1025)] {
            assert_eq!(
                codec.validate(malformed, &binding, 1),
                Err(CursorError::Invalid)
            );
        }
    }
    #[test]
    fn signed_malformed_payloads_still_fail_closed() {
        let codec = CursorCodec::new([4; 32]);
        let binding = binding();
        let token = codec.issue(&binding, json!([1]), 0).unwrap();
        let bytes = URL_SAFE_NO_PAD.decode(token).unwrap();
        let original: serde_json::Value =
            serde_json::from_slice(&bytes[..bytes.len() - 32]).unwrap();
        for field in ["v", "m", "t", "p", "f", "k", "g", "exp"] {
            let mut value = original.clone();
            value.as_object_mut().unwrap().remove(field);
            let altered = codec.sign(&serde_json::to_vec(&value).unwrap()).unwrap();
            assert_eq!(
                codec.validate(&altered, &binding, 1),
                Err(CursorError::Invalid)
            );
        }
        let mut future = original.clone();
        future["v"] = json!(2);
        assert_eq!(
            codec.validate(
                &codec.sign(&serde_json::to_vec(&future).unwrap()).unwrap(),
                &binding,
                1
            ),
            Err(CursorError::Invalid)
        );
        let mut extra = original;
        extra["extra"] = json!(true);
        assert_eq!(
            codec.validate(
                &codec.sign(&serde_json::to_vec(&extra).unwrap()).unwrap(),
                &binding,
                1
            ),
            Err(CursorError::Invalid)
        );
    }
    #[test]
    fn bounded_payload_and_bad_positions() {
        let codec = CursorCodec::new([5; 32]);
        let binding = binding();
        for position in [
            json!(null),
            json!({"offset":1}),
            json!([]),
            json!([true]),
            json!([{}]),
            json!(["x".repeat(1024)]),
        ] {
            assert_eq!(
                codec.issue(&binding, position, 0),
                Err(CursorError::Invalid)
            );
        }
        assert_eq!(
            codec.issue(&binding, json!([1]), u64::MAX),
            Err(CursorError::Invalid)
        );
    }
}
