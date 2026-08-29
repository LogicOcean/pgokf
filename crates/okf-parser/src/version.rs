// SPDX-License-Identifier: AGPL-3.0-only
//! OKF format-version conformance.
//!
//! The accepted set of Open Knowledge Format versions is deliberately small and
//! centralized here so every conformance decision - in the parser, the sync
//! engine's `okf_version_policy` gate, and any future tooling - consults one
//! authority and can never drift. The catalog models OKF v0.2, so `0.2` (and
//! its patch refinements `0.2.x`) is the only supported major.minor line.

/// The `major.minor` OKF version lines this build models.
///
/// Kept as a single centralized constant so the accepted set never drifts
/// between the parser and its consumers.
pub const SUPPORTED_OKF_VERSIONS: &[&str] = &["0.2"];

/// Report whether `version` names an OKF format version this build supports.
///
/// Recognition is on the `major.minor` line: `0.2` and any patch refinement
/// `0.2.x` are accepted, while a different minor (`0.3`) or major (`1.0`) is
/// not. Matching is lenient on a single leading `v`/`V` (`v0.2` is accepted),
/// mirroring the common `vX.Y` spelling, and surrounding whitespace is ignored.
/// Any value that does not carry at least a `major.minor` pair is unsupported.
#[must_use]
pub fn is_supported_okf_version(version: &str) -> bool {
    let trimmed = version.trim().trim_start_matches(['v', 'V']);
    let mut parts = trimmed.split('.');
    let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
        return false;
    };
    if major.is_empty() || minor.is_empty() {
        return false;
    }
    SUPPORTED_OKF_VERSIONS.iter().any(|supported| {
        let mut supported_parts = supported.split('.');
        supported_parts.next() == Some(major) && supported_parts.next() == Some(minor)
    })
}

#[cfg(test)]
mod tests {
    use super::is_supported_okf_version;

    #[test]
    fn accepts_the_exact_supported_version() {
        // Arrange / Act / Assert
        assert!(is_supported_okf_version("0.2"));
    }

    #[test]
    fn accepts_patch_refinements_of_the_supported_line() {
        // Arrange / Act / Assert: 0.2.x refines the supported major.minor line.
        assert!(is_supported_okf_version("0.2.0"));
        assert!(is_supported_okf_version("0.2.7"));
    }

    #[test]
    fn is_lenient_on_a_leading_v_and_whitespace() {
        // Arrange / Act / Assert
        assert!(is_supported_okf_version("v0.2"));
        assert!(is_supported_okf_version("  0.2  "));
        assert!(is_supported_okf_version("V0.2.1"));
    }

    #[test]
    fn rejects_a_different_minor_or_major() {
        // Arrange / Act / Assert
        assert!(!is_supported_okf_version("0.3"));
        assert!(!is_supported_okf_version("1.0"));
        assert!(!is_supported_okf_version("0.20"));
    }

    #[test]
    fn rejects_values_without_a_major_minor_pair() {
        // Arrange / Act / Assert
        assert!(!is_supported_okf_version("0"));
        assert!(!is_supported_okf_version(""));
        assert!(!is_supported_okf_version("draft"));
        assert!(!is_supported_okf_version("0."));
    }
}
