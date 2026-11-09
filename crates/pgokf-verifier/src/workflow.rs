//! Workflow policy over parsed YAML, with closed reviewed execution contexts.
//! Hash equality is a review boundary, not a sandbox for arbitrary shell code.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

pub const CLEAN_SHELL: &str =
    "/usr/bin/env -u BASH_ENV -u ENV /bin/bash --noprofile --norc -e -o pipefail {0}";
pub type Row = BTreeMap<String, Value>;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Matrix {
    #[serde(default)]
    pub include: Option<Vec<Row>>,
    #[serde(default)]
    pub exclude: Option<Vec<Row>>,
    #[serde(flatten)]
    pub axes: BTreeMap<String, Vec<Value>>,
}

/// Actions applies exclusions to original combinations before merging includes.
/// Includes cannot overwrite an original axis; unmatched includes add jobs.
#[must_use]
pub fn effective_matrix(matrix: &Matrix) -> Vec<Row> {
    let mut original = if matrix.axes.is_empty() {
        Vec::new()
    } else {
        vec![Row::new()]
    };
    for (key, values) in &matrix.axes {
        original = original
            .iter()
            .flat_map(|row| {
                values.iter().map(|value| {
                    let mut next = row.clone();
                    next.insert(key.clone(), value.clone());
                    next
                })
            })
            .collect();
    }
    original.retain(|row| {
        !matrix
            .exclude
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|exclusion| {
                exclusion
                    .iter()
                    .all(|(key, value)| row.get(key) == Some(value))
            })
    });
    let mut rows = original.clone();
    let mut extras = Vec::new();
    for inclusion in matrix.include.as_deref().unwrap_or_default() {
        let mut matched = false;
        for (base, row) in original.iter().zip(&mut rows) {
            if inclusion
                .iter()
                .all(|(key, value)| base.get(key).is_none_or(|original| original == value))
            {
                row.extend(inclusion.clone());
                matched = true;
            }
        }
        if !matched {
            extras.push(inclusion.clone());
        }
    }
    rows.extend(extras);
    rows
}

/// Resolve aliases and YAML merge keys before policy. Duplicate YAML keys fail.
/// # Errors
/// Invalid YAML, non-string mapping keys or non-object workflow roots fail.
pub fn parse(source: &str) -> Result<Value, String> {
    fn string_keys(value: &serde_yaml::Value) -> Result<(), String> {
        match value {
            serde_yaml::Value::Mapping(mapping) => {
                for (key, value) in mapping {
                    if !key.is_string() {
                        return Err("workflow mapping keys must be strings".into());
                    }
                    string_keys(value)?;
                }
            }
            serde_yaml::Value::Sequence(values) => {
                for value in values {
                    string_keys(value)?;
                }
            }
            serde_yaml::Value::Tagged(_) => return Err("custom YAML tags are unsupported".into()),
            _ => {}
        }
        Ok(())
    }
    let mut yaml: serde_yaml::Value =
        serde_yaml::from_str(source).map_err(|error| error.to_string())?;
    string_keys(&yaml)?;
    yaml.apply_merge().map_err(|error| error.to_string())?;
    let value = serde_json::to_value(yaml).map_err(|error| error.to_string())?;
    if !value.is_object() {
        return Err("workflow must be an object".into());
    }
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContext {
    pub workflow: String,
    pub triggers: String,
    pub permissions: String,
    pub env: String,
    pub jobs: BTreeMap<String, String>,
}
pub type Contexts = BTreeMap<String, ExecutionContext>;

// Python json.dumps(sort_keys=True,separators=(',', ':')) escapes non-ASCII.
// Serialize sorted JSON first, then reproduce ensure_ascii without changing
// ASCII escape sequences. Values in GitHub workflow jobs use integer numbers;
// reject floats instead of silently claiming cross-language formatting parity.
fn canonical(value: &Value) -> Result<String, String> {
    fn sorted(value: &Value) -> Result<Value, String> {
        Ok(match value {
            Value::Number(number) if number.is_f64() => {
                return Err("floating-point execution-context value is unsupported".into());
            }
            Value::Array(values) => {
                Value::Array(values.iter().map(sorted).collect::<Result<_, _>>()?)
            }
            Value::Object(values) => {
                let sorted: BTreeMap<_, _> = values
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), sorted(value)?)))
                    .collect::<Result<_, String>>()?;
                serde_json::to_value(sorted).map_err(|error| error.to_string())?
            }
            _ => value.clone(),
        })
    }
    let json = serde_json::to_string(&sorted(value)?).map_err(|error| error.to_string())?;
    let mut escaped = String::new();
    for character in json.chars() {
        if character.is_ascii() {
            escaped.push(character);
        } else {
            for code in character.encode_utf16(&mut [0; 2]).iter() {
                write!(escaped, "\\u{code:04x}").expect("writing String cannot fail");
            }
        }
    }
    Ok(escaped)
}

