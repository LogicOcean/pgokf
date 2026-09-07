// SPDX-License-Identifier: AGPL-3.0-only
//! Who may call this server over HTTP, and which tools they may call.
//!
//! Over stdio the client already holds the connection string and there is
//! nothing to authenticate; over HTTP the server is reachable, so every
//! request carries a bearer token. Tokens live in a file the operator
//! writes with `pgokf-mcp hash-token`, one per line:
//!
//! ```text
//! name:role:<sha256 of the token, hex>
//! ```
//!
//! The digest is the last field, so a future field would be added after it;
//! a line with more than three is refused rather than half-understood.
//!
//! Only the digest is stored, so the file leaks nothing usable, and a token
//! is a long random string rather than a password: one SHA-256 and a
//! constant-time comparison per request, with no slow hash to make a
//! request expensive. That trade is only sound while the token really is
//! unguessable, so [`Tokens::bearer`] accepts only the shape [`new_token`]
//! mints — a hand-chosen token is refused rather than silently protected by
//! a fast hash.
//!
//! The file is re-read when it changes, so removing a line revokes that
//! token without a restart. A file that cannot be read or no longer parses
//! is reported and the last good set kept for [`STALE_GRACE`] — long enough
//! to ride out a half-written save, short enough that a revocation cannot
//! fail silently for ever. [`Tokens::stale`] says so, and the health
//! endpoint reports it.

use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// How long a tokens file that cannot be read keeps the last good set.
/// After this the set is dropped and every request is refused: an operator
/// who removed a line must never be left believing a token is gone when it
/// is not.
pub const STALE_GRACE: Duration = Duration::from_mins(1);

/// How often the file is looked at. This runs on the request path, so it is
/// throttled: a revocation takes effect within a second, and a flood of
/// requests still costs one `stat` per second.
const CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// The longest a token name may be. It is written into the log beside every
/// call the token makes.
const NAME_MAX: usize = 128;

/// What a token may do.
///
/// **The declaration order is the privilege order**: [`Role::allows_tool`]
/// compares roles with `>=`, so a role added in the middle of this list
/// inherits everything below it and is inherited by everything above it. A
/// test pins the order of [`Role::all`].
///
/// Both roles are read-only against the catalog — the server holds a reader
/// connection. The distinction is between reading the catalog and building
/// a workspace plugin out of it, which reads every selected source through
/// the audited readers and so leaves a much longer trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// Search the catalog and read concepts and skills.
    Reader,
    /// Everything a reader may do, and build workspace plugins.
    Builder,
}

impl Role {
    /// Every role, least privileged first.
    pub const fn all() -> &'static [Role] {
        &[Role::Reader, Role::Builder]
    }

    pub const fn id(self) -> &'static str {
        match self {
            Role::Reader => "reader",
            Role::Builder => "builder",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        let id = id.trim();
        Self::all().iter().copied().find(|role| role.id() == id)
    }

    /// Whether this role may call `tool`. The one place that decides it:
    /// `tools/list` filters by the same answer, so a client is never shown
    /// a tool its token cannot call.
    pub fn allows_tool(self, tool: &str) -> bool {
        match tool {
            "concept_search" | "find_similar" | "concept_neighbors" | "get_concept"
            | "get_skill" => true,
            "list_plugin_targets" | "build_workspace_plugin" => self >= Role::Builder,
            // A tool no role names is refused here and reported as unknown
            // by the catalog, never silently allowed.
            _ => false,
        }
    }

    /// Whether any role may call `tool`, which is how a tool this file has
    /// never heard of is told apart from one the caller merely may not
    /// reach.
    pub fn any_allows(tool: &str) -> bool {
        Self::all().iter().any(|role| role.allows_tool(tool))
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// The bearer of a token: what to call them in the log, and what they may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bearer {
    pub name: String,
    pub role: Role,
}

/// The prefix every token carries, so one is recognizable in a
/// configuration file or a leak report.
pub const TOKEN_PREFIX: &str = "pgokf_";

