// SPDX-License-Identifier: AGPL-3.0-only
//! Who is asking: the authentication seam and the roles it yields.
//!
//! The UI is read-only for everyone until an operator turns authentication
//! on. Two ways of knowing a person are built in, behind one interface:
//!
//! - `header`: an authenticating reverse proxy (oauth2-proxy, Authelia,
//!   Pomerium, Caddy `forward_auth`) forwards the identity in request
//!   headers; the headers are believed only from the proxy's own addresses.
//! - `users`: a local users file (name, role, Argon2id hash) with a login
//!   form and a signed session cookie, for a deployment without a proxy.
//!
//! Either way a request resolves to at most one [`Principal`] carrying one
//! [`Role`]; the roles are a ladder (a role holds every role below it), and
//! a handler asks a [`Session`] for the role it needs. A person's OKF actor
//! is `human:<subject>`, which is what the human workflow writes into the
//! documents it touches.

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// What a person may do, as a ladder: each role holds every role below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    /// Read everything the reader role can see (the default for everyone).
    Viewer,
    /// Upload documents into a content bundle.
    Uploader,
    /// Edit or delete documents in a content bundle.
    Editor,
    /// Approve documents (record a human verification) or send them back.
    Approver,
    /// Everything, reserved for operators.
    Admin,
}

impl Role {
    /// Every role, lowest first.
    pub(crate) const fn all() -> &'static [Role] {
        &[
            Role::Viewer,
            Role::Uploader,
            Role::Editor,
            Role::Approver,
            Role::Admin,
        ]
    }

    /// The identifier used in files, flags, and headers.
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Uploader => "uploader",
            Role::Editor => "editor",
            Role::Approver => "approver",
            Role::Admin => "admin",
        }
    }

    pub(crate) fn parse(id: &str) -> Option<Self> {
        let id = id.trim();
        Self::all().iter().copied().find(|r| r.id() == id)
    }

    /// Whether this role holds `required` (the ladder).
    pub(crate) fn allows(self, required: Role) -> bool {
        self >= required
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// An authenticated person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Principal {
    /// The stable identifier (a login name or an email), safe for an OKF
    /// actor string.
    pub subject: String,
    /// What the page shows.
    pub display: String,
    pub role: Role,
}

impl Principal {
    /// The OKF actor written into documents this person touches.
    pub(crate) fn actor(&self) -> String {
        format!("human:{}", self.subject)
    }
}

/// The longest subject accepted from a header or a users file.
const SUBJECT_MAX: usize = 128;

/// A subject is one token of plain characters: letters, digits, and the
/// punctuation found in logins and email addresses. Anything else (spaces,
/// control characters, YAML syntax) is refused so the actor string it
/// becomes stays a single, unambiguous identifier.
pub(crate) fn valid_subject(subject: &str) -> bool {
    !subject.is_empty()
        && subject.len() <= SUBJECT_MAX
        && subject
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '+'))
}

/// How identities reach the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Nobody is identified; everyone is a viewer.
    None,
    /// A trusted reverse proxy forwards the identity in headers.
    Header,
    /// A local users file with a login form and a session cookie.
    Users,
}

impl Mode {
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Mode::None => "none",
            Mode::Header => "header",
            Mode::Users => "users",
        }
    }
}

/// One `address/prefix` a trusted proxy may connect from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Parse `10.0.0.0/8`, `::1/128`, or a bare address (a `/32` or `/128`).
    pub(crate) fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let (addr, prefix) = match text.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (text, None),
        };
        let addr: IpAddr = addr
            .parse()
            .with_context(|| format!("{text:?} is not an IP address or CIDR range"))?;
        let bits = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => bits,
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= bits)
                .with_context(|| format!("{text:?} has an invalid prefix length"))?,
        };
        Ok(Self { addr, prefix })
    }

    pub(crate) fn contains(&self, ip: IpAddr) -> bool {
        let (a, b, bits) = match (self.addr, ip) {
            (IpAddr::V4(a), IpAddr::V4(b)) => {
                (u128::from(a.to_bits()), u128::from(b.to_bits()), 32)
            }
            (IpAddr::V6(a), IpAddr::V6(b)) => (a.to_bits(), b.to_bits(), 128),
            (IpAddr::V4(a), IpAddr::V6(b)) => match b.to_ipv4_mapped() {
                Some(b) => (u128::from(a.to_bits()), u128::from(b.to_bits()), 32),
                None => return false,
            },
            (IpAddr::V6(_), IpAddr::V4(_)) => return false,
        };
        let keep = u32::from(self.prefix);
        let mask = if keep == 0 {
            0
        } else {
            u128::MAX << (bits - keep)
        };
        (a & mask) == (b & mask)
    }
}

