//! Reviewed `PostgreSQL` package and Docker Official Image identities.
//!
//! Every major the source is built and tested against is a GA release: it is
//! provisioned from its own `postgres:<major>` Docker Official Image tag and
//! from PGDG `<major>.x` packages. A prerelease tag or package (`19rc2`,
//! `19~rc2-1`) is refused, so a major enters the matrix only once it ships.
use crate::process::CommandSpec;

/// The `PostgreSQL` majors the source is built and tested against.
pub const MAJORS: std::ops::RangeInclusive<u32> = 15..=19;

/// The Docker Official Image tag a major is built from: the major itself.
#[must_use]
pub fn image_tag(major: u32) -> String {
    major.to_string()
}

/// # Errors
/// Unreviewed major/tag pairs fail before provisioning.
pub fn target(major: u32, image_tag: &str) -> Result<(), String> {
    if !(MAJORS.contains(&major) && image_tag == self::image_tag(major)) {
        return Err(format!(
            "unreviewed PostgreSQL major/base transition: {major}:{image_tag}"
        ));
    }
    Ok(())
}
/// The Docker Official Image reference a major is built from.
/// # Errors
/// Unreviewed major/tag pairs fail before any build.
pub fn base_image(major: u32, image_tag: &str) -> Result<String, String> {
    target(major, image_tag)?;
    Ok(format!("postgres:{image_tag}"))
}
/// # Errors
/// Missing, drifted, prerelease or wrong-major package versions fail.
pub fn package(major: u32, image_tag: &str, version: &str) -> Result<(), String> {
    target(major, image_tag)?;
    let prefix = format!("{major}.");
    if !version.starts_with(&prefix) || version.len() == prefix.len() {
        return Err(format!(
            "PostgreSQL package must start with {prefix}: {version}"
        ));
    }
    Ok(())
}
/// # Errors
/// Missing packages or a mismatch in either server or development files fails.
pub fn installed(major: u32, image_tag: &str) -> Result<(), String> {
    target(major, image_tag)?;
    let read = |name: String| -> Result<String, String> {
        let mut command = CommandSpec::new("dpkg-query");
        command.args = ["-W", "-f=${Version}", &name]
            .into_iter()
            .map(Into::into)
            .collect();
        let result = command.checked()?;
        String::from_utf8(result.stdout)
            .map(|s| s.trim().to_owned())
            .map_err(|e| e.to_string())
    };
    versions(
        major,
        image_tag,
        &read(format!("postgresql-{major}"))?,
        &read(format!("postgresql-server-dev-{major}"))?,
    )
}
/// # Errors
/// Both runtime and development package identities must independently match.
pub fn versions(major: u32, image_tag: &str, server: &str, headers: &str) -> Result<(), String> {
    package(major, image_tag, server).map_err(|e| format!("server: {e}"))?;
    package(major, image_tag, headers).map_err(|e| format!("headers: {e}"))?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_major_builds_from_its_own_tag() {
        assert_eq!(base_image(18, "18").unwrap(), "postgres:18");
        assert_eq!(base_image(19, "19").unwrap(), "postgres:19");
        assert!(base_image(19, "19rc2").is_err());
    }
    #[test]
    fn runtime_and_headers_are_independent() {
        versions(19, "19", "19.0-1", "19.0-1").unwrap();
        assert!(versions(19, "19", "19~rc2-1", "19.0-1").is_err());
        assert!(versions(19, "19", "19.0-1", "19~rc2-1").is_err());
    }
    #[test]
    fn reviewed_targets_and_versions_are_exact() {
        for major in MAJORS {
            let tag = major.to_string();
            package(major, &tag, &format!("{major}.4-1.pgdg")).unwrap();
            for version in [
                String::new(),
                "(none)".into(),
                format!("{major}~rc2-1"),
                format!("{major}~beta3-1"),
                format!("{major}."),
                format!("{}.4-1", major + 1),
            ] {
                assert!(
                    package(major, &tag, &version).is_err(),
                    "{major}: {version}"
                );
            }
        }
        for (major, tag) in [
            (14, "14"),
            (20, "20"),
            (19, "19beta3"),
            (19, "19rc1"),
            (19, "19rc2"),
            (19, "latest"),
            (18, "19"),
        ] {
            assert!(target(major, tag).is_err(), "{major}:{tag}");
        }
    }
}
