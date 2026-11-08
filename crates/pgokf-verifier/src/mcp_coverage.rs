//! Native MCP coverage closure. Receipt counts prove verifier execution only;
//! they must never be reported as product mutation kills or official scenarios.
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

type Check = Result<(), String>;
#[derive(Clone, Deserialize)]
pub struct Inventory {
    pub tools: Vec<String>,
    pub resources: Vec<String>,
    pub templates: Vec<String>,
    pub skills: Vec<String>,
    pub members: Vec<String>,
    pub classes: Vec<String>,
}
#[derive(Clone, Deserialize)]
pub struct Requirement {
    pub scenario: String,
    pub id: String,
    pub assertions: Vec<String>,
    pub minimum_receipts: BTreeMap<String, u64>,
}
#[derive(Clone, Deserialize)]
pub struct Ledger {
    pub requirements: Vec<Requirement>,
    pub required_tests: Vec<String>,
    pub control_labels: Vec<String>,
    pub resource_classes: Vec<String>,
}
#[derive(Clone, Deserialize)]
pub struct PolicyCheck {
    pub id: String,
    pub applicability: String,
}
#[derive(Clone, Deserialize)]
pub struct Scenario {
    pub scenario: String,
    pub checks: Vec<PolicyCheck>,
}
#[derive(Clone, Deserialize)]
pub struct SkillCheck {
    pub scenario: String,
    pub id: String,
    pub applicability: String,
}
pub const NATIVE_COMMAND: &str =
    "cargo run --locked -p pgokf-verifier -- mcp-native ROOT PG_CONFIG MCP_BINARY NEW_EVIDENCE_DIR";
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateB {
    pub command: String,
    pub ledger: String,
    pub required: bool,
    pub authority: String,
    pub no_skips: bool,
    pub no_zero_assertion_selectors: bool,
}
#[derive(Clone, Deserialize)]
pub struct Policy {
    pub gate_b: GateB,
    pub methods: Vec<String>,
    pub legacy_methods: Vec<String>,
    pub scenarios: Vec<Scenario>,
    pub skills_checks: Vec<SkillCheck>,
}
/// Private mutation prevents call sites from granting assertion credit directly.
#[derive(Clone, Default)]
pub struct Receipts(BTreeMap<String, u64>);
impl Receipts {
    /// Record only after the concrete predicate succeeded, never on test entry.
    /// # Errors
    /// Fails when the predicate is false or the receipt count overflows.
    pub fn assert(&mut self, label: impl Into<String>, predicate: bool) -> Check {
        let label = label.into();
        if !predicate {
            return Err(format!("failed assertion: {label}"));
        }
        let count = self.0.entry(label).or_default();
        *count = count.checked_add(1).ok_or("receipt overflow")?;
        Ok(())
    }
    #[must_use]
    pub fn counts(&self) -> &BTreeMap<String, u64> {
        &self.0
    }
}
fn unique(values: &[String], context: &str) -> Result<BTreeSet<String>, String> {
    let set: BTreeSet<_> = values.iter().cloned().collect();
    if set.len() != values.len() || values.is_empty() || values.iter().any(String::is_empty) {
        return Err(format!("empty/duplicate inventory: {context}"));
    }
    Ok(set)
}
/// Fail closed against a policy independent of both live observations and ledger.
/// # Errors
/// Fails on inventory drift, missing predicates, or unbound receipt labels.
pub fn close(
    inventory: &Inventory,
    receipts: &Receipts,
    ledger: &Ledger,
    policy: &Policy,
) -> Check {
    let gate = &policy.gate_b;
    if gate.command != NATIVE_COMMAND
        || gate.ledger != "spec/mcp-native-coverage.json"
        || !gate.required
        || gate.authority != "project-owned native advertised-surface verification"
        || !gate.no_skips
        || !gate.no_zero_assertion_selectors
    {
        return Err("invalid required native Gate B contract".into());
    }
    let mut required = BTreeSet::new();
    for method in policy.methods.iter().chain(&policy.legacy_methods) {
        required.insert(format!("method:{method}"));
    }
    for (items, prefix) in [
        (&inventory.tools, "tool"),
        (&inventory.resources, "resource"),
        (&inventory.templates, "template"),
        (&inventory.skills, "skill"),
        (&inventory.members, "member"),
    ] {
        let expected = unique(items, prefix)?;
        let observed: BTreeSet<_> = receipts
            .0
            .iter()
            .filter(|(_, n)| **n > 0)
            .filter_map(|(label, _)| label.strip_prefix(&format!("{prefix}:")))
            .map(|rest| {
                if prefix == "tool" {
                    rest.split(':').next().unwrap_or_default()
                } else {
                    rest
                }
            })
            .map(str::to_owned)
            .collect();
        if observed != expected {
            return Err(format!("stale/deleted fixture: {prefix}"));
        }
        for item in items {
            if prefix == "tool" {
                for phase in ["success", "error"] {
                    required.insert(format!("tool:{item}:{phase}"));
                }
                for phase in ["input", "output", "error"] {
                    required.insert(format!("schema:{item}:{phase}"));
                }
                for role in ["reader", "builder", "writer", "admin"] {
                    for writer in ["True", "False"] {
                        required.insert(format!("role:{role}:{writer}:{item}"));
                    }
                }
            } else {
                required.insert(format!("{prefix}:{item}"));
            }
        }
    }
    if unique(&inventory.classes, "classes")? != unique(&ledger.resource_classes, "ledger classes")?
    {
        return Err("resource class mismatch".into());
    }
    for class in &ledger.resource_classes {
        required.insert(format!("class:{class}"));
    }
    for test in &ledger.required_tests {
        required.insert(format!("test:{test}"));
    }
    required.extend(ledger.control_labels.iter().cloned());
    for role in ["reader", "builder", "writer", "admin"] {
        required.insert(format!("tenant:{role}"));
    }
    for method in ["resources/list", "skills/list", "concept_search"] {
        for field in ["t", "p", "m", "f", "exp", "v", "generation"] {
            required.insert(format!("cursor:{method}:{field}"));
        }
    }
    close_semantics(receipts, ledger, policy, &mut required)?;
    for label in required {
        if receipts.0.get(&label).copied().unwrap_or_default() == 0 {
            return Err(format!("missing executed assertion: {label}"));
        }
    }
    Ok(())
}
fn close_semantics(
    receipts: &Receipts,
    ledger: &Ledger,
    policy: &Policy,
    required: &mut BTreeSet<String>,
) -> Check {
    let mut applicable = BTreeSet::new();
    for scenario in &policy.scenarios {
        for check in &scenario.checks {
            if ["applicable", "applicable_fixture_blocker"].contains(&check.applicability.as_str())
                && check.id != "sep-2164-data-uri"
            {
                applicable.insert((scenario.scenario.clone(), check.id.clone()));
            }
        }
    }
    for check in &policy.skills_checks {
        if check.applicability == "applicable" {
            applicable.insert((check.scenario.clone(), check.id.clone()));
        }
    }
    let actual: BTreeSet<_> = ledger
        .requirements
        .iter()
        .map(|r| (r.scenario.clone(), r.id.clone()))
        .collect();
    if applicable.is_empty() || applicable != actual || actual.len() != ledger.requirements.len() {
        return Err("requirement coverage inventory mismatch".into());
    }
    let mut semantic = BTreeSet::new();
    for row in &ledger.requirements {
        let prefix = format!("semantic:{}:{}:", row.scenario, row.id);
        let assertions = unique(&row.assertions, &prefix)?;
        if assertions != row.minimum_receipts.keys().cloned().collect() {
            return Err(format!("missing semantic receipt contract: {prefix}"));
        }
        for (label, floor) in &row.minimum_receipts {
            if !label.starts_with(&prefix) || *floor == 0 {
                return Err(format!("invalid semantic receipt contract: {label}"));
            }
            if receipts.0.get(label).copied().unwrap_or_default() < *floor {
                return Err(format!(
                    "missing semantic receipt: {label} (minimum {floor})"
                ));
            }
            semantic.insert(label.clone());
        }
        required.extend(assertions);
    }
    let observed: BTreeSet<_> = receipts
        .0
        .keys()
        .filter(|k| k.starts_with("semantic:"))
        .cloned()
        .collect();
    if observed != semantic {
        return Err("stale/unbound semantic receipts".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Inventory, Receipts, Ledger, Policy) {
        let inventory = Inventory {
            tools: vec!["tool".into()],
            resources: vec!["r".into()],
            templates: vec!["t".into()],
            skills: vec!["s".into()],
            members: vec!["m".into()],
            classes: vec!["c".into()],
        };
        let label = "semantic:scenario:check:operation".to_owned();
        let ledger = Ledger {
            requirements: vec![Requirement {
                scenario: "scenario".into(),
                id: "check".into(),
                assertions: vec![label.clone()],
                minimum_receipts: BTreeMap::from([(label.clone(), 2)]),
            }],
            required_tests: vec!["probe".into()],
            control_labels: vec!["control:wire".into()],
            resource_classes: vec!["c".into()],
        };
        let policy = Policy {
            gate_b: GateB {
                command: NATIVE_COMMAND.into(),
                ledger: "spec/mcp-native-coverage.json".into(),
                required: true,
                authority: "project-owned native advertised-surface verification".into(),
                no_skips: true,
                no_zero_assertion_selectors: true,
            },
            methods: vec!["method".into()],
            legacy_methods: vec![],
            scenarios: vec![Scenario {
                scenario: "scenario".into(),
                checks: vec![PolicyCheck {
                    id: "check".into(),
                    applicability: "applicable".into(),
                }],
            }],
            skills_checks: vec![],
        };
        let mut receipts = Receipts::default();
        for label in [
            "method:method",
            "resource:r",
            "template:t",
            "skill:s",
            "member:m",
            "class:c",
            "test:probe",
            "control:wire",
        ] {
            receipts.assert(label, true).unwrap();
        }
        for phase in ["success", "error"] {
            receipts.assert(format!("tool:tool:{phase}"), true).unwrap();
        }
        for phase in ["input", "output", "error"] {
            receipts
                .assert(format!("schema:tool:{phase}"), true)
                .unwrap();
        }
        for role in ["reader", "builder", "writer", "admin"] {
            receipts.assert(format!("tenant:{role}"), true).unwrap();
            for writer in ["True", "False"] {
                receipts
                    .assert(format!("role:{role}:{writer}:tool"), true)
                    .unwrap();
            }
        }
        for method in ["resources/list", "skills/list", "concept_search"] {
            for field in ["t", "p", "m", "f", "exp", "v", "generation"] {
                receipts
                    .assert(format!("cursor:{method}:{field}"), true)
                    .unwrap();
            }
        }
        receipts.assert(&label, true).unwrap();
        receipts.assert(&label, true).unwrap();
        (inventory, receipts, ledger, policy)
    }
    #[test]
    fn valid_closure() {
        let (i, r, l, p) = fixture();
        close(&i, &r, &l, &p).unwrap();
    }
    #[test]
    fn active_gate_command_and_flags_are_required() {
        let (inventory, receipts, ledger, policy) = fixture();
        close(&inventory, &receipts, &ledger, &policy).unwrap();
        for field in [
            "command",
            "ledger",
            "required",
            "authority",
            "no_skips",
            "no_zero",
        ] {
            let mut bad = policy.clone();
            match field {
                "command" => bad.gate_b.command = "python removed-verifier.py".into(),
                "ledger" => bad.gate_b.ledger = "wrong.json".into(),
                "required" => bad.gate_b.required = false,
                "authority" => bad.gate_b.authority = "optional".into(),
                "no_skips" => bad.gate_b.no_skips = false,
                _ => bad.gate_b.no_zero_assertion_selectors = false,
            }
            assert!(
                close(&inventory, &receipts, &ledger, &bad).is_err(),
                "{field}"
            );
        }
    }
    #[test]
    fn failed_predicate_issues_no_receipt() {
        let mut r = Receipts::default();
        assert!(r.assert("semantic:x", false).is_err());
        assert!(r.counts().is_empty());
    }
    #[test]
    fn every_receipt_removal_fails() {
        let (i, r, l, p) = fixture();
        for label in r.0.keys() {
            let mut bad = r.clone();
            bad.0.remove(label);
            assert!(close(&i, &bad, &l, &p).is_err(), "{label}");
        }
    }
    #[test]
    fn semantic_floor_substitution_fails() {
        let (i, r, l, p) = fixture();
        for n in [0, 1] {
            let mut bad = r.clone();
            bad.0.insert("semantic:scenario:check:operation".into(), n);
            assert!(close(&i, &bad, &l, &p).is_err());
        }
    }
    #[test]
    fn stale_semantics_fail() {
        let (i, mut r, l, p) = fixture();
        r.assert("semantic:stale:check:operation", true).unwrap();
        assert!(close(&i, &r, &l, &p).is_err());
    }
    #[test]
    fn removed_requirement_does_not_reduce_scope() {
        let (i, r, mut l, p) = fixture();
        l.requirements.clear();
        assert!(close(&i, &r, &l, &p).is_err());
    }
    #[test]
    fn reverse_join_rejects_deleted_and_added_discovery() {
        let (i, r, l, p) = fixture();
        for delete in [true, false] {
            let mut bad = i.clone();
            if delete {
                bad.resources.clear();
            } else {
                bad.resources.push("uncovered".into());
            }
            assert!(close(&bad, &r, &l, &p).is_err());
        }
    }
    #[test]
    fn duplicate_requirements_and_assertions_fail() {
        let (i, r, l, p) = fixture();
        let mut bad = l.clone();
        bad.requirements.push(bad.requirements[0].clone());
        assert!(close(&i, &r, &bad, &p).is_err());
        let mut bad = l;
        let label = bad.requirements[0].assertions[0].clone();
        bad.requirements[0].assertions.push(label);
        assert!(close(&i, &r, &bad, &p).is_err());
    }
}