/// The `header` mode: which headers to read and whom to believe.
#[derive(Debug, Clone)]
pub(crate) struct HeaderAuth {
    pub user_header: HeaderName,
    pub name_header: Option<HeaderName>,
    pub groups_header: Option<HeaderName>,
    /// Group name to role, first match wins; a person in no mapped group
    /// gets `default_role`.
    pub role_map: Vec<(String, Role)>,
    pub default_role: Role,
    /// Peers whose headers are believed. Empty means nobody (fail closed).
    pub trusted: Vec<Cidr>,
    /// Believe every peer: only for a server that is reachable from the
    /// proxy alone (a private network), stated explicitly.
    pub trust_any_peer: bool,
}

impl HeaderAuth {
    /// Parse `group=role,group=role`.
    pub(crate) fn parse_role_map(text: &str) -> Result<Vec<(String, Role)>> {
        text.split(',')
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(|entry| {
                let (group, role) = entry
                    .split_once('=')
                    .with_context(|| format!("role map entry {entry:?} is not group=role"))?;
                let role = Role::parse(role)
                    .with_context(|| format!("role map entry {entry:?} names an unknown role"))?;
                Ok((group.trim().to_owned(), role))
            })
            .collect()
    }

    fn trusts(&self, peer: Option<IpAddr>) -> bool {
        if self.trust_any_peer {
            return true;
        }
        peer.is_some_and(|ip| self.trusted.iter().any(|c| c.contains(ip)))
    }

    /// The person the headers name, when the peer is trusted and the user
    /// header is a valid subject.
    fn identify(&self, headers: &HeaderMap, peer: Option<IpAddr>) -> Option<Principal> {
        if !self.trusts(peer) {
            return None;
        }
        let subject = header_text(headers, &self.user_header)?;
        if !valid_subject(&subject) {
            return None;
        }
        let display = self
            .name_header
            .as_ref()
            .and_then(|h| header_text(headers, h))
            .filter(|d| !d.is_empty() && d.chars().all(|c| !c.is_control()))
            .unwrap_or_else(|| subject.clone());
        let groups: Vec<String> = self
            .groups_header
            .as_ref()
            .and_then(|h| header_text(headers, h))
            .map(|g| g.split(',').map(|s| s.trim().to_owned()).collect())
            .unwrap_or_default();
        let role = self
            .role_map
            .iter()
            .filter(|(group, _)| groups.iter().any(|g| g == group))
            .map(|(_, role)| *role)
            .max()
            .unwrap_or(self.default_role);
        Some(Principal {
            subject,
            display,
            role,
        })
    }
}

