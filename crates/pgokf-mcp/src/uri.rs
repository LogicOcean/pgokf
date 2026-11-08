//! Canonical resource identifiers. Parsing grants no catalog access.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::fmt;

pub const MAX_URI_BYTES: usize = 2048;
const MAX_SEGMENT_BYTES: usize = 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResourceUri {
    Catalog,
    Bundle {
        bundle_id: i64,
    },
    Concept {
        bundle_id: i64,
        concept_id: String,
    },
    Document {
        bundle_id: i64,
        path: String,
    },
    SkillFile {
        bundle_id: i64,
        concept_id: String,
        skill_name: String,
        member_path: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UriError;
impl fmt::Display for UriError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid_uri")
    }
}
impl std::error::Error for UriError {}

impl ResourceUri {
    /// # Errors
    /// Returns `UriError` when fields violate the canonical grammar or size limits.
    #[cfg(test)]
    pub fn bundle(bundle_id: i64) -> Result<Self, UriError> {
        Self::checked(Self::Bundle { bundle_id })
    }
    /// # Errors
    /// Returns `UriError` when fields violate the canonical grammar or size limits.
    #[cfg(test)]
    pub fn concept(bundle_id: i64, concept_id: &str) -> Result<Self, UriError> {
        Self::checked(Self::Concept {
            bundle_id,
            concept_id: concept_id.into(),
        })
    }
    /// # Errors
    /// Returns `UriError` when fields violate the canonical grammar or size limits.
    #[cfg(test)]
    pub fn document(bundle_id: i64, path: &str) -> Result<Self, UriError> {
        Self::checked(Self::Document {
            bundle_id,
            path: path.into(),
        })
    }
    /// # Errors
    /// Returns `UriError` when fields violate the canonical grammar or size limits.
    #[cfg(test)]
    pub fn skill_file(
        bundle_id: i64,
        concept_id: &str,
        skill_name: &str,
        member_path: &str,
    ) -> Result<Self, UriError> {
        Self::checked(Self::SkillFile {
            bundle_id,
            concept_id: concept_id.into(),
            skill_name: skill_name.into(),
            member_path: member_path.into(),
        })
    }
    #[cfg(test)]
    fn checked(value: Self) -> Result<Self, UriError> {
        value.render()?;
        Ok(value)
    }
    /// # Errors
    /// Returns `UriError` when fields violate the canonical grammar or size limits.
    pub fn parse(input: &str) -> Result<Self, UriError> {
        if input.len() > MAX_URI_BYTES || !input.is_ascii() {
            return Err(UriError);
        }
        let result = if let Some(path) = input.strip_prefix("okf://catalog/") {
            let parts: Vec<_> = path.split('/').collect();
            match parts.as_slice() {
                ["manifest"] => Self::Catalog,
                ["bundles", id] => Self::Bundle {
                    bundle_id: parse_id(id)?,
                },
                ["bundles", id, "concepts", key] => Self::Concept {
                    bundle_id: parse_id(id)?,
                    concept_id: decode_key(key)?,
                },
                ["bundles", id, "documents", key] => Self::Document {
                    bundle_id: parse_id(id)?,
                    path: decode_key(key)?,
                },
                _ => return Err(UriError),
            }
        } else if let Some(path) = input.strip_prefix("skill://pgokf/") {
            let parts: Vec<_> = path.split('/').collect();
            match parts.as_slice() {
                ["b", id, "c", key, name, members @ ..] if !members.is_empty() => Self::SkillFile {
                    bundle_id: parse_id(id)?,
                    concept_id: decode_key(key)?,
                    skill_name: decode_segment(name)?,
                    member_path: members
                        .iter()
                        .map(|part| decode_segment(part))
                        .collect::<Result<Vec<_>, _>>()?
                        .join("/"),
                },
                _ => return Err(UriError),
            }
        } else {
            return Err(UriError);
        };
        if result.render()? != input {
            return Err(UriError);
        }
        Ok(result)
    }
    /// # Errors
    /// Returns `UriError` when fields violate the canonical grammar or size limits.
    pub fn render(&self) -> Result<String, UriError> {
        let output = match self {
            Self::Catalog => "okf://catalog/manifest".into(),
            Self::Bundle { bundle_id } => {
                format!("okf://catalog/bundles/{}", checked_id(*bundle_id)?)
            }
            Self::Concept {
                bundle_id,
                concept_id,
            } => format!(
                "okf://catalog/bundles/{}/concepts/{}",
                checked_id(*bundle_id)?,
                concept_key(concept_id)?
            ),
            Self::Document { bundle_id, path } => {
                validate_path(path)?;
                format!(
                    "okf://catalog/bundles/{}/documents/{}",
                    checked_id(*bundle_id)?,
                    encode_key(path)?
                )
            }
            Self::SkillFile {
                bundle_id,
                concept_id,
                skill_name,
                member_path,
            } => {
                validate_path(member_path)?;
                let members = member_path
                    .split('/')
                    .map(encode_segment)
                    .collect::<Result<Vec<_>, _>>()?
                    .join("/");
                format!(
                    "skill://pgokf/b/{}/c/{}/{}/{}",
                    checked_id(*bundle_id)?,
                    concept_key(concept_id)?,
                    encode_segment(skill_name)?,
                    members
                )
            }
        };
        if output.len() > MAX_URI_BYTES {
            return Err(UriError);
        }
        Ok(output)
    }
}

