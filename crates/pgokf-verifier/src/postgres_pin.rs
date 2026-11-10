//! Reviewed `PostgreSQL` package and Docker Official Image identities.
use crate::owned_docker::DockerApi;
use crate::process::CommandSpec;

pub const BETA_DIGEST: &str =
    "sha256:4b3e4b2edcf554492dd1c5e008237208c1d6e6faf2f15c24bf5fc5c4bcf0a21d";
/// # Errors
/// Unreviewed major/tag pairs fail before provisioning.
pub fn target(major: u32, image_tag: &str) -> Result<(), String> {
    if !((15..=18).contains(&major) && image_tag == major.to_string()
        || major == 19 && image_tag == "19rc1")
    {
        return Err(format!(
            "unreviewed PostgreSQL major/base transition: {major}:{image_tag}"
        ));
    }
    Ok(())
}
/// Resolve the reviewed beta index into the build reference itself.
/// # Errors
/// Unreviewed major/tag pairs fail before any build.
pub fn base_image(major: u32, image_tag: &str) -> Result<String, String> {
    target(major, image_tag)?;
    if major == 19 {
        Ok(format!("postgres:{image_tag}@{BETA_DIGEST}"))
    } else {
        Ok(format!("postgres:{image_tag}"))
    }
}
/// # Errors
/// Missing, drifted, prerelease or wrong-major package versions fail.
pub fn package(major: u32, image_tag: &str, version: &str) -> Result<(), String> {
    target(major, image_tag)?;
    let prefix = if major == 19 {
        "19~rc1-".into()
    } else {
        format!("{major}.")
    };
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
/// # Errors
/// Empty, changed or nonliteral image-index digests fail.
pub fn digest(actual: &str) -> Result<(), String> {
    if actual.trim() != BETA_DIGEST {
        return Err("postgres:19rc1 immutable index digest changed or disappeared".into());
    }
    Ok(())
}
/// # Errors
/// Docker lookup errors and changed immutable index identity fail closed.
pub fn beta_image(docker: &mut impl DockerApi) -> Result<(), String> {
    let result = docker.call(
        &[
            "buildx",
            "imagetools",
            "inspect",
            "postgres:19rc1",
            "--format",
            "{{.Manifest.Digest}}",
        ]
        .into_iter()
        .map(Into::into)
        .collect::<Vec<_>>(),
    )?;
    digest(&result)
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Missing;
    impl DockerApi for Missing {
        fn call(&mut self, _: &[String]) -> Result<String, String> {
            Err("missing image".into())
        }
    }
    #[test]
    fn beta_build_reference_is_immutable() {
        assert_eq!(base_image(18, "18").unwrap(), "postgres:18");
        let reference = base_image(19, "19rc1").unwrap();
        assert_eq!(
            reference.split_once('@'),
            Some(("postgres:19rc1", BETA_DIGEST))
        );
        assert!(base_image(19, "19").is_err());
    }
    #[test]
    fn runtime_and_headers_are_independent() {
        versions(19, "19rc1", "19~rc1-1", "19~rc1-1").unwrap();
        assert!(versions(19, "19rc1", "19~rc2-1", "19~rc1-1").is_err());
        assert!(versions(19, "19rc1", "19~rc1-1", "19~rc2-1").is_err());
    }
    #[test]
    fn reviewed_targets_and_versions_are_exact() {
        for major in 15..=18 {
            package(major, &major.to_string(), &format!("{major}.4-1.pgdg")).unwrap();
            for version in ["", "(none)", "19~rc1-1", "18~rc1-1"] {
                assert!(package(major, &major.to_string(), version).is_err());
            }
        }
        package(19, "19rc1", "19~rc1-1.pgdg13+1").unwrap();
        for (major, tag) in [
            (14, "14"),
            (20, "20"),
            (19, "19"),
            (19, "19beta3"),
            (19, "19rc2"),
            (18, "19rc1"),
        ] {
            assert!(target(major, tag).is_err());
        }
        for version in [
            "19~beta2-1",
            "19~beta3-1",
            "19~rc2-1",
            "19.0-1",
            "19~rc1-",
            "",
        ] {
            assert!(package(19, "19rc1", version).is_err());
        }
    }
    #[test]
    fn image_index_must_be_exact_and_lookup_must_succeed() {
        digest(BETA_DIGEST).unwrap();
        for value in [
            "",
            "sha256:wrong",
            "sha256:f0056b553c58e4533ba81921d9bd5b49641d4fa176e1add956a678b034007ac4",
            "prefix sha256:f0056b553c58e4533ba81921d9bd5b49641d4fa176e1add956a678b034007ac4",
        ] {
            assert!(digest(value).is_err());
        }
        assert!(beta_image(&mut Missing).is_err());
    }
}
