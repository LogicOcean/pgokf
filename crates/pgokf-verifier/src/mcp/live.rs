//! Disposable `PostgreSQL` and authenticated HTTP/stdio drivers.
use crate::{process::CommandSpec, service::Service};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::TcpListener,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
pub const ROLES: [&str; 4] = ["reader", "builder", "writer", "admin"];
#[must_use]
pub fn token(role: &str) -> String {
    format!(
        "pgokf_{}",
        match role {
            "reader" => "A",
            "builder" => "B",
            "writer" => "C",
            "admin" => "D",
            "foreign" => "F",
            _ => "Z",
        }
        .repeat(43)
    )
}
#[must_use]
pub fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut s, b| {
            write!(s, "{b:02x}").expect("string formatting");
            s
        })
}
#[must_use]
#[allow(clippy::needless_pass_by_value)] // Callers construct owned JSON request values.
pub fn request(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}
#[must_use]
pub fn modern(method: &str, mut params: Value) -> Value {
    params["_meta"] = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}});
    request(1, method, params)
}
pub fn headers(message: &Value) -> BTreeMap<String, String> {
    let mut h = BTreeMap::from([
        (
            "Accept".into(),
            "application/json, text/event-stream".into(),
        ),
        ("MCP-Protocol-Version".into(), "2026-07-28".into()),
        (
            "Mcp-Method".into(),
            message["method"].as_str().unwrap_or_default().into(),
        ),
    ]);
    if ["tools/call", "resources/read"].contains(&message["method"].as_str().unwrap_or_default()) {
        h.insert(
            "Mcp-Name".into(),
            message["params"]
                .get("name")
                .or_else(|| message["params"].get("uri"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
        );
    }
    h
}
pub struct Cluster {
    pub bindir: PathBuf,
    pub work: tempfile::TempDir,
    service: Service,
}
impl Cluster {
    /// # Errors
    /// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
    pub fn start(root: &Path, pg_config: &Path, evidence: &Path) -> Result<Self, String> {
        let config = |flag: &str| {
            let mut c = CommandSpec::new(pg_config);
            c.args = vec![flag.into()];
            String::from_utf8(c.checked()?.stdout)
                .map(|s| s.trim().to_owned())
                .map_err(|e| e.to_string())
        };
        if !config("--version")?.starts_with("PostgreSQL 18.") {
            return Err("MCP fixtures require PostgreSQL 18".into());
        }
        let installed =
            std::fs::read(Path::new(&config("--sharedir")?).join("extension/pgokf.control"))
                .map_err(|e| e.to_string())?;
        if installed
            != std::fs::read(root.join("crates/extension/pgokf.control"))
                .map_err(|e| e.to_string())?
        {
            return Err("installed extension control differs".into());
        }
        let bindir = PathBuf::from(config("--bindir")?);
        let work = tempfile::Builder::new()
            .prefix("pgokf-mcp-")
            .tempdir_in(std::env::temp_dir())
            .map_err(|e| e.to_string())?;
        let data = work.path().join("data");
        let mut init = pg_command(bindir.join("initdb"));
        init.args = vec![
            "-D".into(),
            data.as_os_str().into(),
            "-U".into(),
            "postgres".into(),
            "--auth=trust".into(),
            "--no-locale".into(),
        ];
        init.env.insert("LC_ALL".into(), "C".into());
        init.checked()?;
        let mut spec = pg_command(bindir.join("postgres"));
        spec.args = vec![
            "-D".into(),
            data.as_os_str().into(),
            "-k".into(),
            work.path().as_os_str().into(),
            "-c".into(),
            "listen_addresses=".into(),
        ];
        spec.env.insert("LC_ALL".into(), "C".into());
        spec.timeout = Duration::from_hours(1);
        let service = Service::spawn(&spec, &evidence.join("postgres.json"))?;
        let mut cluster = Self {
            bindir,
            work,
            service,
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            cluster.service.check()?;
            if cluster.sql("postgres", "SELECT 1").is_ok() {
                break;
            }
            if Instant::now() >= deadline {
                return Err("postgres readiness timeout".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(cluster)
    }
    #[must_use]
    pub fn dsn(&self, db: &str, user: &str) -> String {
        format!(
            "host={} dbname={db} user={user}",
            self.work.path().display()
        )
    }
    /// # Errors
    /// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
    pub fn sql(&mut self, db: &str, sql: &str) -> Result<String, String> {
        self.service.check()?;
        let mut c = pg_command(self.bindir.join("psql"));
        c.args = vec![
            "-X".into(),
            "-qAt".into(),
            "-v".into(),
            "ON_ERROR_STOP=1".into(),
            "-h".into(),
            self.work.path().as_os_str().into(),
            "-U".into(),
            "postgres".into(),
            "-d".into(),
            db.into(),
        ];
        c.stdin = sql.as_bytes().to_vec();
        String::from_utf8(c.checked()?.stdout)
            .map(|s| s.trim().into())
            .map_err(|e| e.to_string())
    }
    /// # Errors
    /// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
    pub fn stop(&mut self) -> Result<(), String> {
        self.service.stop()
    }
}
impl Drop for Cluster {
    fn drop(&mut self) {
        let _ = self.service.stop();
    }
}
pub struct Endpoint {
    pub origin: String,
    service: Service,
}
pub struct Response {
    pub status: u16,
    pub value: Value,
    pub headers: BTreeMap<String, String>,
}
impl Endpoint {
    /// # Errors
    /// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
    pub fn start(
        binary: &Path,
        dsn: &str,
        writer: bool,
        tenant: Option<&str>,
        env: BTreeMap<std::ffi::OsString, std::ffi::OsString>,
        receipt: &Path,
    ) -> Result<Self, String> {
        Self::start_config(binary, dsn, writer, tenant, env, receipt, false)
    }
    /// # Errors
    /// Rejects failed startup of the explicitly same-origin official fixture.
    pub fn start_same_origin(binary: &Path, dsn: &str, receipt: &Path) -> Result<Self, String> {
        Self::start_config(
            binary,
            dsn,
            true,
            Some("contract-a"),
            BTreeMap::new(),
            receipt,
            true,
        )
    }
    fn start_config(
        binary: &Path,
        dsn: &str,
        writer: bool,
        tenant: Option<&str>,
        env: BTreeMap<std::ffi::OsString, std::ffi::OsString>,
        receipt: &Path,
        same_origin: bool,
    ) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        drop(listener);
        let mut spec = pg_command(binary);
        spec.args = vec!["--database-url".into(), dsn.into()];
        if writer {
            spec.args.extend(["--writer-url".into(), dsn.into()]);
        }
        if let Some(t) = tenant {
            spec.args.extend(["--tenant".into(), t.into()]);
        }
        spec.args
            .extend(["--http".into(), format!("127.0.0.1:{port}").into()]);
        if same_origin {
            spec.args.extend([
                "--allowed-origins".into(),
                format!("http://127.0.0.1:{port}").into(),
            ]);
        }
        spec.env.extend(env);
        spec.timeout = Duration::from_hours(1);
        spec.output_cap = 64 * 1024 * 1024;
        let service = Service::spawn(&spec, receipt)?;
        let mut endpoint = Self {
            origin: format!("http://127.0.0.1:{port}"),
            service,
        };
        let deadline = Instant::now() + Duration::from_mins(1);
        loop {
            endpoint.service.check()?;
            let mut c = CommandSpec::new("curl");
            c.args = [
                "--fail",
                "--silent",
                "--max-time",
                "1",
                &format!("{}/healthz", endpoint.origin),
            ]
            .map(Into::into)
            .into();
            if c.checked().is_ok() {
                break;
            }
            if Instant::now() >= deadline {
                return Err("HTTP readiness timeout".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(endpoint)
    }
    /// # Errors
    /// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
    pub fn call(
        &mut self,
        verb: &str,
        bearer: Option<&str>,
        body: Option<&str>,
        headers: &BTreeMap<String, String>,
    ) -> Result<Response, String> {
        self.service.check()?;
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let header_path = dir.path().join("headers");
        let mut c = CommandSpec::new("curl");
        c.args = vec![
            "--silent".into(),
            "--show-error".into(),
            "--max-time".into(),
            "60".into(),
            "--request".into(),
            verb.into(),
            "--header".into(),
            "Accept:".into(),
            "--dump-header".into(),
            header_path.as_os_str().into(),
            "--write-out".into(),
            "\n%{http_code}".into(),
        ];
        if let Some(token) = bearer {
            c.args.extend([
                "--header".into(),
                format!("Authorization: Bearer {token}").into(),
            ]);
        }
        for (key, value) in headers {
            c.args
                .extend(["--header".into(), format!("{key}: {value}").into()]);
        }
        if let Some(body) = body {
            c.args.extend([
                "--header".into(),
                "Content-Type: application/json".into(),
                "--data-binary".into(),
                "@-".into(),
            ]);
            c.stdin = body.as_bytes().to_vec();
        }
        c.args.push(format!("{}/mcp", self.origin).into());
        let output = c.checked()?;
        let raw = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
        let (body, status) = raw.rsplit_once('\n').ok_or("HTTP status missing")?;
        let value = if body.is_empty() {
            Value::Null
        } else {
            crate::json::from_slice(body.as_bytes()).unwrap_or_else(|_| json!(body))
        };
        let h = std::fs::read_to_string(header_path)
            .map_err(|e| e.to_string())?
            .lines()
            .filter_map(|line| line.split_once(':'))
            .map(|(k, v)| (k.to_lowercase(), v.trim().into()))
            .collect();
        Ok(Response {
            status: status.parse().map_err(|_| "invalid HTTP status")?,
            value,
            headers: h,
        })
    }
    /// # Errors
    /// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
    pub fn exchange(
        &mut self,
        message: &Value,
        role: &str,
        modern: bool,
    ) -> Result<Response, String> {
        let body = message
            .as_str()
            .map_or_else(|| message.to_string(), str::to_owned);
        let bearer = token(role);
        self.call(
            "POST",
            if role == "none" { None } else { Some(&bearer) },
            Some(&body),
            &if modern {
                headers(message)
            } else {
                BTreeMap::new()
            },
        )
    }
    /// # Errors
    /// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
    pub fn stop(&mut self) -> Result<(), String> {
        self.service.stop()
    }
}
/// # Errors
/// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
pub fn stdio(binary: &Path, dsn: &str, writer: bool, receipt: &Path) -> Result<Service, String> {
    let mut c = pg_command(binary);
    c.args = vec!["--database-url".into(), dsn.into()];
    if writer {
        c.args.extend(["--writer-url".into(), dsn.into()]);
    }
    c.timeout = Duration::from_mins(10);
    c.output_cap = 64 * 1024 * 1024;
    Service::spawn(&c, receipt)
}
/// # Errors
/// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
pub fn stdio_exchange(service: &mut Service, message: &Value) -> Result<Value, String> {
    let text = message
        .as_str()
        .map_or_else(|| message.to_string(), str::to_owned);
    service.write(format!("{text}\n").as_bytes())?;
    if message.is_object() && message.get("id").is_none() {
        return Ok(Value::Null);
    }
    crate::json::from_slice(&service.line(Duration::from_mins(1))?)
}

/// Create a fresh evidence directory and bind the actual artifacts before execution.
/// # Errors
/// Refuses reused evidence paths or unreadable candidate/runtime artifacts.
pub fn begin_evidence(
    root: &Path,
    pg_config: &Path,
    binary: &Path,
    evidence: &Path,
) -> Result<(), String> {
    std::fs::create_dir(evidence).map_err(|e| format!("new evidence directory: {e}"))?;
    let output = |program: &Path, args: &[&str], cwd: Option<&Path>| -> Result<String, String> {
        let mut c = CommandSpec::new(program);
        c.args = args.iter().map(Into::into).collect();
        c.cwd = cwd.map(Path::to_owned);
        String::from_utf8(c.checked()?.stdout)
            .map(|s| s.trim().to_owned())
            .map_err(|e| e.to_string())
    };
    let shared = PathBuf::from(output(pg_config, &["--sharedir"], None)?);
    let lib = PathBuf::from(output(pg_config, &["--pkglibdir"], None)?);
    let control = shared.join("extension/pgokf.control");
    let text = std::fs::read_to_string(&control).map_err(|e| e.to_string())?;
    let version = text
        .lines()
        .find_map(|l| {
            l.strip_prefix("default_version = '")
                .and_then(|v| v.strip_suffix('\''))
        })
        .ok_or("control default version")?;
    let library = [lib.join("pgokf.so"), lib.join("pgokf.dylib")]
        .into_iter()
        .find(|p| p.is_file())
        .ok_or("installed extension library")?;
    let mut artifacts = BTreeMap::new();
    for (name, path) in [
        ("extension_control", control),
        (
            "extension_sql",
            shared.join(format!("extension/pgokf--{version}.sql")),
        ),
        ("extension_library", library),
        ("mcp_binary", binary.to_owned()),
        ("wire_sources", root.join("spec/mcp-wire-sources.json")),
        ("coverage", root.join("spec/mcp-native-coverage.json")),
        (
            "applicability",
            root.join("spec/mcp-conformance-applicability.json"),
        ),
    ] {
        artifacts.insert(name,json!({"path":path.canonicalize().map_err(|e|e.to_string())?,"sha256":digest(&std::fs::read(&path).map_err(|e|e.to_string())?)}));
    }
    crate::owned_docker::publish(
        &evidence.join("artifacts.json"),
        &json!({"artifacts":artifacts,"head":output(Path::new("git"),&["rev-parse","HEAD"],Some(root))?,"tree":output(Path::new("git"),&["rev-parse","HEAD^{tree}"],Some(root))?,"status":output(Path::new("git"),&["status","--porcelain"],Some(root))?,"diff_sha256":digest(output(Path::new("git"),&["diff","HEAD","--binary"],Some(root))?.as_bytes())}),
    )
}

fn pg_command(program: impl Into<PathBuf>) -> CommandSpec {
    let mut c = CommandSpec::new(program);
    c.env_remove = std::env::vars_os()
        .filter(|(key, _)| key.to_string_lossy().starts_with("PG"))
        .map(|(key, _)| key)
        .collect();
    c.env.insert("LC_ALL".into(), "C".into());
    c
}