fn checked_id(id: i64) -> Result<i64, UriError> {
    if id > 0 { Ok(id) } else { Err(UriError) }
}
fn parse_id(input: &str) -> Result<i64, UriError> {
    if input.is_empty() || input.starts_with('0') || !input.bytes().all(|b| b.is_ascii_digit()) {
        return Err(UriError);
    }
    checked_id(input.parse().map_err(|_| UriError)?)
}
fn concept_key(value: &str) -> Result<String, UriError> {
    if value.len() > 512 {
        return Err(UriError);
    }
    validate_path(value)?;
    encode_key(value)
}
fn encode_key(value: &str) -> Result<String, UriError> {
    let key = URL_SAFE_NO_PAD.encode(value.as_bytes());
    if key.len() > MAX_SEGMENT_BYTES {
        return Err(UriError);
    }
    Ok(key)
}
fn decode_key(value: &str) -> Result<String, UriError> {
    if value.len() > MAX_SEGMENT_BYTES {
        return Err(UriError);
    }
    String::from_utf8(URL_SAFE_NO_PAD.decode(value).map_err(|_| UriError)?).map_err(|_| UriError)
}
fn validate_path(value: &str) -> Result<(), UriError> {
    for part in value.split('/') {
        validate_opaque_segment(part)?;
    }
    Ok(())
}
// Catalog keys contain opaque UTF-8 bytes, not percent-encoded URI syntax.
// Validate their path structure before base64url encoding, without unescaping
// or interpreting literal percent sequences.
fn validate_opaque_segment(value: &str) -> Result<(), UriError> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value
            .chars()
            .any(|ch| ch.is_control() || ch == '/' || ch == '\\')
    {
        return Err(UriError);
    }
    Ok(())
}
fn validate_segment(value: &str) -> Result<(), UriError> {
    validate_opaque_segment(value)?;
    // A residual escape triplet in a URI path segment would permit a second decoder to
    // reinterpret bytes. Literal percent characters without a triplet are safe.
    if value
        .as_bytes()
        .windows(3)
        .any(|w| w[0] == b'%' && w[1].is_ascii_hexdigit() && w[2].is_ascii_hexdigit())
    {
        return Err(UriError);
    }
    Ok(())
}
fn encode_segment(value: &str) -> Result<String, UriError> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    validate_segment(value)?;
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            output.push(char::from(byte));
        } else {
            output.push('%');
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
    if output.len() > MAX_SEGMENT_BYTES {
        return Err(UriError);
    }
    Ok(output)
}
fn decode_segment(value: &str) -> Result<String, UriError> {
    if value.len() > MAX_SEGMENT_BYTES {
        return Err(UriError);
    }
    let mut bytes = Vec::with_capacity(value.len());
    let mut input = value.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let a = input
                .next()
                .and_then(|b| char::from(b).to_digit(16))
                .ok_or(UriError)?;
            let b = input
                .next()
                .and_then(|b| char::from(b).to_digit(16))
                .ok_or(UriError)?;
            bytes.push(u8::try_from(a * 16 + b).map_err(|_| UriError)?);
        } else {
            bytes.push(byte);
        }
    }
    let decoded = String::from_utf8(bytes).map_err(|_| UriError)?;
    validate_segment(&decoded)?;
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_examples() {
        let cases = [
            (ResourceUri::Catalog, "okf://catalog/manifest"),
            (
                ResourceUri::Bundle { bundle_id: 42 },
                "okf://catalog/bundles/42",
            ),
            (
                ResourceUri::Concept {
                    bundle_id: 42,
                    concept_id: "src/main".into(),
                },
                "okf://catalog/bundles/42/concepts/c3JjL21haW4",
            ),
            (
                ResourceUri::Document {
                    bundle_id: 42,
                    path: "docs/a.md".into(),
                },
                "okf://catalog/bundles/42/documents/ZG9jcy9hLm1k",
            ),
            (
                ResourceUri::SkillFile {
                    bundle_id: 42,
                    concept_id: "a".into(),
                    skill_name: "my skill".into(),
                    member_path: "scripts/a b.sh".into(),
                },
                "skill://pgokf/b/42/c/YQ/my%20skill/scripts/a%20b.sh",
            ),
        ];
        for (value, wire) in cases {
            assert_eq!(value.render().unwrap(), wire);
            assert_eq!(ResourceUri::parse(wire).unwrap(), value);
        }
    }

    #[test]
    fn rejects_hostile_and_noncanonical_inputs() {
        for uri in [
            "",
            "file:///etc/passwd",
            "OKF://catalog/manifest",
            "okf://Catalog/manifest",
            "okf://catalog/manifest/",
            "okf://catalog/manifest?x=1",
            "okf://catalog/manifest#x",
            "okf://catalog/bundles/0",
            "okf://catalog/bundles/01",
            "okf://catalog/bundles/+1",
            "okf://catalog/bundles/-1",
            "okf://catalog/bundles/9223372036854775808",
            "okf://catalog/bundles/1/concepts/",
            "okf://catalog/bundles/1/concepts/YQ==",
            "okf://catalog/bundles/1/concepts/YR",
            "okf://catalog/bundles/1/concepts/_w",
            "okf://catalog/bundles/1/concepts/Li4",
            "okf://catalog/bundles/1/concepts/L2E",
            "okf://catalog/bundles/1/documents/YS8uLi9i",
            "okf://catalog/bundles/1/documents/YVxi",
            "skill://pgokf/b/1/c/YQ/s/../SKILL.md",
            "skill://pgokf/b/1/c/YQ/s/./SKILL.md",
            "skill://pgokf/b/1/c/YQ/s//SKILL.md",
            "skill://pgokf/b/1/c/YQ/s/%2FSKILL.md",
            "skill://pgokf/b/1/c/YQ/s/%5CSKILL.md",
            "skill://pgokf/b/1/c/YQ/s/%00",
            "skill://pgokf/b/1/c/YQ/s/%41",
            "skill://pgokf/b/1/c/YQ/s/%c3%a9",
            "skill://pgokf/b/1/c/YQ/s/é",
            "skill://pgokf/b/1/c/YQ/s/%",
            "skill://pgokf/b/1/c/YQ/s/%FF",
            "skill://pgokf/b/1/c/YQ/s/a\\b",
            "skill://pgokf/b/1/c/YQ/s/%252e%252e/x",
            "skill://pgokf/b/1/c/YQ/s/%252Fetc",
            "skill://pgokf/b/1/c/YQ/s/%255cetc",
        ] {
            assert!(ResourceUri::parse(uri).is_err(), "accepted {uri:?}");
        }
    }

    #[test]
    fn deterministic_roundtrip_corpus() {
        let words = [
            "a", "é", "資料", "a b", "a%b", "100%", "a:b", "a?b", "a#b", "a~b", "🙂",
        ];
        for (i, a) in words.iter().enumerate() {
            for b in words {
                let id = format!("{a}/{b}");
                let bundle_id = if i % 2 == 0 { 1 } else { i64::MAX };
                let variants = [
                    ResourceUri::concept(bundle_id, &id).unwrap(),
                    ResourceUri::document(bundle_id, &format!("{id}.md")).unwrap(),
                    ResourceUri::skill_file(bundle_id, &id, a, &format!("{b}/SKILL.md")).unwrap(),
                ];
                for value in variants {
                    let wire = value.render().unwrap();
                    assert_eq!(ResourceUri::parse(&wire).unwrap(), value);
                }
            }
        }
    }

    #[test]
    fn opaque_percent_keys_roundtrip_without_uri_decoding() {
        for id in [
            "odd%20name",
            "odd%2fname",
            "odd%25name",
            "odd%",
            "odd%%20",
            "資料%2f🙂",
        ] {
            for value in [
                ResourceUri::concept(1, id).unwrap(),
                ResourceUri::document(1, &format!("{id}.md")).unwrap(),
                ResourceUri::skill_file(1, id, "deploy", "SKILL.md").unwrap(),
            ] {
                let wire = value.render().unwrap();
                assert_eq!(ResourceUri::parse(&wire).unwrap(), value);
                assert!(!wire.contains('%'));
            }
        }
    }

    #[test]
    fn limits_and_constructor_validation() {
        let max_id = "x".repeat(512);
        assert!(ResourceUri::concept(i64::MAX, &max_id).is_ok());
        assert!(ResourceUri::concept(1, &"x".repeat(513)).is_err());
        assert!(ResourceUri::bundle(0).is_err());
        assert!(ResourceUri::bundle(-1).is_err());
        assert!(ResourceUri::document(1, "../a").is_err());
        assert!(ResourceUri::skill_file(1, "a", "a/b", "SKILL.md").is_err());
        assert!(ResourceUri::skill_file(1, "a", "a", &"x".repeat(1024)).is_ok());
        assert!(ResourceUri::skill_file(1, "a", "a", &"x".repeat(1025)).is_err());
        let prefix = "skill://pgokf/b/1/c/YQ/s/";
        let member = format!(
            "{}/{}",
            "a".repeat(1000),
            "b".repeat(2048 - prefix.len() - 1001)
        );
        let exact = format!("{prefix}{member}");
        assert_eq!(exact.len(), 2048);
        assert!(ResourceUri::parse(&exact).is_ok());
        assert!(ResourceUri::parse(&(exact + "b")).is_err());
    }

    #[test]
    fn no_panics_on_deterministic_arbitrary_bytes() {
        let mut state = 0x1234_5678_u32;
        for len in 0..4096 {
            let bytes: Vec<u8> = (0..len % 257)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    state.to_le_bytes()[0]
                })
                .collect();
            let input = String::from_utf8_lossy(&bytes);
            if let Ok(parsed) = ResourceUri::parse(&input) {
                assert_eq!(parsed.render().unwrap(), input);
            }
        }
    }
}
