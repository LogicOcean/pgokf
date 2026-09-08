// SPDX-License-Identifier: AGPL-3.0-only
//! Who is asking: the authentication seam and the roles it yields.
//!
//! The UI is read-only for everyone until an operator turns authentication
//! on. Three ways of knowing a person are built in, behind one interface:
//!
//! - `oidc`: the site is its own `OAuth` client, signing people in against an
//!   `OpenID` Connect provider (Entra ID, Okta, Keycloak, Auth0, Google, a
//!   `GitLab` instance) with the authorization code flow and PKCE; see
//!   [`crate::oidc`].
//! - `header`: an authenticating reverse proxy (oauth2-proxy, Authelia,
//!   Pomerium, Caddy `forward_auth`) forwards the identity in request
//!   headers; the headers are believed only from the proxy's own addresses.
//! - `users`: a local users file (name, role, Argon2id hash) with a login
//!   form and a signed session cookie, for a deployment without a proxy.
//!
//! The `oidc` and `users` modes both end in a session this site signs, so
//! [`Sessions`] owns that cookie and both hold one.
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
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::session_store::SessionStore;

use anyhow::{Context, Result, anyhow, bail};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::de::DeserializeOwned;
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
    /// An `OpenID` Connect provider this site signs people in against.
    Oidc,
}

impl Mode {
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Mode::None => "none",
            Mode::Header => "header",
            Mode::Users => "users",
            Mode::Oidc => "oidc",
        }
    }

    /// Whether this site holds the session itself, so it can end it and
    /// offer its own sign-in page.
    pub(crate) const fn is_local_session(self) -> bool {
        matches!(self, Mode::Users | Mode::Oidc)
    }
}

/// Group names mapped to roles, with the role of a person in no mapped
/// group. Shared by every mode that learns a person's groups from
/// somewhere else (a proxy's header, a provider's claim).
#[derive(Debug, Clone)]
pub(crate) struct RoleMapping {
    map: Vec<(String, Role)>,
    default_role: Role,
}

impl RoleMapping {
    /// Parse `group=role,group=role`.
    ///
    /// # Errors
    ///
    /// An entry that is not `group=role`, or one naming an unknown role.
    pub(crate) fn parse(text: &str, default_role: Role) -> Result<Self> {
        let map = text
            .split(',')
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
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { map, default_role })
    }

    /// The groups that appear in the map, in the order they were given:
    /// the only ones whose membership changes a role, so a session need
    /// carry no others whatever the provider sends.
    pub(crate) fn known_groups(&self, groups: &[String]) -> Vec<String> {
        groups
            .iter()
            .filter(|group| self.map.iter().any(|(mapped, _)| mapped == *group))
            .cloned()
            .collect()
    }

    /// The highest role any of `groups` maps to, or the default. Taking the
    /// highest (rather than the first) keeps the result independent of the
    /// order the groups arrive in.
    pub(crate) fn role_for(&self, groups: &[String]) -> Role {
        self.map
            .iter()
            .filter(|(group, _)| groups.iter().any(|g| g == group))
            .map(|(_, role)| *role)
            .max()
            .unwrap_or(self.default_role)
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

/// The reverse proxies whose `X-Forwarded-For` this server believes, so the
/// sign-in throttle can key on the real client rather than the proxy. Empty
/// (the default) trusts no proxy: the throttle keys on the TCP peer, which is
/// right for a direct connection and fails safe behind an unconfigured proxy.
#[derive(Debug, Clone, Default)]
pub(crate) struct TrustedProxies {
    cidrs: Vec<Cidr>,
    any: bool,
}

impl TrustedProxies {
    /// Parse a comma-separated CIDR list, or the word `any` to trust every
    /// peer (only for a server reachable from the proxy alone).
    pub(crate) fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(Self::default());
        }
        if text.eq_ignore_ascii_case("any") {
            return Ok(Self {
                cidrs: Vec::new(),
                any: true,
            });
        }
        let cidrs = text
            .split(',')
            .filter(|c| !c.trim().is_empty())
            .map(Cidr::parse)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { cidrs, any: false })
    }

    fn trusts(&self, ip: IpAddr) -> bool {
        self.any || self.cidrs.iter().any(|c| c.contains(ip))
    }

    /// The client address to throttle on. When no proxy is trusted, or the
    /// TCP `peer` is not one of them, that is the peer itself. When the peer
    /// is a trusted proxy, it is the rightmost `X-Forwarded-For` address that
    /// is not itself trusted - the real client as the outermost trusted proxy
    /// saw it. A prefix an attacker spoofs sits to the left of the address the
    /// proxy appended, so it is never chosen.
    pub(crate) fn client_ip(&self, headers: &HeaderMap, peer: Option<IpAddr>) -> Option<IpAddr> {
        let peer = peer?;
        if !self.trusts(peer) {
            return Some(peer);
        }
        let forwarded: Vec<IpAddr> = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .filter_map(|hop| hop.trim().parse::<IpAddr>().ok())
            .collect();
        forwarded
            .iter()
            .rev()
            .find(|ip| !self.trusts(**ip))
            .or_else(|| forwarded.first())
            .copied()
            .or(Some(peer))
    }
}