/// # Errors
/// Missing job maps or unsupported numeric values fail.
pub fn execution_context(workflow: &Value) -> Result<ExecutionContext, String> {
    fn digest(value: &Value) -> Result<String, String> {
        Ok(Sha256::digest(canonical(value)?.as_bytes()).iter().fold(
            String::new(),
            |mut output, byte| {
                write!(output, "{byte:02x}").expect("writing String cannot fail");
                output
            },
        ))
    }
    let jobs = workflow
        .get("jobs")
        .and_then(Value::as_object)
        .ok_or("workflow jobs must be an object")?;
    let empty = json!({});
    let triggers = workflow.get("on").ok_or("workflow triggers are required")?;
    let permissions = workflow
        .get("permissions")
        .ok_or("workflow permissions are required")?;
    Ok(ExecutionContext {
        workflow: digest(workflow)?,
        triggers: digest(triggers)?,
        permissions: digest(permissions)?,
        env: digest(workflow.get("env").unwrap_or(&empty))?,
        jobs: jobs
            .iter()
            .map(|(name, job)| Ok((name.clone(), digest(job)?)))
            .collect::<Result<_, String>>()?,
    })
}

fn hook(text: &str) -> bool {
    text.contains("BASH_FUNC_")
        || text
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|token| {
                matches!(
                    token,
                    "BASH_ENV" | "ENV" | "SHELLOPTS" | "BASHOPTS" | "GITHUB_ENV"
                )
            })
}
fn inspect(scope: &Value, errors: &mut Vec<String>) {
    if let Some(env) = scope.get("env") {
        if env
            .as_object()
            .is_none_or(|env| env.keys().any(|key| hook(key)))
        {
            errors.push("startup environment injection".into());
        }
        if env.get("PATH").is_some() {
            errors.push("PATH environment override".into());
        }
    }
    let Some(object) = scope.as_object() else {
        errors.push("workflow scope must be an object".into());
        return;
    };
    let values: BTreeMap<_, _> = object
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "jobs" | "steps" | "defaults"))
        .collect();
    let text = serde_json::to_string(&values).expect("JSON values serialize");
    if text.contains("GITHUB_PATH") || text.contains("::add-path::") {
        errors.push("GITHUB_PATH manipulation".into());
    }
    if let Some(run) = scope.get("run") {
        if let Some(run) = run.as_str() {
            if run
                .split("PATH")
                .skip(1)
                .any(|tail| tail.trim_start().starts_with('='))
            {
                errors.push("PATH command override".into());
            }
            if hook(run) {
                errors.push("startup/environment-file manipulation".into());
            }
        } else {
            errors.push("run must be a string".into());
        }
    }
}

/// Enforce exact reviewed job definitions plus explicit clean shell selection.
/// Token checks supply diagnostics; only the closed digest admits code.
#[must_use]
pub fn check_shell_policy(workflow: &Value, contracts: &Contexts) -> Vec<String> {
    let mut errors = Vec::new();
    if workflow.get("defaults") != Some(&json!({"run":{"shell":CLEAN_SHELL}})) {
        errors.push("explicit clean strict shell required".into());
    }
    let expected = workflow
        .get("name")
        .and_then(Value::as_str)
        .and_then(|name| contracts.get(name));
    match execution_context(workflow) {
        Ok(actual) if Some(&actual) == expected => {}
        _ => errors.push("unreviewed shell-selection execution context".into()),
    }
    inspect(workflow, &mut errors);
    let Some(jobs) = workflow.get("jobs").and_then(Value::as_object) else {
        return errors;
    };
    for job in jobs.values() {
        inspect(job, &mut errors);
        if job.get("defaults").is_some() {
            errors.push("job shell override".into());
        }
        if let Some(steps) = job.get("steps") {
            if let Some(steps) = steps.as_array() {
                for step in steps {
                    inspect(step, &mut errors);
                    if step.get("shell").is_some() {
                        errors.push("step shell override".into());
                    }
                }
            } else {
                errors.push("steps must be an array".into());
            }
        }
    }
    errors
}

fn matrix(workflow: &Value, job: &str) -> Result<Matrix, String> {
    serde_json::from_value(workflow["jobs"][job]["strategy"]["matrix"].clone())
        .map_err(|error| format!("{job}: invalid matrix: {error}"))
}
fn dependencies(job: &Value) -> BTreeSet<&str> {
    match job.get("needs") {
        Some(Value::String(name)) => BTreeSet::from([name.as_str()]),
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).collect(),
        _ => BTreeSet::new(),
    }
}