/// How much randomness a token carries. 32 bytes is 256 bits, so guessing
/// one is not a threat and a fast hash is the right way to store it.
const TOKEN_BYTES: usize = 32;

/// How many base64url characters [`TOKEN_BYTES`] become without padding.
const TOKEN_CHARS: usize = TOKEN_BYTES.div_ceil(3) * 4 - 1;

/// The tokens file as last read, and what it looked like when it was.
#[derive(Debug)]
struct Loaded {
    /// Enough of the file's metadata to notice any change, not just a
    /// newer timestamp: a same-second rewrite moves the length or the
    /// inode even when the modification time stands still.
    stamp: Option<Stamp>,
    /// Digest (hex, lower case) to bearer.
    by_digest: HashMap<String, Bearer>,
    /// When the file was last looked at, so the request path does not stat
    /// it more than once a second.
    checked_at: Instant,
    /// Since when the file has been unreadable or unparseable, and why.
    /// `None` while it is fine.
    degraded: Option<(Instant, String)>,
}

/// What the tokens file looked like when it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    inode: u64,
}

impl Stamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
            inode: inode_of(metadata),
        }
    }
}

#[cfg(unix)]
fn inode_of(metadata: &std::fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::ino(metadata)
}

#[cfg(not(unix))]
fn inode_of(_metadata: &std::fs::Metadata) -> u64 {
    0
}

/// The tokens this server accepts, re-read when the file changes so a
/// revoked token stops working without a restart.
#[derive(Debug)]
pub struct Tokens {
    path: PathBuf,
    loaded: RwLock<Loaded>,
}

impl Tokens {
    /// Read a tokens file.
    ///
    /// A file that holds no token at all is accepted with a warning: it is
    /// the state an operator reaches by revoking the last token, and it
    /// simply refuses every request.
    ///
    /// # Errors
    ///
    /// The file cannot be read, is not a regular file, a line is malformed,
    /// or it names an unknown role or a digest that is not 64 hex
    /// characters.
    pub fn load(path: &Path) -> Result<Self> {
        let (stamp, by_digest) = Self::read(path)?;
        if by_digest.is_empty() {
            eprintln!(
                "pgokf-mcp: the tokens file {} holds no token; every request will be refused",
                path.display()
            );
        }
        Ok(Self {
            path: path.to_path_buf(),
            loaded: RwLock::new(Loaded {
                stamp,
                by_digest,
                checked_at: Instant::now(),
                degraded: None,
            }),
        })
    }