/// The `header` mode: which headers to read and whom to believe.
#[derive(Debug, Clone)]
pub(crate) struct HeaderAuth {
    pub user_header: HeaderName,
    pub name_header: Option<HeaderName>,
    pub groups_header: Option<HeaderName>,
    /// Group names to roles, with the role of a person in no mapped group.
    pub roles: RoleMapping,
    /// Peers whose headers are believed. Empty means nobody (fail closed).
    pub trusted: Vec<Cidr>,
    /// Believe every peer: only for a server that is reachable from the
    /// proxy alone (a private network), stated explicitly.
    pub trust_any_peer: bool,
}

impl HeaderAuth {
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
        let role = self.roles.role_for(&groups);
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
    sessions: Arc<Sessions>,
    /// Failed sign-ins per (user name, client address), for the throttle.
    /// Keyed on both, because keyed on the name alone anyone who knew a
    /// name could hold that account under cooldown indefinitely - and lock
    /// its owner out of changing their own password, which shares the
    /// counter. The address is the *client* address: behind a reverse proxy
    /// the caller resolves it from a trusted `X-Forwarded-For` hop, so the
    /// key is the real client and not the one proxy every request arrives
    /// from (see [`TrustedProxies::client_ip`]).
    failures: Mutex<HashMap<(String, Option<IpAddr>), Failures>>,
    /// How many password verifications may run at once. Argon2id is meant
    /// to be expensive, which cuts both ways on a public sign-in page.
    verifying: tokio::sync::Semaphore,
}

/// Recent failed sign-ins for one name: after a few, each further attempt
/// waits out a cooldown that doubles, so guessing cannot run in parallel.
#[derive(Debug, Clone, Copy)]
struct Failures {
    count: u32,
    until: Instant,
}

/// Password verifications allowed to run at once. Each is ~19 MiB and
/// ~50 ms of one core, so this bounds what an unauthenticated flood of
/// sign-in attempts can take from the rest of the server.
const VERIFY_AT_ONCE: usize = 4;
/// Failures before the cooldown starts.
const FREE_FAILURES: u32 = 5;
/// The longest cooldown between attempts.
const MAX_COOLDOWN: Duration = Duration::from_mins(15);
/// Failures are forgotten after this long without one.
const FAILURE_MEMORY: Duration = Duration::from_hours(1);
/// The longest a name may be before it is refused unheard. A users-file
/// name is a short handle; a longer one is never valid, and counting it
/// would let an unauthenticated flood grow the throttle map without bound.
const MAX_NAME_LEN: usize = 256;
/// A ceiling on distinct throttle entries, so even a flood of differently
/// named attempts cannot grow the map past a bounded size.
const MAX_TRACKED_FAILURES: usize = 10_000;

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

/// The cookie holding one sign-in attempt while the person is away at the
/// provider (`oidc` mode).
pub(crate) const FLOW_COOKIE: &str = "pgokf_signin";

/// The session this site signs and the rules for its cookie. Both modes
/// that end in a local session (`users`, `oidc`) hold one, so a session
/// looks the same however it was opened. `Debug` is written by hand so the
/// signing key cannot reach a log line through it.
pub(crate) struct Sessions {
    secret: Vec<u8>,
    seconds: u64,
    cookie_secure: bool,
    /// The server's memory of which sessions are live, so one can be
    /// ended rather than left to expire. Attached whenever a mode that
    /// issues sessions is on; a signed cookie whose session is not here is
    /// refused.
    store: Option<SessionStore>,
}

impl fmt::Debug for Sessions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sessions")
            .field("secret", &"<redacted>")
            .field("seconds", &self.seconds)
            .field("cookie_secure", &self.cookie_secure)
            .field("store", &self.store)
            .finish()
    }
}

impl Sessions {
    /// A session signer. The secret must be long enough to be a key.
    ///
    /// # Errors
    ///
    /// A secret shorter than 32 bytes.
    pub(crate) fn new(secret: Vec<u8>, seconds: u64, cookie_secure: bool) -> Result<Self> {
        if secret.len() < 32 {
            bail!("the session secret must be at least 32 bytes");
        }
        Ok(Self {
            secret,
            seconds,
            cookie_secure,
            store: None,
        })
    }