/// Enforce the strict-build, manual-only GitHub Pages deployment boundary.
#[must_use]
#[allow(clippy::too_many_lines)] // One closed workflow boundary with explicit diagnostics.
pub fn check_pages_policy(docs: &Value, contexts: &Contexts) -> Vec<String> {
    const POLICY_COMMAND: &str = "cargo metadata --locked --no-deps --format-version 1 | bash scripts/pages-deploy-policy.sh";
    const DEPLOY_CONDITION: &str = "${{ github.event_name == 'workflow_dispatch' && inputs.deploy_pages == true && needs.build.result == 'success' && needs.pages-policy.result == 'success' && needs.pages-policy.outputs.deploy == 'true' }}";
    let mut errors = Vec::new();

    if docs.get("defaults").is_some() {
        errors.push("Pages workflow shell defaults are forbidden".into());
    }
    if docs.get("concurrency").is_some() {
        errors.push("workflow concurrency must not suppress strict Pages builds".into());
    }
    match execution_context(docs) {
        Ok(actual) if contexts.get("Docs") == Some(&actual) => {}
        _ => errors.push("unreviewed Pages execution context".into()),
    }

    if docs["on"]["workflow_dispatch"]["inputs"]["deploy_pages"]
        != json!({
            "description":"Manually deploy the successfully built site to GitHub Pages",
            "required":true,
            "type":"boolean",
            "default":false
        })
    {
        errors
            .push("manual Pages deployment requires an explicit default-false allow input".into());
    }
    if docs["on"]["push"] != json!({"branches":["main"]}) {
        errors.push("strict Pages build must run on every main-branch push".into());
    }
    if docs["permissions"] != json!({"contents":"read"}) {
        errors.push("Pages workflow-level permissions must be read-only".into());
    }

    let Some(jobs) = docs["jobs"].as_object() else {
        errors.push("Pages jobs missing".into());
        return errors;
    };
    if jobs.keys().map(String::as_str).collect::<BTreeSet<_>>()
        != BTreeSet::from(["pages-policy", "build", "deploy"])
    {
        errors.push("Pages job inventory changed".into());
    }
    let policy = &docs["jobs"]["pages-policy"];
    if policy["runs-on"] != "ubuntu-latest"
        || policy.get("if").is_some()
        || policy.get("continue-on-error").is_some()
        || policy.get("needs").is_some()
        || policy.get("permissions").is_some()
        || policy["outputs"]
            != json!({
                "version":"${{ steps.policy.outputs.version }}",
                "deploy":"${{ steps.policy.outputs.deploy }}"
            })
    {
        errors.push("Pages policy job must run directly and fail closed".into());
    }
    if policy["steps"]
        != json!([
            {
                "name":"Checkout",
                "uses":"actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"
            },
            {
                "name":"Resolve manual-only Pages deployment policy",
                "id":"policy",
                "env":{"INPUT_DEPLOY_PAGES":"${{ inputs.deploy_pages }}"},
                "run":POLICY_COMMAND
            }
        ])
    {
        errors.push("exact Cargo-metadata Pages policy execution required".into());
    }

    let build = &docs["jobs"]["build"];
    let build_steps = build["steps"].as_array();
    if build["runs-on"] != "ubuntu-latest"
        || build.get("if").is_some()
        || build.get("continue-on-error").is_some()
        || build.get("needs").is_some()
        || build.get("permissions").is_some()
        || build_steps.is_none_or(|steps| {
            steps.iter().any(|step| {
                step.get("if").is_some()
                    || step
                        .get("continue-on-error")
                        .is_some_and(|value| value != &json!(false))
            })
        })
        || build_steps.is_none_or(|steps| {
            steps
                .iter()
                .filter(|step| step["run"] == "mkdocs build --strict")
                .count()
                != 1
        })
    {
        errors.push("strict Pages build must remain an unconditional required job".into());
    }

    let deploy = &docs["jobs"]["deploy"];
    if deploy["needs"] != json!(["build", "pages-policy"])
        || deploy["if"] != DEPLOY_CONDITION
        || deploy.get("continue-on-error").is_some()
        || deploy["runs-on"] != "ubuntu-latest"
        || deploy["permissions"] != json!({"pages":"write","id-token":"write"})
        || deploy["concurrency"] != json!({"group":"pages","cancel-in-progress":false})
        || deploy["environment"]
            != json!({
                "name":"github-pages",
                "url":"${{ steps.deployment.outputs.page_url }}"
            })
    {
        errors.push(
            "Pages deployment must require successful build and approved policy output".into(),
        );
    }
    if deploy["steps"]
        != json!([{
            "name":"Deploy",
            "id":"deployment",
            "uses":"actions/deploy-pages@cd2ce8fcbc39b97be8ca5fce6e763baed58fa128"
        }])
    {
        errors.push("exact pinned Pages deployment step required".into());
    }

    let mut deploy_actions = Vec::new();
    for (job_name, job) in jobs {
        for step in job["steps"].as_array().into_iter().flatten() {
            if step["uses"]
                .as_str()
                .is_some_and(|action| action.starts_with("actions/deploy-pages@"))
            {
                deploy_actions.push((job_name.as_str(), step));
            }
        }
    }
    if deploy_actions.len() != 1
        || deploy_actions[0].0 != "deploy"
        || deploy_actions[0].1["id"] != "deployment"
        || deploy_actions[0].1["uses"]
            != "actions/deploy-pages@cd2ce8fcbc39b97be8ca5fce6e763baed58fa128"
        || deploy_actions[0].1.get("if").is_some()
        || deploy_actions[0].1.get("continue-on-error").is_some()
    {
        errors.push("unique pinned Pages deployment action required".into());
    }
    errors
}

