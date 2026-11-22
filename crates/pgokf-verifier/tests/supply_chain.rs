//! The two supply-chain gates each read the advisories they accept from a
//! file of their own: `cargo deny check` from `deny.toml` and
//! `cargo audit` from `.cargo/audit.toml`. Neither tool reads the other's
//! file, so these tests keep the two lists the same, and keep every accepted
//! advisory explained.

use std::{collections::BTreeSet, fs, path::PathBuf};

fn repository_file(path: &str) -> toml::Table {
    let full = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path);
    let text = fs::read_to_string(&full).unwrap_or_else(|error| panic!("{path}: {error}"));
    text.parse()
        .unwrap_or_else(|error| panic!("{path} is not TOML: {error}"))
}

/// The `[advisories] ignore` array of a repository file, empty when absent.
fn ignore_entries(path: &str) -> Vec<toml::Value> {
    repository_file(path)
        .get("advisories")
        .and_then(|advisories| advisories.get("ignore"))
        .map(|ignore| {
            ignore
                .as_array()
                .unwrap_or_else(|| panic!("{path}: [advisories] ignore is not an array"))
                .clone()
        })
        .unwrap_or_default()
}

/// The advisory ids `deny.toml` accepts, each written `{ id, reason }`.
fn deny_ignores() -> BTreeSet<String> {
    ignore_entries("deny.toml")
        .iter()
        .map(|entry| {
            entry
                .get("id")
                .and_then(toml::Value::as_str)
                .unwrap_or_else(|| panic!("deny.toml: an ignore entry has no id: {entry}"))
                .to_owned()
        })
        .collect()
}

/// The advisory ids `.cargo/audit.toml` accepts, each a bare string.
fn audit_ignores() -> BTreeSet<String> {
    ignore_entries(".cargo/audit.toml")
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .unwrap_or_else(|| {
                    panic!(".cargo/audit.toml: an ignore entry is not an id: {entry}")
                })
                .to_owned()
        })
        .collect()
}

#[test]
fn cargo_deny_and_cargo_audit_accept_the_same_advisories() {
    // Arrange / Act
    let deny = deny_ignores();
    let audit = audit_ignores();

    // Assert
    assert_eq!(
        deny, audit,
        "deny.toml and .cargo/audit.toml must ignore the same advisories"
    );
}

#[test]
fn every_advisory_deny_accepts_is_a_rustsec_id_with_a_reason() {
    // Arrange
    let entries = ignore_entries("deny.toml");

    // Act / Assert
    for entry in &entries {
        let id = entry.get("id").and_then(toml::Value::as_str).unwrap_or("");
        let numbered = id
            .strip_prefix("RUSTSEC-")
            .and_then(|rest| rest.split_once('-'))
            .is_some_and(|(year, number)| {
                year.len() == 4
                    && number.len() == 4
                    && year
                        .bytes()
                        .chain(number.bytes())
                        .all(|b| b.is_ascii_digit())
            });
        assert!(numbered, "deny.toml: {id:?} is not a RUSTSEC-YYYY-NNNN id");
        let reason = entry
            .get("reason")
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        assert!(
            !reason.trim().is_empty(),
            "deny.toml: {id} is accepted without a reason"
        );
    }
}