    /// Remember issued sessions in `store`, so they can be ended.
    #[must_use]
    pub(crate) fn with_store(mut self, store: SessionStore) -> Self {
        self.store = Some(store);
        self
    }

    /// Whether sessions can be ended before they expire.
    pub(crate) fn revocable(&self) -> bool {
        self.store.is_some()
    }

    /// A `Set-Cookie` value that ends the session.
    pub(crate) fn clear_session(&self) -> String {
        self.clear(SESSION_COOKIE)
    }

    /// A `Set-Cookie` value that drops a sign-in in progress.
    pub(crate) fn clear_flow(&self) -> String {
        self.clear(FLOW_COOKIE)
    }

    /// A value this site can hand out and recognize again: the JSON of
    /// `value`, base64, with an HMAC tag over it.
    ///
    /// # Errors
    ///
    /// A value that does not serialize.
    pub(crate) fn seal<T: Serialize>(&self, value: &T) -> Result<String> {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(value)?);
        let tag = URL_SAFE_NO_PAD.encode(self.sign(payload.as_bytes()));
        Ok(format!("{payload}.{tag}"))
    }

    /// The value behind a sealed token, when the tag is this site's own.
    /// Verification happens before the payload is parsed, so a forged
    /// payload is never deserialized.
    pub(crate) fn open<T: DeserializeOwned>(&self, token: &str) -> Option<T> {
        let (payload, tag) = token.split_once('.')?;
        let tag = URL_SAFE_NO_PAD.decode(tag).ok()?;
        self.verify_tag(payload.as_bytes(), &tag).ok()?;
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
    }

    /// A `Set-Cookie` value for one of this site's cookies.
    pub(crate) fn cookie(&self, name: &str, value: &str, max_age: u64) -> String {
        format!(
            "{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{}",
            if self.cookie_secure { "; Secure" } else { "" }
        )
    }

    /// A `Set-Cookie` value that removes one of this site's cookies.
    pub(crate) fn clear(&self, name: &str) -> String {
        self.cookie(name, "", 0)
    }

    /// Open the session cookie of a request, when it is this site's, was
    /// opened by `mode`, and has not expired.
    pub(crate) fn read_session(&self, headers: &HeaderMap, mode: Mode) -> Option<SessionClaims> {
        let claims: SessionClaims = self.open(&cookie_value(headers, SESSION_COOKIE)?)?;
        let now = now_unix();
        if claims.expires <= now || claims.mode != mode.id() {
            return None;
        }
        // A cookie that verifies but whose session has been ended - signed
        // out, revoked, or opened before a password change - is refused,
        // whichever browser presents it.
        if let Some(store) = &self.store
            && !store.is_live(&claims.nonce, &claims.subject, now)
        {
            return None;
        }
        Some(claims)
    }

    /// End the session a request presents, so no copy of its cookie works
    /// again. A request with no live session of `mode` ends nothing.
    ///
    /// # Errors
    ///
    /// The session store cannot be rewritten.
    pub(crate) fn end_session_from(&self, headers: &HeaderMap, mode: Mode) -> Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        if let Some(claims) = self.read_session(headers, mode) {
            store.remove(&claims.nonce, now_unix())?;
        }
        Ok(())
    }

    /// End every session of `subject`, on every device.
    ///
    /// # Errors
    ///
    /// The session store cannot be rewritten.
    pub(crate) fn end_all_sessions_of(&self, subject: &str) -> Result<()> {
        match &self.store {
            Some(store) => store.remove_all_for(subject, now_unix()),
            None => Ok(()),
        }
    }

    /// How many live sessions `subject` holds, when that is known.
    pub(crate) fn session_count_for(&self, subject: &str) -> Option<usize> {
        self.store
            .as_ref()
            .map(|store| store.count_for(subject, now_unix()))
    }

    /// Everyone holding a live session, with how many, for the admin page.
    pub(crate) fn live_subjects(&self) -> Vec<(String, usize)> {
        self.store
            .as_ref()
            .map_or_else(Vec::new, |store| store.subjects(now_unix()))
    }

    /// A session cookie for `subject`, bound to `binding` (what the mode
    /// checks again on every request: a password fingerprint, or nothing).
    ///
    /// # Errors
    ///
    /// The system random source failing.
    pub(crate) fn open_session(
        &self,
        subject: &str,
        mode: Mode,
        binding: String,
    ) -> Result<String> {
        self.open_session_with(subject, mode, binding, None, Vec::new())
    }

    /// A session cookie carrying what a provider told this site about the
    /// person: what to call them, and the groups their role comes from.
    ///
    /// # Errors
    ///
    /// The system random source failing.
    pub(crate) fn open_session_with(
        &self,
        subject: &str,
        mode: Mode,
        binding: String,
        display: Option<String>,
        groups: Vec<String>,
    ) -> Result<String> {
        let now = now_unix();
        let claims = SessionClaims {
            subject: subject.to_owned(),
            expires: now + self.seconds,
            nonce: URL_SAFE_NO_PAD.encode(random_bytes(12)?),
            binding,
            display,
            groups,
            mode: mode.id().to_owned(),
        };
        // Recorded before the cookie is handed out: a session the store
        // does not know is refused, so an unrecorded one must never exist.
        if let Some(store) = &self.store {
            store.add(&claims.nonce, subject, claims.expires, now)?;
        }
        Ok(self.cookie(SESSION_COOKIE, &self.seal(&claims)?, self.seconds))
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

/// What a session cookie carries: the subject, when it expires, a nonce so
/// two sessions of one person differ, and a binding the mode checks again
/// on every request. In `users` mode the binding is a fingerprint of the
/// password hash, so a changed password ends every session opened before
/// it; in `oidc` mode the provider governs and the binding is empty. The
/// role is never carried: it is looked up on every request, so removing or
/// demoting a person takes effect at once.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SessionClaims {
    #[serde(rename = "s")]
    pub subject: String,
    #[serde(rename = "e")]
    pub expires: u64,
    #[serde(rename = "n")]
    nonce: String,
    #[serde(rename = "p", default, skip_serializing_if = "String::is_empty")]
    pub binding: String,
    /// What to call the person (`oidc`); their subject otherwise.
    #[serde(rename = "d", default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// The groups that decide their role (`oidc`), so the role is derived
    /// again on every request rather than carried.
    #[serde(rename = "g", default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// The mode that opened this session. A server reconfigured from one
    /// mode to another keeps its signing key, and a session opened under
    /// the old mode must not be honoured by the new one.
    #[serde(rename = "m", default)]
    mode: String,
}