/// Stable packaging remains PG15–18; pinned PG19 compatibility is required.
/// Exact execution-context contracts bind all current commands and conditions.
#[must_use]
#[allow(clippy::too_many_lines)] // One support-matrix boundary checks every required leg.
pub fn check_support(packages: &Value, pgrx: &Value, contexts: &Contexts) -> Vec<String> {
    let mut errors = check_shell_policy(packages, contexts);
    errors.extend(check_shell_policy(pgrx, contexts));
    let stable = vec![json!(15), json!(16), json!(17), json!(18)];
    for job in ["deb", "docker", "docker-publish", "docker-manifest"] {
        match matrix(packages, job) {
            Ok(m)
                if m.axes.get("pg") == Some(&stable)
                    && !m
                        .include
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .any(|row| row.contains_key("pg"))
                    && m.exclude.as_deref().unwrap_or_default().is_empty() => {}
            _ => errors.push(format!("stable GA boundary: {job}")),
        }
    }
    let beta = BTreeMap::from([
        ("pg".into(), json!(19)),
        ("image_tag".into(), json!("19beta3")),
    ]);
    match matrix(pgrx, "test") {
        Ok(m) => {
            if m.axes.get("pg") != Some(&stable)
                || m.include.as_deref() != Some(std::slice::from_ref(&beta))
            {
                errors.push("required pinned beta compatibility".into());
            }
            let mut expected: Vec<Row> = stable
                .into_iter()
                .map(|pg| BTreeMap::from([("pg".into(), pg)]))
                .collect();
            expected.push(beta);
            if effective_matrix(&m) != expected {
                errors.push("effective compatibility coverage changed".into());
            }
        }
        Err(error) => errors.push(error),
    }
    let job = &pgrx["jobs"]["test"];
    if job
        .get("continue-on-error")
        .is_some_and(|v| v != &json!(false))
        || job.get("if").is_some()
        || job.get("defaults").is_some()
    {
        errors.push("compatibility must fail closed".into());
    }
    let compatibility = &packages["jobs"]["compatibility"];
    if compatibility.get("if").is_some()
        || compatibility
            .get("continue-on-error")
            .is_some_and(|v| v != &json!(false))
        || compatibility.get("with")
            != Some(&json!({"source_ref":"${{ needs.prep.outputs.source_ref }}"}))
    {
        errors.push("compatibility source/dependency bypass".into());
    }
    if compatibility.get("uses").and_then(Value::as_str)
        != Some("./.github/workflows/pgrx-test.yml")
    {
        errors.push("publication requires compatibility".into());
    }
    for name in ["docker", "companions"] {
        let needs = dependencies(&packages["jobs"][name]);
        if ![
            "prep",
            "notices",
            "lint",
            "meta",
            "deb",
            "compatibility",
            "homebrew-policy",
        ]
        .into_iter()
        .all(|job| needs.contains(job))
        {
            errors.push(format!("publication gate: {name}"));
        }
    }
    for (name, build) in [
        ("docker-publish", "docker"),
        ("companions-publish", "companions"),
    ] {
        let job = &packages["jobs"][name];
        let needs = dependencies(job);
        if job["if"] != "needs.prep.outputs.publish == 'true'"
            || job["permissions"] != json!({"contents":"read","packages":"write"})
            || !["prep", "notices", build]
                .into_iter()
                .all(|dependency| needs.contains(dependency))
        {
            errors.push(format!("dispatch-only publication authority: {name}"));
        }
    }
    for name in ["docker-manifest", "companions-manifest"] {
        let needs = dependencies(&packages["jobs"][name]);
        if !["docker-publish", "companions-publish"]
            .into_iter()
            .all(|job| needs.contains(job))
        {
            errors.push(format!("all builds before release tags: {name}"));
        }
    }
    errors
}

fn normalized(path: &str) -> Option<String> {
    if path.starts_with('/') {
        return None;
    }
    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            _ => components.push(component),
        }
    }
    Some(if components.is_empty() {
        ".".into()
    } else {
        components.join("/")
    })
}

/// Deepest containing checkout wins; latest checkout wins at equal depth.
#[must_use]
pub fn owning_checkout<'a>(
    steps: &'a [Value],
    index: usize,
    path: &str,
) -> Option<(String, &'a Value)> {
    let path = normalized(path)?;
    let mut best: Option<(usize, String, &Value)> = None;
    for step in steps.iter().take(index) {
        if !step
            .get("uses")
            .and_then(Value::as_str)
            .is_some_and(|action| action.starts_with("actions/checkout@"))
        {
            continue;
        }
        let location = normalized(step["with"]["path"].as_str().unwrap_or("."))?;
        if location == "." || path == location || path.starts_with(&(location.clone() + "/")) {
            let depth = if location == "." {
                0
            } else {
                location.split('/').count()
            };
            if best.as_ref().is_none_or(|(old, _, _)| depth >= *old) {
                best = Some((depth, location, step));
            }
        }
    }
    best.map(|(_, path, step)| (path, step))
}

#[must_use]
pub fn full_history(checkout: &Value) -> bool {
    checkout.get("if").is_none()
        && checkout
            .get("continue-on-error")
            .is_none_or(|value| value == &json!(false))
        && matches!(&checkout["with"]["fetch-depth"],Value::Number(n) if n.as_u64()==Some(0))
        || checkout.get("if").is_none()
            && checkout
                .get("continue-on-error")
                .is_none_or(|value| value == &json!(false))
            && checkout["with"]["fetch-depth"] == json!("0")
}

/// A closed Rust command registry declares source history consumers explicitly.
/// No shell-string token extraction or Python source AST introspection is used.
#[derive(Debug, Clone)]
pub struct HistoryRequirement {
    pub job: String,
    pub step_name: String,
    pub consumer_path: String,
}

