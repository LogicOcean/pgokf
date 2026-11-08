//! Verdicts over observations emitted by the real Homebrew Formula test.
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    control_exists: bool,
    control: Option<String>,
    extension_version: String,
    expected_control_version: String,
    expected_extension_version: String,
}

/// # Errors
/// Missing, ambiguous, malformed or inconsistent installed observations fail.
pub fn validate(log: &str, expected: &str) -> Result<(), String> {
    let records: Vec<_> = log
        .lines()
        .filter_map(|line| line.strip_prefix("PGOKF_TEST_OBSERVATION="))
        .collect();
    if records.len() != 1 {
        return Err("one Formula test observation required".into());
    }
    let observation: Observation = crate::json::from_slice(records[0].as_bytes())?;
    if !observation.control_exists {
        return Err("control file absent".into());
    }
    if observation.expected_control_version != expected
        || observation.expected_extension_version != expected
    {
        return Err("formula expectation disagrees with source version".into());
    }
    let control = observation.control.as_deref().unwrap_or("");
    let versions: Vec<_> = control
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "default_version").then_some(value.trim())
        })
        .collect();
    if versions != [format!("'{expected}'")] {
        return Err("control version disagrees with source version".into());
    }
    let output = observation
        .extension_version
        .trim()
        .strip_prefix("CREATE EXTENSION\n")
        .unwrap_or(observation.extension_version.trim());
    if output.trim() != expected {
        return Err("extension version disagrees with source version".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn observation() -> serde_json::Value {
        serde_json::json!({"control_exists":true,"control":"# extension\ndefault_version = '0.2.0'\n", "extension_version":"CREATE EXTENSION\n0.2.0\n", "expected_control_version":"0.2.0", "expected_extension_version":"0.2.0"})
    }
    fn log(value: &serde_json::Value) -> String {
        format!("Homebrew native command output\nPGOKF_TEST_OBSERVATION={value}\n")
    }
    #[test]
    fn actual_formula_observations_require_every_verdict() {
        let good = observation();
        validate(&log(&good), "0.2.0").unwrap();
        for (key, value, reason) in [
            (
                "control_exists",
                serde_json::json!(false),
                "control file absent",
            ),
            (
                "control",
                serde_json::json!("# default_version = '0.2.0'"),
                "control version",
            ),
            (
                "control",
                serde_json::json!("default_version = '0.2.0'\ndefault_version = '0.1.0'"),
                "control version",
            ),
            (
                "extension_version",
                serde_json::json!("CREATE EXTENSION\n0.1.0\n"),
                "extension version",
            ),
            (
                "expected_control_version",
                serde_json::json!("0.1.0"),
                "formula expectation",
            ),
            (
                "expected_extension_version",
                serde_json::json!("0.1.0"),
                "formula expectation",
            ),
        ] {
            let mut bad = good.clone();
            bad[key] = value;
            assert!(
                validate(&log(&bad), "0.2.0").unwrap_err().contains(reason),
                "{key}"
            );
        }
    }
    #[test]
    fn observations_are_unique_and_duplicate_safe() {
        assert!(validate("", "0.2.0").is_err());
        let good = log(&observation());
        assert!(validate(&format!("{good}{good}"), "0.2.0").is_err());
        assert!(validate(&good.replace('{', "{\"control_exists\":false,"), "0.2.0").is_err());
        assert!(validate(&good.replace('{', "{\"unexpected\":true,"), "0.2.0").is_err());
        let mut bad = observation();
        bad["extension_version"] = serde_json::json!("CREATE EXTENSION\n0.2.0\n0.2.0");
        assert!(validate(&log(&bad), "0.2.0").is_err());
    }
}