/// A short digest of a stored hash: enough to tell a session opened under
/// an old password from one under the current one, without carrying the
/// hash itself.
fn fingerprint(hash: &str) -> String {
    let digest = Sha256::digest(hash.as_bytes());
    URL_SAFE_NO_PAD.encode(&digest[..12])
}

/// Write a private file whole: to a temporary beside it, mode `0600`,
/// synced, then renamed into place, so a reader sees the old file or the
/// new one and never a half-written one. The users file and the session
/// store are both written this way.
///
/// # Errors
///
/// The temporary cannot be written or renamed.
pub(crate) fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write as _;

    // The temporary is `<name>.tmp` beside the target - appended, so `a.db`
    // and `a` never share one - and created fresh: a link planted at that
    // name is refused rather than written through, and a temporary left by
    // a crash is cleared first so it cannot block every later write.
    let mut temp_name = path
        .file_name()
        .context("a private file needs a file name")?
        .to_os_string();
    temp_name.push(".tmp");
    let temp = path.with_file_name(temp_name);
    let _ = std::fs::remove_file(&temp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&temp)
        .with_context(|| format!("writing {}", temp.display()))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", temp.display()))?;
    drop(file);
    std::fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

impl UsersAuth {
    /// Load a users file: one `name:role:$argon2id$...` per line, `#`
    /// comments and blank lines ignored.
    pub(crate) fn load(path: &Path, sessions: Arc<Sessions>) -> Result<Self> {
        let (modified, users) = Self::read_file(path)?;
        if users.is_empty() {
            bail!("the users file {} names no user", path.display());
        }
        Ok(Self {
            path: Some(path.to_path_buf()),
            loaded: RwLock::new(LoadedUsers { modified, users }),
            sessions,
            failures: Mutex::new(HashMap::new()),
            verifying: tokio::sync::Semaphore::new(VERIFY_AT_ONCE),
        })
    }

    /// The file's contents and the modification time they belong to. The
    /// stamp is taken *before* the read, so a text older than its stamp is
    /// impossible and a reload can never install a stale set under a fresh
    /// stamp (and then stop noticing changes).
    fn read_file(path: &Path) -> Result<(Option<SystemTime>, HashMap<String, UserRecord>)> {
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the users file {}", path.display()))?;
        Ok((modified, Self::parse_users(&text)?))
    }