#[must_use]
pub fn check_history_policy(workflow: &Value, required: &[HistoryRequirement]) -> Vec<String> {
    let mut errors = Vec::new();
    for requirement in required {
        let Some(steps) = workflow["jobs"][&requirement.job]["steps"].as_array() else {
            errors.push(format!("{}: history consumer job missing", requirement.job));
            continue;
        };
        let matches: Vec<_> = steps
            .iter()
            .enumerate()
            .filter(|(_, step)| step["name"].as_str() == Some(&requirement.step_name))
            .collect();
        if matches.len() != 1 {
            errors.push(format!(
                "{}: unique history consumer step required",
                requirement.job
            ));
            continue;
        }
        match owning_checkout(steps, matches[0].0, &requirement.consumer_path) {
            Some((_, checkout))
                if full_history(checkout) && checkout["with"].get("repository").is_none() => {}
            _ => errors.push(format!(
                "{}: {} needs release history from its owning checkout",
                requirement.job, requirement.consumer_path
            )),
        }
    }
    errors
}

fn gated(
    name: &str,
    jobs: &serde_json::Map<String, Value>,
    visiting: &mut BTreeSet<String>,
) -> bool {
    if !visiting.insert(name.into()) {
        return false;
    }
    let Some(job) = jobs.get(name) else {
        return false;
    };
    if job
        .get("continue-on-error")
        .is_some_and(|v| v != &json!(false))
    {
        return false;
    }
    let condition = job["if"].as_str().unwrap_or("");
    if ["always(", "failure(", "cancelled("]
        .iter()
        .any(|c| condition.contains(c))
    {
        return false;
    }
    if name == "prep" {
        return job.get("if").is_none();
    }
    let needs = dependencies(job);
    let result = needs.iter().any(|n| gated(n, jobs, &mut visiting.clone()));
    visiting.remove(name);
    result
}

fn valid_graph(jobs: &serde_json::Map<String, Value>) -> bool {
    fn visit(
        name: &str,
        jobs: &serde_json::Map<String, Value>,
        active: &mut BTreeSet<String>,
        done: &mut BTreeSet<String>,
    ) -> bool {
        if done.contains(name) {
            return true;
        }
        let Some(job) = jobs.get(name) else {
            return false;
        };
        if !active.insert(name.into()) {
            return false;
        }
        let needs = match job.get("needs") {
            None => Vec::new(),
            Some(Value::String(name)) => vec![name.as_str()],
            Some(Value::Array(names)) => {
                let Some(names) = names.iter().map(Value::as_str).collect::<Option<Vec<_>>>()
                else {
                    return false;
                };
                names
            }
            _ => return false,
        };
        if needs
            .into_iter()
            .any(|dependency| !visit(dependency, jobs, active, done))
        {
            return false;
        }
        active.remove(name);
        done.insert(name.into());
        true
    }
    let mut done = BTreeSet::new();
    jobs.keys()
        .all(|name| visit(name, jobs, &mut BTreeSet::new(), &mut done))
}