fn header_text(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// One line of the users file.
#[derive(Debug, Clone)]
pub(crate) struct UserRecord {
    role: Role,
    hash: String,
}

/// The `users` mode: a users file, a session key, and the cookie rules.
/// The file is re-read when it changes on disk, so adding, removing, or
/// demoting a person takes effect on their next request.
#[derive(Debug)]
pub(crate) struct UsersAuth {
    path: Option<PathBuf>,
    loaded: RwLock<LoadedUsers>,
    secret: Vec<u8>,
    session_seconds: u64,
    cookie_secure: bool,
    /// Failed sign-ins per user name, for the throttle.
    failures: Mutex<HashMap<String, Failures>>,
}

/// Recent failed sign-ins for one name: after a few, each further attempt
/// waits out a cooldown that doubles, so guessing cannot run in parallel.
#[derive(Debug, Clone, Copy)]
struct Failures {
    count: u32,
    until: Instant,
}

/// Failures before the cooldown starts.
const FREE_FAILURES: u32 = 5;
/// The longest cooldown between attempts.
const MAX_COOLDOWN: Duration = Duration::from_mins(15);
/// Failures are forgotten after this long without one.
const FAILURE_MEMORY: Duration = Duration::from_hours(1);

/// A hash that is verified when the name is unknown, so an unknown name
/// costs the same time as a wrong password (no name enumeration by timing).
const DECOY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$Y6DPgaHqK98VOJvF2yIP2m1TPVK0Nk2K7jBCqZqTdxU";

/// The users file as last read, with the modification time it had.
#[derive(Debug, Clone)]
struct LoadedUsers {
    modified: Option<SystemTime>,
    users: HashMap<String, UserRecord>,
}

/// The session cookie's name.
pub(crate) const SESSION_COOKIE: &str = "pgokf_session";

/// What a session cookie carries: the subject, when it expires, and a
/// fingerprint of the password hash the session was opened with. The role
/// is looked up in the users file on every request, so removing or
/// demoting a user takes effect at once, and a changed password ends every
/// session opened before it.
#[derive(Debug, Serialize, Deserialize)]
struct SessionClaims {
    #[serde(rename = "s")]
    subject: String,
    #[serde(rename = "e")]
    expires: u64,
    #[serde(rename = "n")]
    nonce: String,
    #[serde(rename = "p", default)]
    password_fingerprint: String,
}

/// A short digest of a stored hash: enough to tell a session opened under
/// an old password from one under the current one, without carrying the
/// hash itself.
fn fingerprint(hash: &str) -> String {
    let digest = Sha256::digest(hash.as_bytes());
    URL_SAFE_NO_PAD.encode(&digest[..12])
}

impl UsersAuth {
    /// Load a users file: one `name:role:$argon2id$...` per line, `#`
    /// comments and blank lines ignored.
    pub(crate) fn load(
        path: &Path,
        secret: Vec<u8>,
        session_seconds: u64,
        cookie_secure: bool,
    ) -> Result<Self> {
        let (modified, users) = Self::read_file(path)?;
        if users.is_empty() {
            bail!("the users file {} names no user", path.display());
        }
        if secret.len() < 32 {
            bail!("the session secret must be at least 32 bytes");
        }
        Ok(Self {
            path: Some(path.to_path_buf()),
            loaded: RwLock::new(LoadedUsers { modified, users }),
            secret,
            session_seconds,
            cookie_secure,
            failures: Mutex::new(HashMap::new()),
        })
    }

    fn read_file(path: &Path) -> Result<(Option<SystemTime>, HashMap<String, UserRecord>)> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the users file {}", path.display()))?;
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        Ok((modified, Self::parse_users(&text)?))
    }

    /// Re-read the users file when its modification time moved. A file
    /// that no longer parses is reported and the last good one kept.
    fn refresh(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let unchanged = self
            .loaded
            .read()
            .is_ok_and(|loaded| loaded.modified == modified);
        if unchanged {
            return;
        }
        match Self::read_file(path) {
            Ok((modified, users)) if !users.is_empty() => {
                if let Ok(mut loaded) = self.loaded.write() {
                    *loaded = LoadedUsers { modified, users };
                }
            }
            Ok(_) => eprintln!("pgokf-web: the users file names no user; keeping the last one"),
            Err(error) => eprintln!("pgokf-web: the users file did not reload: {error:#}"),
        }
    }

    /// One user's record, from the file as last read.
    fn record(&self, name: &str) -> Option<UserRecord> {
        self.refresh();
        self.loaded.read().ok()?.users.get(name).cloned()
    }

    /// Every user with their role, sorted by name (for the admin page).
    pub(crate) fn list(&self) -> Vec<(String, Role)> {
        self.refresh();
        let mut users: Vec<(String, Role)> = self
            .loaded
            .read()
            .map(|loaded| {
                loaded
                    .users
                    .iter()
                    .map(|(n, r)| (n.clone(), r.role))
                    .collect()
            })
            .unwrap_or_default();
        users.sort();
        users
    }

    /// Change the users file through `edit` and write it back atomically
    /// (a temporary file in the same directory, then a rename), so a
    /// half-written file is never read. The loaded state follows.
    ///
    /// # Errors
    ///
    /// No file path (a test instance), `edit` refusing, or the write failing.
    pub(crate) fn mutate(
        &self,
        edit: impl FnOnce(&mut HashMap<String, UserRecord>) -> Result<()>,
    ) -> Result<()> {
        let path = self
            .path
            .as_ref()
            .context("this users store is not backed by a file")?;
        let mut guard = self
            .loaded
            .write()
            .map_err(|_| anyhow!("the users file lock is poisoned"))?;
        let mut users = guard.users.clone();
        edit(&mut users)?;
        let mut names: Vec<&String> = users.keys().collect();
        names.sort();
        let mut text = String::from("# pgokf-web users: name:role:argon2id-hash, one per line\n");
        for name in names {
            let record = &users[name];
            let _ = writeln!(text, "{name}:{}:{}", record.role.id(), record.hash);
        }
        let temp = path.with_extension("tmp");
        std::fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;
        std::fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        *guard = LoadedUsers { modified, users };
        Ok(())
    }

    /// Add a person (a new name) with a hashed password.
    pub(crate) fn add_user(&self, name: &str, role: Role, password: &str) -> Result<()> {
        let name = name.trim();
        if !valid_subject(name) {
            bail!("{name:?} is not a valid user name (letters, digits, . _ - @ +)");
        }
        let hash = validated_password(password).and_then(hash_password)?;
        self.mutate(|users| {
            if users.contains_key(name) {
                bail!("{name} already exists");
            }
            users.insert(name.to_owned(), UserRecord { role, hash });
            Ok(())
        })
    }

    pub(crate) fn set_role(&self, name: &str, role: Role) -> Result<()> {
        self.mutate(|users| {
            let record = users
                .get_mut(name)
                .with_context(|| format!("{name} is not a user"))?;
            record.role = role;
            Ok(())
        })
    }

    pub(crate) fn set_password(&self, name: &str, password: &str) -> Result<()> {
        let hash = validated_password(password).and_then(hash_password)?;
        self.mutate(|users| {
            let record = users
                .get_mut(name)
                .with_context(|| format!("{name} is not a user"))?;
            record.hash = hash;
            Ok(())
        })
    }

    pub(crate) fn remove_user(&self, name: &str) -> Result<()> {
        self.mutate(|users| {
            if users.remove(name).is_none() {
                bail!("{name} is not a user");
            }
            if users.is_empty() {
                bail!("the last user cannot be removed");
            }
            Ok(())
        })
    }

    fn parse_users(text: &str) -> Result<HashMap<String, UserRecord>> {
        let mut users = HashMap::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.splitn(3, ':');
            let (name, role, hash) = match (parts.next(), parts.next(), parts.next()) {
                (Some(n), Some(r), Some(h)) => (n.trim(), r.trim(), h.trim()),
                _ => bail!("users file line {}: expected name:role:hash", index + 1),
            };
            if !valid_subject(name) {
                bail!(
                    "users file line {}: {name:?} is not a valid user name",
                    index + 1
                );
            }
            let role = Role::parse(role)
                .with_context(|| format!("users file line {}: unknown role {role:?}", index + 1))?;
            PasswordHash::new(hash)
                .map_err(|e| anyhow!("users file line {}: bad password hash ({e})", index + 1))?;
            if users
                .insert(
                    name.to_owned(),
                    UserRecord {
                        role,
                        hash: hash.to_owned(),
                    },
                )
                .is_some()
            {
                bail!("users file line {}: {name:?} appears twice", index + 1);
            }
        }
        Ok(users)
    }

    /// The person a sign-in names, when the password verifies. A name under
    /// cooldown is refused without checking; an unknown name still costs a
    /// hash verification; a failure counts towards the cooldown.
    pub(crate) fn verify(&self, name: &str, password: &str) -> Option<Principal> {
        let name = name.trim();
        if self.throttled(name) {
            return None;
        }
        let record = self.record(name);
        let hash = record.as_ref().map_or(DECOY_HASH, |r| r.hash.as_str());
        let verified = PasswordHash::new(hash).is_ok_and(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        });
        match record {
            Some(record) if verified => {
                self.forget_failures(name);
                Some(Principal {
                    subject: name.to_owned(),
                    display: name.to_owned(),
                    role: record.role,
                })
            }
            _ => {
                self.count_failure(name);
                None
            }
        }
    }

    /// How long a name must wait before its next attempt is even checked.
    pub(crate) fn cooldown(&self, name: &str) -> Option<Duration> {
        let failures = self.failures.lock().ok()?;
        let entry = failures.get(name.trim())?;
        entry.until.checked_duration_since(Instant::now())
    }

    fn throttled(&self, name: &str) -> bool {
        self.cooldown(name).is_some()
    }

    fn count_failure(&self, name: &str) {
        let Ok(mut failures) = self.failures.lock() else {
            return;
        };
        let now = Instant::now();
        failures.retain(|_, f| now.duration_since(f.until) < FAILURE_MEMORY);
        let entry = failures.entry(name.to_owned()).or_insert(Failures {
            count: 0,
            until: now,
        });
        entry.count += 1;
        if entry.count > FREE_FAILURES {
            let doublings = (entry.count - FREE_FAILURES).min(10);
            let wait = Duration::from_secs(2_u64.pow(doublings)).min(MAX_COOLDOWN);
            entry.until = now + wait;
        }
    }

    fn forget_failures(&self, name: &str) {
        if let Ok(mut failures) = self.failures.lock() {
            failures.remove(name);
        }
    }

    /// A `Set-Cookie` value that signs the person in.
    pub(crate) fn issue_cookie(&self, principal: &Principal) -> Result<String> {
        let claims = SessionClaims {
            subject: principal.subject.clone(),
            expires: now_unix() + self.session_seconds,
            nonce: URL_SAFE_NO_PAD.encode(random_bytes(12)?),
            password_fingerprint: self
                .record(&principal.subject)
                .map(|r| fingerprint(&r.hash))
                .unwrap_or_default(),
        };
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
        let tag = URL_SAFE_NO_PAD.encode(self.sign(payload.as_bytes()));
        Ok(format!(
            "{SESSION_COOKIE}={payload}.{tag}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}",
            self.session_seconds,
            if self.cookie_secure { "; Secure" } else { "" }
        ))
    }

    /// A `Set-Cookie` value that signs the person out.
    pub(crate) fn clear_cookie(&self) -> String {
        format!(
            "{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}",
            if self.cookie_secure { "; Secure" } else { "" }
        )
    }

    /// The person a request's cookie names: a valid signature, not expired,
    /// and still in the users file (whose role applies).
    fn identify(&self, headers: &HeaderMap) -> Option<Principal> {
        let token = cookie_value(headers, SESSION_COOKIE)?;
        let (payload, tag) = token.split_once('.')?;
        let tag = URL_SAFE_NO_PAD.decode(tag).ok()?;
        self.verify_tag(payload.as_bytes(), &tag).ok()?;
        let claims: SessionClaims =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
        if claims.expires <= now_unix() {
            return None;
        }
        let record = self.record(&claims.subject)?;
        if claims.password_fingerprint != fingerprint(&record.hash) {
            // The password changed since this session was opened.
            return None;
        }
        Some(Principal {
            subject: claims.subject.clone(),
            display: claims.subject,
            role: record.role,
        })
    }

    fn sign(&self, payload: &[u8]) -> Vec<u8> {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.secret).expect("HMAC accepts any key length");
        mac.update(payload);
        mac.finalize().into_bytes().to_vec()
    }

    fn verify_tag(&self, payload: &[u8], tag: &[u8]) -> Result<()> {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.secret).expect("HMAC accepts any key length");
        mac.update(payload);
        mac.verify_slice(tag)
            .map_err(|_| anyhow!("session signature mismatch"))
    }
}

