//! Reviewed immutable inputs for the historical upgrade starting catalog.
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

pub const IMAGE: &str = "ghcr.io/logicocean/pgokf@sha256:fe62d727a7986bc408979f31e61d3218c98330c8f880ee510097baba22f5490d";
const ARM64_IMAGE: &str = "ghcr.io/logicocean/pgokf@sha256:77dd4ca88a1638e373b7bc433d55d7ecba7f6cc0dccb654ef58a6ec90bc9a6dd";
const AMD64_IMAGE: &str = "ghcr.io/logicocean/pgokf@sha256:d84ff73a8641d99915fe3c228232139fc807433eeea32b689734fb50e36476e6";
const AMD64_SQL: &str = "ab11dc9a6c01a5315d4d746b1af11f8e9a73f16cd65f4312f9355e08c947975d";
const PUBLISHED_SQL: &str = "47431a21f32613813605ea25dbc384efdf9669ffcad53375ea27c4d9b1d7923b";
const SOURCE_SQL: &str = "9bffe2161480522d145c541ebcb5cb67cd3771bd0e8777ff8c2f9b76050ff774";
const SOURCE_COMMIT: &str = "59d29d3dc4b78cd5c5000bf473b0f097c9fb2ea4";

#[derive(Debug, Serialize)]
pub struct Origin {
    pub version: &'static str,
    pub sql_sha256: String,
    pub provenance: &'static str,
}
/// # Errors
/// Unknown bytes are rejected before the historical catalog can be installed.
pub fn validate(bytes: &[u8]) -> Result<Origin, String> {
    let digest = Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        });
    if digest != PUBLISHED_SQL && digest != AMD64_SQL && digest != SOURCE_SQL {
        return Err("historical install SQL does not match a reviewed immutable input".into());
    }
    let provenance = if digest == PUBLISHED_SQL {
        ARM64_IMAGE
    } else if digest == AMD64_SQL {
        AMD64_IMAGE
    } else {
        SOURCE_COMMIT
    };
    Ok(Origin {
        version: "0.2.0",
        sql_sha256: digest,
        provenance,
    })
}

/// # Errors
/// Only the two published Linux platforms have reviewed helper images.
pub fn helper_image() -> Result<(&'static str, &'static str), String> {
    match std::env::consts::ARCH {
        "aarch64" => Ok((ARM64_IMAGE, "linux/arm64")),
        "x86_64" => Ok((AMD64_IMAGE, "linux/amd64")),
        _ => Err("no reviewed historical helper for this architecture".into()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn substituted_historical_sql_is_rejected() {
        crate::semantic_assert!(
            "historical-substitution",
            super::validate(b"-- replaced historical SQL\nCREATE SCHEMA pgokf;").is_err()
        );
    }
}