fn active_named_command(job: &Value, name: &str, command: &str) -> bool {
    let matches: Vec<_> = job["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|step| step["name"] == name)
        .collect();
    matches.len() == 1
        && matches[0]["run"] == command
        && matches[0].get("if").is_none()
        && matches[0].get("continue-on-error").is_none()
        && matches[0].get("shell").is_none()
}

fn notice_job_contract(workflow: &Value, packages: bool) -> bool {
    let job = &workflow["jobs"]["notices"];
    if job.get("if").is_some()
        || job.get("continue-on-error").is_some()
        || job.get("defaults").is_some()
    {
        return false;
    }
    if (packages && dependencies(job) != BTreeSet::from(["prep"]))
        || (!packages && job.get("needs").is_some())
    {
        return false;
    }
    let Some(steps) = job["steps"].as_array() else {
        return false;
    };
    let checkouts: Vec<_> = steps
        .iter()
        .filter(|step| {
            step["uses"]
                .as_str()
                .is_some_and(|uses| uses.starts_with("actions/checkout@"))
        })
        .collect();
    let checkout_contract = if packages {
        checkouts.len() == 2
            && checkouts[0]["name"] == "Checkout selected release source"
            && checkouts[0]["with"]
                == json!({"persist-credentials":false,"ref":"${{ needs.prep.outputs.source_ref }}","path":"release-source"})
            && checkouts[1]["name"] == "Checkout current provenance tooling"
            && checkouts[1]["with"]
                == json!({"persist-credentials":false,"ref":"${{ github.sha }}","path":"release-tools"})
            && checkouts.iter().all(|checkout| {
                checkout.get("if").is_none() && checkout.get("continue-on-error").is_none()
            })
    } else {
        checkouts.len() == 1
            && checkouts[0]["with"] == json!({"persist-credentials":false})
            && checkouts[0].get("if").is_none()
            && checkouts[0].get("continue-on-error").is_none()
    };
    if !checkout_contract {
        return false;
    }
    let install = if packages {
        "release-tools/scripts/install-cargo-about.sh \"$RUNNER_TEMP/cargo-about\""
    } else {
        "scripts/install-cargo-about.sh \"$RUNNER_TEMP/cargo-about\""
    };
    let verify = if packages {
        "cargo run --locked --manifest-path release-tools/Cargo.toml -p pgokf-verifier -- notices-verify release-source \"$RUNNER_TEMP/cargo-about\""
    } else {
        "cargo run --locked -p pgokf-verifier -- notices-verify . \"$RUNNER_TEMP/cargo-about\""
    };
    active_named_command(job, "Install checksum-pinned cargo-about", install)
        && active_named_command(job, "Verify deterministic third-party notices", verify)
}

/// Explicit release topology and source/tooling checkout contracts supplement
/// the reviewed execution hashes. Unknown or cyclic publication graphs fail.
#[must_use]
#[allow(clippy::too_many_lines)] // The release graph is one review boundary.
pub fn check_release(packages: &Value, ci: &Value, pgrx: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(jobs) = packages["jobs"].as_object() else {
        return vec!["release jobs missing".into()];
    };
    if !valid_graph(jobs) {
        errors.push("release dependency graph is cyclic, malformed or incomplete".into());
    }
    if !notice_job_contract(packages, true) {
        errors.push("authoritative package notice job changed".into());
    }
    if !notice_job_contract(ci, false) {
        errors.push("authoritative CI notice verification changed".into());
    }
    let inputs = &packages["on"]["workflow_dispatch"]["inputs"];
    let release_tag = &inputs["release_tag"];
    if inputs.as_object().is_none_or(|inputs| inputs.len() != 1)
        || release_tag["type"] != "string"
        || release_tag["required"] != false
        || release_tag["default"] != ""
        || release_tag["description"]
            .as_str()
            .is_none_or(str::is_empty)
    {
        errors.push("immutable release tag input required".into());
    }
    let mut artifacts = BTreeSet::new();
    for (name, job) in jobs {
        let package_write = job["permissions"]["packages"] == "write";
        if package_write && job["if"] != "needs.prep.outputs.publish == 'true'" {
            errors.push(format!(
                "package-write authority is not dispatch-only: {name}"
            ));
        }
        let Some(steps) = job["steps"].as_array() else {
            continue;
        };
        if package_write
            && steps.iter().any(|step| {
                step["uses"]
                    .as_str()
                    .is_some_and(|uses| uses.starts_with("actions/checkout@"))
                    || step["run"]
                        .as_str()
                        .is_some_and(|run| run.contains("cargo run"))
            })
        {
            errors.push(format!(
                "{name}: repository-controlled code executes under package-write authority"
            ));
        }
        for step in steps {
            if step["uses"]
                .as_str()
                .is_some_and(|u| u.contains("upload-artifact@"))
                || step["with"]["push"] == true
                || step["run"]
                    .as_str()
                    .is_some_and(|r| r.contains("imagetools create"))
            {
                artifacts.insert(name.as_str());
                if !gated(name, jobs, &mut BTreeSet::new()) {
                    errors.push(format!("publication bypasses prep: {name}"));
                }
            }
            if name != "prep"
                && step["uses"]
                    .as_str()
                    .is_some_and(|u| u.starts_with("actions/checkout@"))
            {
                if step["with"]["persist-credentials"] != false {
                    errors.push(format!("{name}: checkout credentials must not persist"));
                }
                if matches!(
                    step["name"].as_str(),
                    Some(
                        "Checkout release validation tooling"
                            | "Checkout current provenance tooling"
                    )
                ) {
                    if step["with"]
                        != json!({"persist-credentials":false,"ref":"${{ github.sha }}","path":"release-tools"})
                    {
                        errors.push("current tooling checkout changed".into());
                    }
                } else if step["with"]["ref"] != "${{ needs.prep.outputs.source_ref }}" {
                    errors.push(format!("{name}: proven source checkout required"));
                }
            }
        }
        if let Some(env) = job["env"].as_object() {
            for key in ["VERSION", "LOCAL_TAG", "TAG"] {
                if env.get(key).is_some_and(|v| {
                    !v.as_str()
                        .unwrap_or("")
                        .contains("needs.prep.outputs.version")
                }) {
                    errors.push(format!("{name}: proven package version required"));
                }
            }
        }
    }
    for name in ["docker-publish", "companions-publish"] {
        let steps = jobs[name]["steps"].as_array();
        let Some(steps) = steps else {
            errors.push(format!("{name}: publication steps missing"));
            continue;
        };
        let uses: Vec<_> = steps
            .iter()
            .filter_map(|step| step["uses"].as_str())
            .collect();
        let runs = steps
            .iter()
            .filter_map(|step| step["run"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if uses.iter().any(|uses| {
            uses.starts_with("actions/checkout@") || uses.contains("build-push-action@")
        }) || !uses
            .iter()
            .any(|uses| uses.starts_with("actions/download-artifact@"))
            || !runs.contains("docker load --input")
            || !runs.contains("docker push")
        {
            errors.push(format!(
                "{name}: publication must push the downloaded smoke-tested image without rebuilding"
            ));
        }
    }
    if artifacts
        != BTreeSet::from([
            "deb",
            "docker",
            "docker-publish",
            "companions",
            "companions-publish",
            "docker-manifest",
            "companions-manifest",
        ])
    {
        errors.push("artifact job inventory changed".into());
    }
    for name in &artifacts {
        if !dependencies(&jobs[*name]).contains("notices") {
            errors.push(format!("artifact bypasses notices: {name}"));
        }
    }
    // The production deb build depends on the maintained packaging suite:
    // build-deb.sh must not run in this workflow unless its own test suite
    // passed first on the same proven source, under no write authority.
    let absent = &Value::Null;
    let suite = jobs.get("deb-suite").unwrap_or(absent);
    if suite.get("if").is_some()
        || suite.get("continue-on-error").is_some()
        || suite.get("defaults").is_some()
        || suite["permissions"]["packages"] == "write"
        || !active_named_command(
            suite,
            "Run the maintained build-deb.sh suite",
            "packaging/deb/test-build-deb.sh",
        )
    {
        errors.push("maintained deb suite execution changed".into());
    }
    if !dependencies(jobs.get("deb").unwrap_or(absent)).contains("deb-suite") {
        errors.push("deb build bypasses the maintained deb suite".into());
    }
    if let Some(steps) = packages["jobs"]["prep"]["steps"].as_array() {
        let resolve: Vec<_> = steps
            .iter()
            .enumerate()
            .filter(|(_, s)| s["id"] == "resolve")
            .collect();
        if resolve.len() == 1 {
            let (index, step) = resolve[0];
            if step["run"] != "../release-tools/packaging/resolve-release.sh"
                || step.get("if").is_some()
                || step.get("continue-on-error").is_some()
            {
                errors.push("required resolver execution changed".into());
            }
            let cwd = step["working-directory"].as_str().unwrap_or(".");
            if owning_checkout(steps, index, cwd).is_none_or(|(_, c)| !full_history(c)) {
                errors.push("prep checkout must fetch-depth: 0 to prove tag identity".into());
            }
        } else {
            errors.push("unique resolver required".into());
        }
    } else {
        errors.push("resolver steps missing".into());
    }
    let checkouts: Vec<_> = pgrx["jobs"]["test"]["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| {
            s["uses"]
                .as_str()
                .is_some_and(|u| u.starts_with("actions/checkout@"))
        })
        .collect();
    if checkouts.len() != 2
        || checkouts[0]["with"]
            != json!({"persist-credentials":false,"ref":"${{ inputs.source_ref || github.sha }}"})
        || checkouts[1]["with"]
            != json!({"persist-credentials":false,"ref":"${{ github.sha }}","path":"release-tools"})
        || checkouts
            .iter()
            .any(|s| s.get("if").is_some() || s.get("continue-on-error").is_some())
    {
        errors.push("compatibility source/tooling checkout contract changed".into());
    }
    for (workflow, job, name) in [
        (ci, "rust", "Test required PostgreSQL coverage guards"),
        (
            packages,
            "lint",
            "Portable source-packaging and cleanup guards",
        ),
    ] {
        errors.extend(check_history_policy(
            workflow,
            &[HistoryRequirement {
                job: job.into(),
                step_name: name.into(),
                consumer_path: ".".into(),
            }],
        ));
        let guard = &workflow["jobs"][job];
        if guard.get("if").is_some() || guard.get("continue-on-error").is_some() {
            errors.push("portable guards disabled".into());
        }
        let matches: Vec<_> = guard["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["name"] == name)
            .collect();
        let expected = "cargo test --locked -p pgokf-verifier\ncargo run --locked -p pgokf-verifier -- history-check .\ncargo run --locked -p pgokf-verifier -- release-check .\ncargo run --locked -p pgokf-verifier -- workflow-check .";
        if matches.len() != 1
            || matches[0]["run"].as_str().map(str::trim) != Some(expected)
            || matches[0].get("if").is_some()
            || matches[0].get("continue-on-error").is_some()
            || matches[0].get("shell").is_some()
        {
            errors.push("required commands are not active direct execution".into());
        }
        let homebrew = &workflow["jobs"]["homebrew-policy"];
        if homebrew["runs-on"] != "macos-15"
            || homebrew.get("if").is_some()
            || homebrew.get("continue-on-error").is_some()
        {
            errors.push("required macOS job disabled".into());
        }
        let matches: Vec<_> = homebrew["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["name"] == "Execute actual Homebrew environment policy")
            .collect();
        let command = if job == "rust" {
            "cargo run --locked -p pgokf-verifier -- homebrew-policy ."
        } else {
            "cargo run --locked --manifest-path release-tools/Cargo.toml -p pgokf-verifier -- homebrew-policy release-tools"
        };
        if matches.len() != 1
            || matches[0]["run"] != command
            || matches[0].get("if").is_some()
            || matches[0].get("continue-on-error").is_some()
        {
            errors.push("Homebrew API execution contract changed".into());
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    fn matrix_yaml(source: &str) -> Matrix {
        serde_yaml::from_str(source).unwrap()
    }
    #[test]
    fn effective_matrix_exclusion_then_include_and_nulls() {
        let m = matrix_yaml(
            "pg: [18]\narch: [amd64, arm64]\nexclude: [{arch: arm64}]\ninclude: [{pg: 18, runner: linux}, {pg: 19, image_tag: 19beta3}]",
        );
        assert_eq!(
            serde_json::to_value(effective_matrix(&m)).unwrap(),
            json!([{"pg":18,"arch":"amd64","runner":"linux"},{"pg":19,"image_tag":"19beta3"}])
        );
        assert_eq!(
            effective_matrix(&matrix_yaml("include: [{arch: amd64}, {arch: arm64}]")).len(),
            2
        );
        assert_eq!(
            effective_matrix(&matrix_yaml("pg: [18]\ninclude: null\nexclude: null")).len(),
            1
        );
        assert_eq!(
            effective_matrix(&matrix_yaml(
                "pg: [18]\nexclude: [{pg: 18}]\ninclude: [{pg: 19}]"
            ))[0]["pg"],
            json!(19)
        );
    }
    #[test]
    fn empty_axes_and_unknown_matrix_shapes_fail_closed() {
        assert!(effective_matrix(&matrix_yaml("pg: []")).is_empty());
        assert!(serde_yaml::from_str::<Matrix>("pg: 18").is_err());
        assert!(serde_yaml::from_str::<Matrix>("include: [19]").is_err());
    }
    #[test]
    fn canonical_matches_python_ascii_unicode_and_sorted_objects() {
        assert_eq!(
            canonical(&json!({"z":"é😀","a":{"b":true,"a":null}})).unwrap(),
            r#"{"a":{"a":null,"b":true},"z":"\u00e9\ud83d\ude00"}"#
        );
        assert!(canonical(&json!(0.1)).is_err());
    }
    fn fixture() -> (Value, Contexts) {
        let workflow = json!({"name":"example","on":{"push":{}},"permissions":{"contents":"read"},"defaults":{"run":{"shell":CLEAN_SHELL}},"jobs":{"test":{"steps":[{"run":"cargo test --locked"}]}}});
        let contexts = BTreeMap::from([("example".into(), execution_context(&workflow).unwrap())]);
        (workflow, contexts)
    }
    #[test]
    fn closed_context_rejects_computed_hooks_and_command_success_masks() {
        let (workflow, contracts) = fixture();
        assert!(check_shell_policy(&workflow, &contracts).is_empty());
        for command in [
            "cargo test --locked || true",
            "# cargo test --locked",
            "if false; then cargo test --locked; fi",
            "key=GITHUB_; key+=PATH; echo /tmp/x >> ${!key}",
        ] {
            let mut mutated = workflow.clone();
            mutated["jobs"]["test"]["steps"][0]["run"] = json!(command);
            assert!(
                check_shell_policy(&mutated, &contracts)
                    .contains(&"unreviewed shell-selection execution context".into())
            );
        }
    }
    #[test]
    fn workflow_job_step_startup_and_shell_mutations_fail() {
        let (workflow, contracts) = fixture();
        for pointer in ["", "/jobs/test", "/jobs/test/steps/0"] {
            let mut mutated = workflow.clone();
            mutated.pointer_mut(pointer).unwrap()["env"] =
                json!({"PATH":"/tmp/mask","BASH_ENV":"hook"});
            let errors = check_shell_policy(&mutated, &contracts);
            assert!(errors.contains(&"PATH environment override".into()));
            assert!(errors.contains(&"startup environment injection".into()));
        }
        let mut mutated = workflow.clone();
        mutated["defaults"]["run"]["shell"] = json!("bash {0}");
        assert!(
            check_shell_policy(&mutated, &contracts)
                .contains(&"explicit clean strict shell required".into())
        );
        let mut mutated = workflow.clone();
        mutated["jobs"]["test"]["defaults"] = json!({});
        assert!(check_shell_policy(&mutated, &contracts).contains(&"job shell override".into()));
        let mut mutated = workflow;
        mutated["jobs"]["test"]["steps"][0]["shell"] = json!("bash");
        assert!(check_shell_policy(&mutated, &contracts).contains(&"step shell override".into()));
    }
    #[test]
    fn yaml_aliases_merges_and_duplicate_keys() {
        let workflow=parse("name: example\njobs:\n  test:\n    steps:\n    - run: echo\n      env: &mask {PATH: /tmp/mask}\n    - run: echo\n      env: {<<: *mask}\n").unwrap();
        assert_eq!(
            workflow["jobs"]["test"]["steps"][1]["env"]["PATH"],
            json!("/tmp/mask")
        );
        assert!(parse("name: first\nname: second\njobs: {}\n").is_err());
    }
    #[test]
    fn deepest_latest_checkout_owns_history() {
        let steps = vec![
            json!({"uses":"actions/checkout@pin","with":{"fetch-depth":0}}),
            json!({"uses":"actions/checkout@pin","with":{"path":"release-tools"}}),
            json!({"uses":"actions/checkout@pin","with":{"path":"release-tools","fetch-depth":"0"}}),
            json!({"name":"guard","run":"cargo test"}),
        ];
        assert!(!full_history(
            owning_checkout(&steps, 2, "release-tools/src/lib.rs")
                .unwrap()
                .1
        ));
        assert!(full_history(
            owning_checkout(&steps, 3, "release-tools/src/lib.rs")
                .unwrap()
                .1
        ));
        assert_eq!(owning_checkout(&steps, 3, "other/file").unwrap().0, ".");
        let workflow = json!({"jobs":{"test":{"steps":steps}}});
        let requirement = HistoryRequirement {
            job: "test".into(),
            step_name: "guard".into(),
            consumer_path: "release-tools/src/lib.rs".into(),
        };
        assert!(check_history_policy(&workflow, std::slice::from_ref(&requirement)).is_empty());
        let mut bad = workflow;
        bad["jobs"]["test"]["steps"][2]["if"] = json!("true");
        assert!(!check_history_policy(&bad, &[requirement]).is_empty());
    }
}