/// The shortest password the UI accepts when one is set through it.
const PASSWORD_MIN: usize = 12;

/// A password long enough to be worth hashing.
fn validated_password(password: &str) -> Result<&str> {
    if password.chars().count() < PASSWORD_MIN {
        bail!("a password needs at least {PASSWORD_MIN} characters");
    }
    Ok(password)
}

/// Hash a password for the users file (Argon2id, default parameters, a
/// fresh 16-byte salt).
pub(crate) fn hash_password(password: &str) -> Result<String> {
    let salt = random_bytes(16)?;
    Argon2::default()
        .hash_password_with_salt(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("hashing the password: {e}"))
}

/// Fresh random bytes from the operating system.
pub(crate) fn random_bytes(len: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0_u8; len];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("reading random bytes: {e}"))?;
    Ok(bytes)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// The value of one cookie in the request's `Cookie` headers.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.trim().to_owned())
}

/// The configured way of identifying people.
#[derive(Debug)]
pub(crate) enum Authenticator {
    Anonymous,
    Header(HeaderAuth),
    Users(UsersAuth),
}

impl Authenticator {
    pub(crate) fn mode(&self) -> Mode {
        match self {
            Authenticator::Anonymous => Mode::None,
            Authenticator::Header(_) => Mode::Header,
            Authenticator::Users(_) => Mode::Users,
        }
    }