    /// Stat, then read: the stamp belongs to a file at least as old as the
    /// content, so a write that lands between the two is noticed on the
    /// next check rather than swallowed for ever.
    fn read(path: &Path) -> Result<(Option<Stamp>, HashMap<String, Bearer>)> {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("reading the tokens file {}", path.display()))?;
        if !metadata.is_file() {
            bail!("the tokens file {} is not a regular file", path.display());
        }
        warn_if_group_or_world_readable(path, &metadata);
        let stamp = Stamp::of(&metadata);
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the tokens file {}", path.display()))?;
        Ok((Some(stamp), Self::parse(&text)?))
    }

    /// Parse the file's lines; `#` comments and blank lines are skipped.
    fn parse(text: &str) -> Result<HashMap<String, Bearer>> {
        let mut tokens = HashMap::new();
        // A file an editor stamped with a byte order mark is still a tokens
        // file; `parse_env_file` strips the same mark.
        for (index, line) in text.trim_start_matches('\u{feff}').lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let number = index + 1;
            let mut parts = line.splitn(4, ':');
            let (name, role, digest) = match (parts.next(), parts.next(), parts.next()) {
                (Some(n), Some(r), Some(d)) => (n.trim(), r.trim(), d.trim()),
                _ => bail!("tokens file line {number}: expected name:role:digest"),
            };
            if parts.next().is_some() {
                bail!(
                    "tokens file line {number}: the digest is the last field of name:role:digest"
                );
            }
            valid_token_name(name).with_context(|| format!("tokens file line {number}"))?;
            let role = Role::parse(role)
                .with_context(|| format!("tokens file line {number}: unknown role {role:?}"))?;
            if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
                bail!("tokens file line {number}: the digest is not 64 hex characters");
            }
            let bearer = Bearer {
                name: name.to_owned(),
                role,
            };
            if tokens.insert(digest.to_ascii_lowercase(), bearer).is_some() {
                bail!("tokens file line {number}: this token appears twice");
            }
        }
        Ok(tokens)
    }

    /// Re-read the file when anything about it moved.
    ///
    /// A file that no longer reads or parses keeps the last good set for
    /// [`STALE_GRACE`] and then loses it, so a half-written save is ridden
    /// out but a revocation can never fail silently for ever. An empty but
    /// well-formed file is applied at once: it is how the last token is
    /// revoked.
    fn refresh(&self) {
        {
            let Ok(loaded) = self.loaded.read() else {
                return;
            };
            if loaded.checked_at.elapsed() < CHECK_INTERVAL {
                return;
            }
        }
        let outcome = Self::read(&self.path);
        let Ok(mut loaded) = self.loaded.write() else {
            return;
        };
        loaded.checked_at = Instant::now();
        match outcome {
            Ok((stamp, by_digest)) => {
                if loaded.degraded.take().is_some() {
                    eprintln!("pgokf-mcp: the tokens file reads again");
                }
                if loaded.stamp != stamp {
                    loaded.stamp = stamp;
                    loaded.by_digest = by_digest;
                }
            }
            Err(error) => match &loaded.degraded {
                Some((since, _)) if since.elapsed() >= STALE_GRACE => {
                    if !loaded.by_digest.is_empty() {
                        eprintln!(
                            "pgokf-mcp: the tokens file has not read for {}s; refusing every \
                             request until it does",
                            STALE_GRACE.as_secs()
                        );
                        loaded.by_digest.clear();
                        loaded.stamp = None;
                    }
                }
                Some(_) => {}
                None => {
                    eprintln!("pgokf-mcp: the tokens file did not reload: {error:#}");
                    loaded.degraded = Some((Instant::now(), format!("{error:#}")));
                }
            },
        }
    }

    /// The bearer a presented token names, or `None`.
    ///
    /// Only the shape [`new_token`] mints is considered, so junk is
    /// discarded before it is hashed and the fast-hash trade stays sound.
    /// The lookup is by digest, and the digest is compared in constant
    /// time, so neither the token nor how much of it was right can be
    /// learnt from the timing.
    pub fn bearer(&self, presented: &str) -> Option<Bearer> {
        self.refresh();
        if !is_minted_token(presented) {
            return None;
        }
        let digest = digest_of(presented);
        let loaded = self.loaded.read().ok()?;
        loaded
            .by_digest
            .iter()
            .find(|(known, _)| constant_time_eq(known, &digest))
            .map(|(_, bearer)| bearer.clone())
    }

    /// Why the tokens file is not being believed, if it is not: the health
    /// endpoint reports this, so a revocation that is not taking effect is
    /// visible rather than silent.
    #[must_use]
    pub fn stale(&self) -> Option<String> {
        let loaded = self.loaded.read().ok()?;
        loaded
            .degraded
            .as_ref()
            .map(|(since, why)| format!("unreadable for {}s: {why}", since.elapsed().as_secs()))
    }

    /// How many tokens the file holds, for the startup line.
    pub fn count(&self) -> usize {
        self.loaded
            .read()
            .map_or(0, |loaded| loaded.by_digest.len())
    }
}

/// A tokens file names who may call; a mode that lets anyone else read it is
/// worth saying out loud, even though a digest is not a token.
#[cfg(unix)]
fn warn_if_group_or_world_readable(path: &Path, metadata: &std::fs::Metadata) {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        eprintln!(
            "pgokf-mcp: the tokens file {} is mode {:04o}; chmod 600 it",
            path.display(),
            mode & 0o7777
        );
    }
}