    /// Re-read the users file when its modification time moved. The reload
    /// is installed only if nothing else moved the set meanwhile
    /// (compare-and-install), so it never puts an older file back over a
    /// change made in this process. A file that no longer parses is
    /// reported and the last good one kept.
    fn refresh(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let on_disk = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let Ok(seen) = self.loaded.read().map(|loaded| loaded.modified) else {
            return;
        };
        if seen == on_disk {
            return;
        }
        match Self::read_file(path) {
            Ok((modified, users)) if !users.is_empty() => {
                if let Ok(mut loaded) = self.loaded.write()
                    && loaded.modified == seen
                {
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
        write_private(path, &text)?;
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        *guard = LoadedUsers { modified, users };
        Ok(())
    }

    /// Replace the users file, atomically and privately.
    ///
    /// The file holds every Argon2id hash, so the replacement is created
    /// readable only by its owner - `fs::write` would create it `0644` and
    /// the rename would then hand the operator's `chmod 600` away on the
    /// first edit through the Admin page - and is flushed to disk before it
    /// takes the name, so a crash leaves either the old file or the new one.
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

    /// Change a password. Every session the person holds is ended: the
    /// binding already stops them verifying, and the store forgets them too,
    /// so nothing lingers on disk. The caller re-issues the changer's own
    /// cookie so they stay signed in.
    pub(crate) fn set_password(&self, name: &str, password: &str) -> Result<()> {
        let hash = validated_password(password).and_then(hash_password)?;
        self.mutate(|users| {
            let record = users
                .get_mut(name)
                .with_context(|| format!("{name} is not a user"))?;
            record.hash = hash;
            Ok(())
        })?;
        // The new hash is already on disk, and the changed binding alone
        // stops every earlier session verifying; this only tidies the store.
        self.sessions.end_all_sessions_of(name).with_context(|| {
            format!("the password of {name} was changed, but its earlier sessions could not be cleared from the session store")
        })
    }

    /// Remove a person, and end every session they hold.
    pub(crate) fn remove_user(&self, name: &str) -> Result<()> {
        self.mutate(|users| {
            if users.remove(name).is_none() {
                bail!("{name} is not a user");
            }
            if users.is_empty() {
                bail!("the last user cannot be removed");
            }
            Ok(())
        })?;
        // The person is already gone from the file, which alone refuses
        // their sessions; this only tidies the store.
        self.sessions.end_all_sessions_of(name).with_context(|| {
            format!(
                "{name} was removed, but their sessions could not be cleared from the session store"
            )
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

    /// The person a sign-in names, when the password verifies.
    ///
    /// A name under cooldown is refused without checking; an unknown name
    /// still costs a hash verification, so a name cannot be probed by
    /// timing; a failure counts towards the cooldown.
    ///
    /// Verification is Argon2id at 19 MiB and ~50 ms, deliberately, and
    /// this runs on a tokio worker thread. [`UsersAuth::permit`] bounds how
    /// many run at once: without it, one unauthenticated client opening
    /// enough connections turned "expensive to guess" into "expensive to
    /// serve", allocating gigabytes and stalling every other request.
    pub(crate) fn verify(
        &self,
        name: &str,
        password: &str,
        peer: Option<IpAddr>,
    ) -> Option<Principal> {
        let name = name.trim();
        if name.len() > MAX_NAME_LEN {
            // Never a valid user name; refuse without touching the throttle
            // map, so a flood of long names cannot grow it.
            return None;
        }
        if self.throttled(name, peer) {
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
                self.forget_failures(name, peer);
                Some(Principal {
                    subject: name.to_owned(),
                    display: name.to_owned(),
                    role: record.role,
                })
            }
            _ => {
                self.count_failure(name, peer);
                None
            }
        }
    }

    /// A permit to run one password verification, waiting for a turn when
    /// [`VERIFY_AT_ONCE`] are already running.
    pub(crate) async fn permit(&self) -> tokio::sync::SemaphorePermit<'_> {
        self.verifying
            .acquire()
            .await
            .expect("the verification semaphore is never closed")
    }

    /// How long a name must wait before its next attempt is even checked.
    pub(crate) fn cooldown(&self, name: &str, peer: Option<IpAddr>) -> Option<Duration> {
        let failures = self.failures.lock().ok()?;
        let entry = failures.get(&(name.trim().to_owned(), peer))?;
        entry.until.checked_duration_since(Instant::now())
    }

    fn throttled(&self, name: &str, peer: Option<IpAddr>) -> bool {
        self.cooldown(name, peer).is_some()
    }

    fn count_failure(&self, name: &str, peer: Option<IpAddr>) {
        let Ok(mut failures) = self.failures.lock() else {
            return;
        };
        if name.len() > MAX_NAME_LEN {
            return;
        }
        let now = Instant::now();
        failures.retain(|_, f| now.duration_since(f.until) < FAILURE_MEMORY);
        let key = (name.to_owned(), peer);
        if !failures.contains_key(&key) && failures.len() >= MAX_TRACKED_FAILURES {
            // The map is full of live entries; do not let a new name grow it
            // further. The flood is already being cooled by those it displaced.
            return;
        }
        let entry = failures.entry(key).or_insert(Failures {
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

    fn forget_failures(&self, name: &str, peer: Option<IpAddr>) {
        if let Ok(mut failures) = self.failures.lock() {
            failures.remove(&(name.to_owned(), peer));
        }
    }

    /// A `Set-Cookie` value that signs the person in.
    pub(crate) fn issue_cookie(&self, principal: &Principal) -> Result<String> {
        let binding = self
            .record(&principal.subject)
            .map(|r| fingerprint(&r.hash))
            .unwrap_or_default();
        self.sessions
            .open_session(&principal.subject, Mode::Users, binding)
    }

    /// The person a request's cookie names: a session this site signed,
    /// not expired, still in the users file (whose role applies), and
    /// opened under the password the file holds now.
    fn identify(&self, headers: &HeaderMap) -> Option<Principal> {
        let claims = self.sessions.read_session(headers, Mode::Users)?;
        let record = self.record(&claims.subject)?;
        if claims.binding != fingerprint(&record.hash) {
            // The password changed since this session was opened.
            return None;
        }
        Some(Principal {
            subject: claims.subject.clone(),
            display: claims.subject,
            role: record.role,
        })
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

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// The value of one cookie in the request's `Cookie` headers.
pub(crate) fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
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
    // Boxed: the provider mode carries an HTTP client and cached metadata,
    // several times the size of the others.
    Oidc(Box<crate::oidc::OidcAuth>),
}

impl Authenticator {
    pub(crate) fn mode(&self) -> Mode {
        match self {
            Authenticator::Anonymous => Mode::None,
            Authenticator::Header(_) => Mode::Header,
            Authenticator::Users(_) => Mode::Users,
            Authenticator::Oidc(_) => Mode::Oidc,
        }
    }

    /// The person a request comes from, if any.
    pub(crate) fn identify(&self, headers: &HeaderMap, peer: Option<IpAddr>) -> Option<Principal> {
        match self {
            Authenticator::Anonymous => None,
            Authenticator::Header(h) => h.identify(headers, peer),
            Authenticator::Users(u) => u.identify(headers),
            Authenticator::Oidc(o) => o.identify(headers),
        }
    }

    pub(crate) fn users(&self) -> Option<&UsersAuth> {
        match self {
            Authenticator::Users(u) => Some(u),
            _ => None,
        }
    }

    pub(crate) fn oidc(&self) -> Option<&crate::oidc::OidcAuth> {
        match self {
            Authenticator::Oidc(o) => Some(o),
            _ => None,
        }
    }

    /// The session signer, for the modes that hold a session here.
    pub(crate) fn sessions(&self) -> Option<&Sessions> {
        match self {
            Authenticator::Users(u) => Some(&u.sessions),
            Authenticator::Oidc(o) => Some(o.sessions()),
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
    /// Where the request came from, when the server records it. The
    /// sign-in throttle keys on it so one person cannot lock out another.
    pub peer: Option<IpAddr>,
}

impl Session {
    pub(crate) fn anonymous(mode: Mode) -> Self {
        Self {
            principal: None,
            mode,
            peer: None,
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
    fn the_sign_in_throttle_cannot_be_used_to_lock_someone_out() {
        // Arrange: a stranger hammers a name they know, from their own
        // address. Keyed on the name alone, that used to hold the account
        // shut for everyone, its owner included - and the owner shares the
        // counter with changing their own password.
        let auth = users_auth();
        let stranger = Some("198.51.100.7".parse::<IpAddr>().expect("address"));
        let owner = Some("203.0.113.9".parse::<IpAddr>().expect("address"));
        for _ in 0..20 {
            assert!(auth.verify("alice", "guess", stranger).is_none());
        }

        // Act / Assert: the stranger is throttled, the owner is not.
        assert!(auth.cooldown("alice", stranger).is_some());
        assert!(auth.cooldown("alice", owner).is_none());
        assert!(auth.verify("alice", "correct horse", owner).is_some());
    }

    #[test]
    fn an_over_long_name_is_refused_without_growing_the_throttle_map() {
        // Arrange: a flood of distinct megabyte-long names, the shape of an
        // unauthenticated memory-exhaustion attempt on the sign-in page.
        let auth = users_auth();
        let peer = Some("198.51.100.7".parse::<IpAddr>().expect("address"));

        // Act
        for i in 0..64 {
            let name = format!("{i}{}", "x".repeat(2 * 1024 * 1024));
            assert!(auth.verify(&name, "guess", peer).is_none());
        }

        // Assert: nothing over the length limit was ever tracked.
        let tracked = auth.failures.lock().expect("lock").len();
        assert_eq!(tracked, 0, "over-long names must never enter the map");
        assert!(auth.cooldown(&"x".repeat(MAX_NAME_LEN + 1), peer).is_none());
    }

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
    fn an_ended_session_is_refused_in_every_browser_that_holds_its_cookie() {
        // Arrange: sessions recorded in a store; alice signed in on two
        // devices, bob on one.
        let dir = std::env::temp_dir().join(format!("pgokf-sess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let store = SessionStore::open(&dir.join("sessions")).expect("store");
        let sessions = Sessions::new(vec![7_u8; 32], 3600, false)
            .expect("valid")
            .with_store(store);
        let present = |set_cookie: &str| -> HeaderMap {
            let pair = set_cookie.split(';').next().unwrap_or_default();
            let mut headers = HeaderMap::new();
            headers.insert("cookie", HeaderValue::from_str(pair).expect("cookie"));
            headers
        };
        let a1 = present(
            &sessions
                .open_session("alice", Mode::Users, String::new())
                .expect("a1"),
        );
        let a2 = present(
            &sessions
                .open_session("alice", Mode::Users, String::new())
                .expect("a2"),
        );
        let b1 = present(
            &sessions
                .open_session("bob", Mode::Users, String::new())
                .expect("b1"),
        );
        assert!(
            sessions.read_session(&a1, Mode::Users).is_some(),
            "issued sessions verify"
        );

        // Act: alice signs out on device 1 - a copy of that cookie elsewhere
        // must die with it; then she signs out everywhere.
        sessions
            .end_session_from(&a1, Mode::Users)
            .expect("ends one");
        let a1_after_logout = sessions.read_session(&a1, Mode::Users).is_some();
        let a2_after_logout = sessions.read_session(&a2, Mode::Users).is_some();
        sessions.end_all_sessions_of("alice").expect("ends all");
        let a2_after_all = sessions.read_session(&a2, Mode::Users).is_some();
        let b1_after_all = sessions.read_session(&b1, Mode::Users).is_some();

        // Assert
        assert!(
            !a1_after_logout,
            "signing out ends that session for every copy of its cookie"
        );
        assert!(a2_after_logout, "the other device stays signed in");
        assert!(!a2_after_all, "sign out everywhere ends the rest");
        assert!(b1_after_all, "another person is untouched");
        assert_eq!(sessions.session_count_for("alice"), Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_validly_signed_cookie_the_store_never_recorded_is_refused() {
        // Arrange: two signers sharing one secret - one without a store
        // (which mints without recording, as a leaked secret would let an
        // attacker do) and one with.
        let dir = std::env::temp_dir().join(format!("pgokf-unrec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let secret = vec![9_u8; 32];
        let storeless = Sessions::new(secret.clone(), 3600, false).expect("valid");
        let store = SessionStore::open(&dir.join("sessions")).expect("store");
        let with_store = Sessions::new(secret, 3600, false)
            .expect("valid")
            .with_store(store);
        let present = |set_cookie: &str| -> HeaderMap {
            let pair = set_cookie.split(';').next().unwrap_or_default();
            let mut headers = HeaderMap::new();
            headers.insert("cookie", HeaderValue::from_str(pair).expect("cookie"));
            headers
        };

        // Act
        let unrecorded = present(
            &storeless
                .open_session("alice", Mode::Users, String::new())
                .expect("mints"),
        );
        let by_signature = storeless.read_session(&unrecorded, Mode::Users).is_some();
        let by_store = with_store.read_session(&unrecorded, Mode::Users).is_some();
        // Ending a session under the wrong mode ends nothing and is not an error.
        let recorded = present(
            &with_store
                .open_session("bob", Mode::Users, String::new())
                .expect("mints"),
        );
        with_store
            .end_session_from(&recorded, Mode::Oidc)
            .expect("wrong mode is a no-op");
        let still_live = with_store.read_session(&recorded, Mode::Users).is_some();

        // Assert
        assert!(by_signature, "the signature alone verifies it");
        assert!(!by_store, "but a session the store never issued is refused");
        assert!(still_live, "a wrong-mode end touches nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cidr_ranges_contain_their_addresses_and_nothing_else() {
        // Arrange
        let lan = Cidr::parse("10.0.0.0/16").expect("valid");
        let host = Cidr::parse("127.0.0.1").expect("valid");
        let six = Cidr::parse("fd00::/8").expect("valid");

        // Act / Assert
        assert!(lan.contains("10.0.0.14".parse().unwrap()));
        assert!(!lan.contains("10.1.0.14".parse().unwrap()));
        assert!(host.contains("127.0.0.1".parse().unwrap()));
        assert!(!host.contains("127.0.0.2".parse().unwrap()));
        assert!(host.contains("::ffff:127.0.0.1".parse().unwrap()));
        assert!(six.contains("fd12::1".parse().unwrap()));
        assert!(!six.contains("fe80::1".parse().unwrap()));
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("nope").is_err());
    }

    #[test]
    fn the_throttle_client_is_the_real_one_behind_a_trusted_proxy() {
        // Arrange: a proxy on 10.0.0.0/8 fronts the site.
        let proxies = TrustedProxies::parse("10.0.0.0/8").expect("valid");
        let proxy: IpAddr = "10.0.0.2".parse().unwrap();
        let direct: IpAddr = "198.51.100.9".parse().unwrap();
        let client: IpAddr = "203.0.113.7".parse().unwrap();
        let mut xff = HeaderMap::new();
        // The proxy appended the client it saw; an attacker's spoofed prefix
        // sits to the left of it.
        xff.insert(
            "x-forwarded-for",
            HeaderValue::from_static("evil, 203.0.113.7"),
        );

        // Act / Assert
        // From the trusted proxy, the rightmost non-proxy hop is the client.
        assert_eq!(
            proxies.client_ip(&xff, Some(proxy)),
            Some(client),
            "the address the proxy appended, not the spoofed prefix"
        );
        // A direct (untrusted) peer is used as-is; its forwarded header is not
        // believed.
        assert_eq!(
            proxies.client_ip(&xff, Some(direct)),
            Some(direct),
            "an untrusted peer's forwarded-for is ignored"
        );
        // With no proxy configured, the peer is always used.
        let none = TrustedProxies::default();
        assert_eq!(none.client_ip(&xff, Some(proxy)), Some(proxy));
    }

    fn header_auth(trusted: &[&str]) -> HeaderAuth {
        HeaderAuth {
            user_header: HeaderName::from_static("x-forwarded-user"),
            name_header: Some(HeaderName::from_static("x-forwarded-preferred-username")),
            groups_header: Some(HeaderName::from_static("x-forwarded-groups")),
            roles: RoleMapping::parse("okf-editors=editor, okf-approvers=approver", Role::Viewer)
                .expect("valid"),
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
        let untrusted = auth.identify(&headers, Some("198.51.100.9".parse().unwrap()));
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
        assert!(RoleMapping::parse("okf-editors", Role::Viewer).is_err());
        assert!(RoleMapping::parse("g=owner", Role::Viewer).is_err());
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
            sessions: Arc::new(Sessions::new(secret, 3600, false).expect("valid")),
            failures: Mutex::new(HashMap::new()),
            verifying: tokio::sync::Semaphore::new(VERIFY_AT_ONCE),
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
        let alice = auth
            .verify("alice", "correct horse", None)
            .expect("verifies");
        let wrong = auth.verify("alice", "wrong", None);
        let nobody = auth.verify("carol", "correct horse", None);
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
        let alice = auth
            .verify("alice", "correct horse", None)
            .expect("verifies");
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
        let auth = UsersAuth::load(
            &path,
            Arc::new(Sessions::new(vec![7_u8; 32], 3600, false).expect("valid")),
        )
        .expect("loads");
        assert_eq!(
            auth.verify("alice", "pw", None).map(|p| p.role),
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
            auth.verify("alice", "pw", None).map(|p| p.role),
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
        let auth = UsersAuth::load(
            &path,
            Arc::new(Sessions::new(vec![7_u8; 32], 3600, false).expect("valid")),
        )
        .expect("loads");

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
        assert!(
            auth.verify("bob", "a different long password", None)
                .is_some()
        );
        assert!(auth.verify("bob", "a long enough password", None).is_none());
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
            assert!(auth.verify("alice", "wrong", None).is_none());
        }
        assert!(auth.cooldown("alice", None).is_none(), "still free");
        assert!(auth.verify("alice", "wrong", None).is_none());
        let waiting = auth.cooldown("alice", None);
        let refused_even_when_right = auth.verify("alice", "correct horse", None);

        // Assert
        assert!(waiting.is_some(), "a cooldown started");
        assert!(
            refused_even_when_right.is_none(),
            "not checked during the cooldown"
        );
        assert!(
            auth.verify("bob", "correct horse", None).is_some(),
            "other names are unaffected"
        );
        auth.forget_failures("alice", None);
        assert!(auth.verify("alice", "correct horse", None).is_some());
        assert!(
            auth.cooldown("alice", None).is_none(),
            "a success clears the record"
        );
        assert!(
            auth.verify("nobody", "x", None).is_none(),
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
        let auth = UsersAuth::load(
            &path,
            Arc::new(Sessions::new(vec![7_u8; 32], 3600, false).expect("valid")),
        )
        .expect("loads");
        let alice = auth
            .verify("alice", "first password", None)
            .expect("verifies");
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
        let again = auth
            .verify("alice", "second password!", None)
            .expect("verifies");
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