    /// The person a request comes from, if any.
    pub(crate) fn identify(&self, headers: &HeaderMap, peer: Option<IpAddr>) -> Option<Principal> {
        match self {
            Authenticator::Anonymous => None,
            Authenticator::Header(h) => h.identify(headers, peer),
            Authenticator::Users(u) => u.identify(headers),
        }
    }

    pub(crate) fn users(&self) -> Option<&UsersAuth> {
        match self {
            Authenticator::Users(u) => Some(u),
            _ => None,
        }
    }
}

/// What a request resolved to: the person (if identified) and how people
/// are identified here, so a page can offer "sign in" when that exists.
#[derive(Debug, Clone)]
pub(crate) struct Session {
    pub principal: Option<Principal>,
    pub mode: Mode,
}

impl Session {
    pub(crate) fn anonymous(mode: Mode) -> Self {
        Self {
            principal: None,
            mode,
        }
    }

    /// The role this session holds (`Viewer` when nobody is identified).
    pub(crate) fn role(&self) -> Role {
        self.principal.as_ref().map_or(Role::Viewer, |p| p.role)
    }

    pub(crate) fn allows(&self, required: Role) -> bool {
        self.role().allows(required)
    }

    /// The peer address axum recorded for a request, when it serves with
    /// connection info.
    pub(crate) fn peer_of(extensions: &axum::http::Extensions) -> Option<IpAddr> {
        extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| info.0.ip())
    }
}