#[cfg(not(unix))]
fn warn_if_group_or_world_readable(_path: &Path, _metadata: &std::fs::Metadata) {}

/// Check that a name is one plain word of at most [`NAME_MAX`] characters:
/// it appears in the log beside every call the token makes.
///
/// # Errors
///
/// The name is empty, too long, or holds a character that is not a letter,
/// a digit, or one of `. _ - @ +`.
pub fn valid_token_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("a token name cannot be empty");
    }
    if name.chars().count() > NAME_MAX {
        bail!("a token name may be at most {NAME_MAX} characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '+'))
    {
        bail!("{name:?} is not a valid token name (letters, digits, . _ - @ +)");
    }
    Ok(())
}

/// Whether a presented string has the shape [`new_token`] mints. Anything
/// else cannot be a token this server issued, so it is refused before it is
/// hashed.
fn is_minted_token(presented: &str) -> bool {
    let Some(random) = presented.strip_prefix(TOKEN_PREFIX) else {
        return false;
    };
    random.len() == TOKEN_CHARS
        && random
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The stored form of a token.
#[must_use]
pub fn digest_of(token: &str) -> String {
    let digest = Sha256::digest(token.trim().as_bytes());
    digest
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// A fresh token: a recognizable prefix and 32 random bytes.
///
/// # Errors
///
/// The system random source failing.
pub fn new_token() -> Result<String> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("reading random bytes: {e}"))?;
    Ok(format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

/// Compare two digests without giving away where they first differ.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0_u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_decides_every_tool_and_nothing_else() {
        // Arrange / Act / Assert
        for tool in [
            "concept_search",
            "find_similar",
            "concept_neighbors",
            "get_concept",
            "get_skill",
        ] {
            assert!(Role::Reader.allows_tool(tool), "{tool}");
            assert!(Role::Builder.allows_tool(tool), "{tool}");
        }
        for tool in ["list_plugin_targets", "build_workspace_plugin"] {
            assert!(!Role::Reader.allows_tool(tool), "{tool}");
            assert!(Role::Builder.allows_tool(tool), "{tool}");
        }
        assert!(!Role::Builder.allows_tool("drop_everything"));
        assert!(!Role::any_allows("drop_everything"));
        assert!(Role::any_allows("build_workspace_plugin"));
        for role in Role::all() {
            assert_eq!(Role::parse(role.id()), Some(*role));
        }
        assert_eq!(Role::parse("admin"), None);
    }

    #[test]
    fn the_role_ladder_is_declared_least_privileged_first() {
        // Arrange / Act / Assert: `allows_tool` compares roles with `>=`,
        // so a role declared out of order would silently inherit the wrong
        // authority.
        assert!(Role::all().is_sorted(), "Role::all() must be a ladder");
        assert_eq!(Role::all().first(), Some(&Role::Reader));
    }

    #[test]
    fn a_token_is_recognizable_random_and_stored_only_as_a_digest() {
        // Arrange / Act
        let token = new_token().expect("random");
        let again = new_token().expect("random");
        let digest = digest_of(&token);

        // Assert
        assert!(token.starts_with(TOKEN_PREFIX));
        assert_eq!(TOKEN_CHARS, 43, "32 bytes, base64url, unpadded");
        assert_eq!(token.len(), TOKEN_PREFIX.len() + TOKEN_CHARS);
        assert_ne!(token, again);
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(digest, digest_of(&format!("  {token}  ")), "trimmed");
        assert_ne!(digest, digest_of(&again));
    }

    #[test]
    fn only_a_minted_token_is_even_hashed() {
        // Arrange
        let token = new_token().expect("random");

        // Act / Assert
        assert!(is_minted_token(&token));
        assert!(!is_minted_token("hunter2"), "a chosen password is not one");
        assert!(!is_minted_token(&token[..token.len() - 1]), "too short");
        assert!(!is_minted_token(&format!("{token}x")), "too long");
        assert!(!is_minted_token(&token[TOKEN_PREFIX.len()..]), "no prefix");
        assert!(
            !is_minted_token(&format!("{TOKEN_PREFIX}{}", "!".repeat(TOKEN_CHARS))),
            "base64url characters only"
        );
    }

    fn write_tokens(tag: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pgokf-tokens-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("tokens");
        std::fs::write(&path, text).expect("write");
        path
    }

    #[test]
    fn a_tokens_file_names_a_bearer_for_the_token_it_holds() {
        // Arrange
        let token = new_token().expect("random");
        let path = write_tokens(
            "load",
            &format!("# agents\nfleet:builder:{}\n\n", digest_of(&token)),
        );
        let tokens = Tokens::load(&path).expect("loads");

        // Act
        let bearer = tokens.bearer(&token);
        let unknown = tokens.bearer(&new_token().expect("random"));
        let empty = tokens.bearer("");

        // Assert
        assert_eq!(
            bearer,
            Some(Bearer {
                name: "fleet".to_owned(),
                role: Role::Builder
            })
        );
        assert!(unknown.is_none() && empty.is_none());
        assert_eq!(tokens.count(), 1);
        assert!(tokens.stale().is_none());
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[test]
    fn a_malformed_tokens_file_is_refused_line_by_line() {
        // Arrange
        let digest = digest_of("x");

        // Act / Assert
        assert!(Tokens::parse(&format!("fleet:builder:{digest}")).is_ok());
        assert!(
            Tokens::parse(&format!("\u{feff}fleet:builder:{digest}")).is_ok(),
            "a byte order mark is not part of the first name"
        );
        assert!(
            Tokens::parse(&format!(
                "fleet:builder:{digest}\r\nb:reader:{}",
                digest_of("y")
            ))
            .is_ok(),
            "CRLF endings"
        );
        assert!(Tokens::parse("fleet:builder").is_err());
        assert!(Tokens::parse(&format!("fleet:admin:{digest}")).is_err());
        assert!(Tokens::parse("fleet:builder:short").is_err());
        assert!(Tokens::parse(&format!("fl eet:builder:{digest}")).is_err());
        assert!(
            Tokens::parse(&format!("fleet:builder:{digest}:2027-01-01")).is_err(),
            "the digest is the last field"
        );
        assert!(
            Tokens::parse(&format!("{}:reader:{digest}", "n".repeat(NAME_MAX + 1))).is_err(),
            "a name is bounded"
        );
        assert!(
            Tokens::parse(&format!("a:reader:{digest}\nb:builder:{digest}")).is_err(),
            "one token cannot name two bearers"
        );
        assert!(Tokens::parse("# nothing\n\n").expect("parses").is_empty());
    }

    /// Move a file's modification time so a reload is due, whatever the
    /// filesystem's timestamp granularity.
    fn touch(path: &Path, seconds: u64) {
        let later = SystemTime::now() + Duration::from_secs(seconds);
        std::fs::File::options()
            .write(true)
            .open(path)
            .and_then(|f| f.set_modified(later))
            .expect("touch");
    }

    /// Let the throttle expire so the next call really looks at the file.
    fn allow_a_recheck(tokens: &Tokens) {
        let mut loaded = tokens.loaded.write().expect("lock");
        loaded.checked_at = Instant::now()
            .checked_sub(CHECK_INTERVAL + Duration::from_millis(1))
            .expect("a moment ago");
    }

    #[test]
    fn a_changed_tokens_file_is_read_again_on_the_next_request() {
        // Arrange
        let first = new_token().expect("random");
        let second = new_token().expect("random");
        let path = write_tokens("reload", &format!("fleet:reader:{}\n", digest_of(&first)));
        let tokens = Tokens::load(&path).expect("loads");
        assert!(tokens.bearer(&first).is_some());

        // Act: revoke the first token and issue another.
        std::fs::write(&path, format!("fleet:builder:{}\n", digest_of(&second))).expect("rewrite");
        touch(&path, 5);
        allow_a_recheck(&tokens);

        // Assert
        assert!(tokens.bearer(&first).is_none(), "the revoked token is gone");
        assert_eq!(tokens.bearer(&second).map(|b| b.role), Some(Role::Builder));
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[test]
    fn a_rewrite_that_keeps_the_modification_time_is_still_noticed() {
        // Arrange
        let first = new_token().expect("random");
        let second = new_token().expect("random");
        let path = write_tokens(
            "same-mtime",
            &format!("fleet:reader:{}\n", digest_of(&first)),
        );
        let tokens = Tokens::load(&path).expect("loads");
        let stamped = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .expect("mtime");

        // Act: rewrite with two lines and put the old modification time
        // back, as a filesystem with coarse timestamps would leave it.
        std::fs::write(
            &path,
            format!(
                "fleet:reader:{}\nother:builder:{}\n",
                digest_of(&first),
                digest_of(&second)
            ),
        )
        .expect("rewrite");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(stamped))
            .expect("restore mtime");
        allow_a_recheck(&tokens);

        // Assert: the length moved even though the timestamp did not.
        assert_eq!(tokens.bearer(&second).map(|b| b.role), Some(Role::Builder));
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[test]
    fn revoking_the_last_token_takes_effect() {
        // Arrange
        let token = new_token().expect("random");
        let path = write_tokens("last", &format!("fleet:reader:{}\n", digest_of(&token)));
        let tokens = Tokens::load(&path).expect("loads");

        // Act
        std::fs::write(&path, "# everyone revoked\n").expect("rewrite");
        touch(&path, 5);
        allow_a_recheck(&tokens);

        // Assert
        assert!(tokens.bearer(&token).is_none());
        assert_eq!(tokens.count(), 0);
        assert!(tokens.stale().is_none(), "an empty file is not a failure");
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[test]
    fn a_file_that_stops_reading_is_ridden_out_and_then_refused() {
        // Arrange
        let token = new_token().expect("random");
        let path = write_tokens("degraded", &format!("fleet:reader:{}\n", digest_of(&token)));
        let tokens = Tokens::load(&path).expect("loads");

        // Act: a half-written save, seen once.
        std::fs::write(&path, "garbage\n").expect("rewrite");
        touch(&path, 5);
        allow_a_recheck(&tokens);
        let during_the_grace = tokens.bearer(&token);

        // Assert: the last good set stands, and the trouble is reportable.
        assert!(during_the_grace.is_some());
        assert!(tokens.stale().is_some_and(|why| why.contains("line 1")));

        // Act: the same trouble, past the grace window.
        {
            let mut loaded = tokens.loaded.write().expect("lock");
            let (_, why) = loaded.degraded.take().expect("degraded");
            let long_ago = Instant::now()
                .checked_sub(STALE_GRACE + Duration::from_secs(1))
                .expect("a while ago");
            loaded.degraded = Some((long_ago, why));
        }
        allow_a_recheck(&tokens);

        // Assert: nothing is believed any more.
        assert!(tokens.bearer(&token).is_none(), "fails closed in the end");
        assert_eq!(tokens.count(), 0);
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[test]
    fn a_tokens_file_that_is_not_a_regular_file_is_refused() {
        // Arrange
        let dir = std::env::temp_dir().join(format!("pgokf-tokens-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");

        // Act
        let loaded = Tokens::load(&dir);

        // Assert
        assert!(
            loaded
                .as_ref()
                .is_err_and(|e| format!("{e:#}").contains("not a regular file")),
            "{loaded:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_token_name_is_one_bounded_plain_word() {
        // Arrange / Act / Assert
        assert!(valid_token_name("fleet-1@example.com").is_ok());
        assert!(valid_token_name("").is_err());
        assert!(valid_token_name("bad name").is_err());
        assert!(valid_token_name("bad:name").is_err());
        assert!(valid_token_name(&"n".repeat(NAME_MAX)).is_ok());
        assert!(valid_token_name(&"n".repeat(NAME_MAX + 1)).is_err());
    }

    #[test]
    fn digests_are_compared_without_leaking_where_they_differ() {
        // Arrange / Act / Assert
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
    }
}