/// A value safe to put in a `Set-Cookie` header.
pub(crate) fn cookie_header(value: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_form_a_ladder_and_round_trip_their_ids() {
        // Arrange / Act / Assert
        assert!(Role::Admin.allows(Role::Viewer));
        assert!(Role::Editor.allows(Role::Uploader));
        assert!(!Role::Uploader.allows(Role::Editor));
        assert!(Role::Viewer.allows(Role::Viewer));
        for role in Role::all() {
            assert_eq!(Role::parse(role.id()), Some(*role));
        }
        assert_eq!(Role::parse("owner"), None);
        assert_eq!(Mode::Header.id(), "header");
    }

    #[test]
    fn subjects_are_single_plain_tokens() {
        // Arrange / Act / Assert
        assert!(valid_subject("alice"));
        assert!(valid_subject("alice.b@example.com"));
        assert!(!valid_subject(""));
        assert!(!valid_subject("alice smith"));
        assert!(!valid_subject("a:b"));
        assert!(!valid_subject(&"x".repeat(129)));
    }

    #[test]
    fn cidr_ranges_contain_their_addresses_and_nothing_else() {
        // Arrange
        let lan = Cidr::parse("10.100.0.0/16").expect("valid");
        let host = Cidr::parse("127.0.0.1").expect("valid");
        let six = Cidr::parse("fd00::/8").expect("valid");

        // Act / Assert
        assert!(lan.contains("10.100.0.14".parse().unwrap()));
        assert!(!lan.contains("10.101.0.14".parse().unwrap()));
        assert!(host.contains("127.0.0.1".parse().unwrap()));
        assert!(!host.contains("127.0.0.2".parse().unwrap()));
        assert!(host.contains("::ffff:127.0.0.1".parse().unwrap()));
        assert!(six.contains("fd12::1".parse().unwrap()));
        assert!(!six.contains("fe80::1".parse().unwrap()));
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("nope").is_err());
    }

    fn header_auth(trusted: &[&str]) -> HeaderAuth {
        HeaderAuth {
            user_header: HeaderName::from_static("x-forwarded-user"),
            name_header: Some(HeaderName::from_static("x-forwarded-preferred-username")),
            groups_header: Some(HeaderName::from_static("x-forwarded-groups")),
            role_map: HeaderAuth::parse_role_map("okf-editors=editor, okf-approvers=approver")
                .expect("valid"),
            default_role: Role::Viewer,
            trusted: trusted.iter().map(|c| Cidr::parse(c).unwrap()).collect(),
            trust_any_peer: false,
        }
    }

    #[test]
    fn header_identities_are_believed_only_from_trusted_peers() {
        // Arrange
        let auth = header_auth(&["10.0.0.0/8"]);
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-user",
            HeaderValue::from_static("alice@example.com"),
        );
        headers.insert(
            "x-forwarded-preferred-username",
            HeaderValue::from_static("Alice"),
        );
        headers.insert(
            "x-forwarded-groups",
            HeaderValue::from_static("okf-editors, okf-approvers, other"),
        );

        // Act
        let trusted = auth.identify(&headers, Some("10.1.2.3".parse().unwrap()));
        let untrusted = auth.identify(&headers, Some("192.168.1.9".parse().unwrap()));
        let unknown_peer = auth.identify(&headers, None);

        // Assert: the highest mapped role wins; strangers stay anonymous.
        let alice = trusted.expect("identified");
        assert_eq!(alice.subject, "alice@example.com");
        assert_eq!(alice.display, "Alice");
        assert_eq!(alice.role, Role::Approver);
        assert_eq!(alice.actor(), "human:alice@example.com");
        assert!(untrusted.is_none());
        assert!(unknown_peer.is_none());
    }

    #[test]
    fn header_identities_without_a_mapped_group_get_the_default_and_bad_subjects_nothing() {
        // Arrange
        let auth = header_auth(&["127.0.0.1"]);
        let peer = Some("127.0.0.1".parse().unwrap());
        let mut plain = HeaderMap::new();
        plain.insert("x-forwarded-user", HeaderValue::from_static("bob"));
        let mut bad = HeaderMap::new();
        bad.insert("x-forwarded-user", HeaderValue::from_static("bob smith"));

        // Act / Assert
        assert_eq!(
            auth.identify(&plain, peer).map(|p| p.role),
            Some(Role::Viewer)
        );
        assert!(auth.identify(&bad, peer).is_none());
        assert!(auth.identify(&HeaderMap::new(), peer).is_none());
        assert!(HeaderAuth::parse_role_map("okf-editors").is_err());
        assert!(HeaderAuth::parse_role_map("g=owner").is_err());
    }

    fn users_auth_with(secret: Vec<u8>) -> UsersAuth {
        let hash = hash_password("correct horse").expect("hashes");
        let text = format!("# people\nalice:editor:{hash}\n\nbob:viewer:{hash}\n");
        UsersAuth {
            path: None,
            loaded: RwLock::new(LoadedUsers {
                modified: None,
                users: UsersAuth::parse_users(&text).expect("parses"),
            }),
            secret,
            session_seconds: 3600,
            cookie_secure: false,
            failures: Mutex::new(HashMap::new()),
        }
    }

    fn users_auth() -> UsersAuth {
        users_auth_with(vec![7_u8; 32])
    }

    #[test]
    fn users_file_passwords_verify_and_sessions_round_trip_through_the_cookie() {
        // Arrange
        let auth = users_auth();

        // Act
        let alice = auth.verify("alice", "correct horse").expect("verifies");
        let wrong = auth.verify("alice", "wrong");
        let nobody = auth.verify("carol", "correct horse");
        let cookie = auth.issue_cookie(&alice).expect("issues");
        let mut headers = HeaderMap::new();
        let value = cookie.split(';').next().unwrap();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("theme=dark; {value}")).unwrap(),
        );
        let back = auth.identify(&headers).expect("identified");

        // Assert
        assert_eq!(alice.role, Role::Editor);
        assert!(wrong.is_none() && nobody.is_none());
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"));
        assert!(!cookie.contains("Secure"));
        assert_eq!(back.subject, "alice");
        assert_eq!(
            back.role,
            Role::Editor,
            "the role comes from the file, not the cookie"
        );
    }

    #[test]
    fn tampered_or_foreign_session_cookies_identify_nobody() {
        // Arrange
        let auth = users_auth();
        let alice = auth.verify("alice", "correct horse").expect("verifies");
        let cookie = auth.issue_cookie(&alice).expect("issues");
        let value = cookie.split(';').next().unwrap().to_owned();
        let (payload, tag) = value
            .trim_start_matches("pgokf_session=")
            .split_once('.')
            .unwrap();
        let other = users_auth_with(vec![9_u8; 32]);
        let forged = format!(
            "{SESSION_COOKIE}={payload}.{}",
            tag.chars().rev().collect::<String>()
        );
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(&forged).unwrap());
        let mut real = HeaderMap::new();
        real.insert(header::COOKIE, HeaderValue::from_str(&value).unwrap());

        // Act / Assert
        assert!(auth.identify(&headers).is_none(), "a bad signature");
        assert!(other.identify(&real).is_none(), "another server's secret");
        assert!(auth.identify(&HeaderMap::new()).is_none());
    }

    #[test]
    fn users_files_are_validated_line_by_line() {
        // Arrange
        let hash = hash_password("pw").expect("hashes");

        // Act / Assert
        assert!(
            UsersAuth::parse_users(&format!("alice:editor:{hash}\nalice:viewer:{hash}\n")).is_err()
        );
        assert!(UsersAuth::parse_users("alice:editor\n").is_err());
        assert!(UsersAuth::parse_users(&format!("alice:owner:{hash}\n")).is_err());
        assert!(UsersAuth::parse_users("alice:editor:not-a-hash\n").is_err());
        assert!(UsersAuth::parse_users(&format!("al ice:editor:{hash}\n")).is_err());
        assert_eq!(
            UsersAuth::parse_users("# nobody\n\n")
                .expect("parses")
                .len(),
            0
        );
    }

    #[test]
    fn a_changed_users_file_is_reloaded_on_the_next_lookup() {
        // Arrange: a file with alice, loaded; then rewritten without her.
        let dir = std::env::temp_dir().join(format!("pgokf-users-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("users");
        let hash = hash_password("pw").expect("hashes");
        std::fs::write(&path, format!("alice:approver:{hash}\n")).expect("write");
        let auth = UsersAuth::load(&path, vec![7_u8; 32], 3600, false).expect("loads");
        assert_eq!(
            auth.verify("alice", "pw").map(|p| p.role),
            Some(Role::Approver)
        );

        // Act: demote alice; make sure the mtime moves.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, format!("alice:viewer:{hash}\n")).expect("rewrite");
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(later))
            .expect("touch");

        // Assert
        assert_eq!(
            auth.verify("alice", "pw").map(|p| p.role),
            Some(Role::Viewer)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_users_file_is_managed_in_place() {
        // Arrange
        let dir = std::env::temp_dir().join(format!("pgokf-users-admin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("users");
        let hash = hash_password("pw").expect("hashes");
        std::fs::write(&path, format!("alice:admin:{hash}\n")).expect("write");
        let auth = UsersAuth::load(&path, vec![7_u8; 32], 3600, false).expect("loads");

        // Act
        auth.add_user("bob", Role::Uploader, "a long enough password")
            .expect("adds");
        let short = auth.add_user("carol", Role::Viewer, "short");
        let duplicate = auth.add_user("bob", Role::Viewer, "another long password");
        auth.set_role("bob", Role::Editor).expect("promotes");
        auth.set_password("bob", "a different long password")
            .expect("resets");
        auth.remove_user("alice").expect("removes");
        let last = auth.remove_user("bob");

        // Assert
        assert!(short.is_err() && duplicate.is_err());
        assert_eq!(auth.list(), vec![("bob".to_owned(), Role::Editor)]);
        assert!(auth.verify("bob", "a different long password").is_some());
        assert!(auth.verify("bob", "a long enough password").is_none());
        assert!(last.is_err(), "the last user stays");
        let text = std::fs::read_to_string(&path).expect("reads");
        assert!(text.contains("bob:editor:$argon2id$"), "{text}");
        assert!(!text.contains("alice"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repeated_failures_earn_a_cooldown_and_a_success_clears_it() {
        // Arrange
        let auth = users_auth();

        // Act: five free failures, then the sixth starts the cooldown.
        for _ in 0..FREE_FAILURES {
            assert!(auth.verify("alice", "wrong").is_none());
        }
        assert!(auth.cooldown("alice").is_none(), "still free");
        assert!(auth.verify("alice", "wrong").is_none());
        let waiting = auth.cooldown("alice");
        let refused_even_when_right = auth.verify("alice", "correct horse");

        // Assert
        assert!(waiting.is_some(), "a cooldown started");
        assert!(
            refused_even_when_right.is_none(),
            "not checked during the cooldown"
        );
        assert!(
            auth.verify("bob", "correct horse").is_some(),
            "other names are unaffected"
        );
        auth.forget_failures("alice");
        assert!(auth.verify("alice", "correct horse").is_some());
        assert!(
            auth.cooldown("alice").is_none(),
            "a success clears the record"
        );
        assert!(
            auth.verify("nobody", "x").is_none(),
            "an unknown name costs a verification too"
        );
    }

    #[test]
    fn a_changed_password_ends_the_sessions_opened_before_it() {
        // Arrange
        let dir = std::env::temp_dir().join(format!("pgokf-users-fp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("users");
        std::fs::write(
            &path,
            format!(
                "alice:editor:{}\n",
                hash_password("first password").unwrap()
            ),
        )
        .expect("write");
        let auth = UsersAuth::load(&path, vec![7_u8; 32], 3600, false).expect("loads");
        let alice = auth.verify("alice", "first password").expect("verifies");
        let cookie = auth.issue_cookie(&alice).expect("issues");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(cookie.split(';').next().unwrap()).unwrap(),
        );
        assert!(auth.identify(&headers).is_some());

        // Act
        auth.set_password("alice", "second password!")
            .expect("changes");

        // Assert
        assert!(auth.identify(&headers).is_none(), "the old session is over");
        let again = auth.verify("alice", "second password!").expect("verifies");
        let fresh = auth.issue_cookie(&again).expect("issues");
        let mut fresh_headers = HeaderMap::new();
        fresh_headers.insert(
            header::COOKIE,
            HeaderValue::from_str(fresh.split(';').next().unwrap()).unwrap(),
        );
        assert!(auth.identify(&fresh_headers).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cookie_values_are_read_from_any_cookie_header() {
        // Arrange
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, HeaderValue::from_static("a=1; b=2"));
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("pgokf_session=xyz"),
        );

        // Act / Assert
        assert_eq!(cookie_value(&headers, "b").as_deref(), Some("2"));
        assert_eq!(
            cookie_value(&headers, SESSION_COOKIE).as_deref(),
            Some("xyz")
        );
        assert_eq!(cookie_value(&headers, "zzz"), None);
    }
}
