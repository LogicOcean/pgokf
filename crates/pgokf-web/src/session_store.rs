// SPDX-License-Identifier: AGPL-3.0-only
//! The sessions this site has issued and not yet ended, so a session can be
//! *ended* - by its owner signing out, by "sign out everywhere", or by an
//! admin - rather than merely left to expire.
//!
//! A session cookie is signed, so on its own the server can only *verify*
//! one; it has no way to *forget* one, and two validly signed cookies look
//! alike. Signing out therefore only cleared the cookie in that one browser,
//! and a copy taken beforehand kept working until it expired. This store is
//! the server's memory of which sessions are live: a cookie that verifies
//! but whose nonce is not here is refused, so ending a session ends every
//! copy of it at once. It is a lever, not a detector: a copied cookie works
//! until its session is ended or expires.
//!
//! It is one small file beside the users file (`nonce:subject:expires`, one
//! line each), written the way the users file is - `0600`, synced, renamed
//! into place - and re-read when its modification time moves, so the
//! per-request cost is one `stat` and a map lookup. One process owns it:
//! two UI instances sharing one store would race their read-modify-writes.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};

use crate::auth::{now_unix, write_private};

/// The live sessions, as the file was last read.
#[derive(Debug, Default)]
struct LoadedSessions {
    modified: Option<SystemTime>,
    live: HashMap<String, LiveSession>,
}

/// One issued session that has not been ended.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveSession {
    subject: String,
    expires: u64,
}

/// The file-backed set of live sessions (see the module doc).
pub(crate) struct SessionStore {
    path: PathBuf,
    loaded: RwLock<LoadedSessions>,
    /// Serialises read-modify-writes of the file within this process.
    write: Mutex<()>,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SessionStore {
    /// Open the store at `path`, creating an empty file (mode `0600`) when
    /// there is none, and rewriting it once whether or not it existed: that
    /// proves the directory is writable at startup rather than at the first
    /// sign-in, and drops the sessions that expired while the process was
    /// down. A file that does not parse refuses to open - an allowlist that
    /// is half-read is worse than none - and deleting it starts the server
    /// with no live sessions (everyone signs in again).
    ///
    /// # Errors
    ///
    /// The file cannot be read, created, parsed, or rewritten.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        if !path.exists() {
            write_private(path, "")
                .with_context(|| format!("creating the session store {}", path.display()))?;
        }
        let (modified, live) = Self::read_file(path)?;
        let store = Self {
            path: path.to_path_buf(),
            loaded: RwLock::new(LoadedSessions { modified, live }),
            write: Mutex::new(()),
        };
        store
            .mutate(now_unix(), |_| {})
            .with_context(|| format!("the session store {} is not writable", path.display()))?;
        Ok(store)
    }

    /// Whether the session `nonce` was issued to `subject` and has not
    /// been ended or expired.
    pub(crate) fn is_live(&self, nonce: &str, subject: &str, now: u64) -> bool {
        self.refresh();
        self.loaded.read().is_ok_and(|loaded| {
            loaded
                .live
                .get(nonce)
                .is_some_and(|s| s.subject == subject && s.expires > now)
        })
    }

    /// Record a newly issued session.
    ///
    /// # Errors
    ///
    /// The file cannot be rewritten.
    pub(crate) fn add(&self, nonce: &str, subject: &str, expires: u64, now: u64) -> Result<()> {
        if nonce.is_empty() || nonce.contains([':', '\n', '\r']) {
            bail!("a session nonce must be one plain token");
        }
        if subject.is_empty() || subject.contains([':', '\n', '\r']) {
            bail!("a session subject must be one plain token");
        }
        self.mutate(now, |live| {
            live.insert(
                nonce.to_owned(),
                LiveSession {
                    subject: subject.to_owned(),
                    expires,
                },
            );
        })
    }

    /// End one session. Ending one that is already gone is not an error.
    ///
    /// # Errors
    ///
    /// The file cannot be rewritten.
    pub(crate) fn remove(&self, nonce: &str, now: u64) -> Result<()> {
        self.mutate(now, |live| {
            live.remove(nonce);
        })
    }

    /// End every session of `subject`: "sign out everywhere", an admin
    /// ending someone's sessions, a changed password, a removed person.
    ///
    /// # Errors
    ///
    /// The file cannot be rewritten.
    pub(crate) fn remove_all_for(&self, subject: &str, now: u64) -> Result<()> {
        self.mutate(now, |live| {
            live.retain(|_, s| s.subject != subject);
        })
    }

    /// How many sessions `subject` holds, for the profile page.
    pub(crate) fn count_for(&self, subject: &str, now: u64) -> usize {
        self.refresh();
        self.loaded.read().map_or(0, |loaded| {
            loaded
                .live
                .values()
                .filter(|s| s.subject == subject && s.expires > now)
                .count()
        })
    }

    /// Everyone holding a live session, with how many, sorted by subject -
    /// so an admin can see whom there is to sign out even where no users
    /// file lists people (`oidc` mode).
    pub(crate) fn subjects(&self, now: u64) -> Vec<(String, usize)> {
        self.refresh();
        let mut counts: HashMap<String, usize> = HashMap::new();
        if let Ok(loaded) = self.loaded.read() {
            for session in loaded.live.values().filter(|s| s.expires > now) {
                *counts.entry(session.subject.clone()).or_insert(0) += 1;
            }
        }
        let mut subjects: Vec<(String, usize)> = counts.into_iter().collect();
        subjects.sort();
        subjects
    }

    /// Read-modify-write under the process lock: re-read the file (another
    /// writer in this process may have moved it), apply `edit`, drop what
    /// has expired, and write the result whole. The in-memory map is
    /// replaced under the same write lock a reload installs through, so a
    /// reload racing this write can never put an older map back.
    fn mutate(&self, now: u64, edit: impl FnOnce(&mut HashMap<String, LiveSession>)) -> Result<()> {
        let _serial = self
            .write
            .lock()
            .map_err(|_| anyhow!("the session store lock is poisoned"))?;
        let (_, mut live) = Self::read_file(&self.path)?;
        edit(&mut live);
        live.retain(|_, s| s.expires > now);
        let mut text =
            String::from("# pgokf-web live sessions: nonce:subject:expires, one per line\n");
        let mut entries: Vec<(&String, &LiveSession)> = live.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (nonce, session) in entries {
            let _ = writeln!(text, "{nonce}:{}:{}", session.subject, session.expires);
        }
        write_private(&self.path, &text)?;
        let modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if let Ok(mut loaded) = self.loaded.write() {
            *loaded = LoadedSessions { modified, live };
        }
        Ok(())
    }

    /// Re-read the file when its modification time moved. The reload is
    /// installed only if nothing else moved the map while it was being
    /// read (compare-and-install), so it never overwrites a writer's newer
    /// set with an older one. A file that no longer reads is reported once
    /// and the last good set kept, so a transient failure does not sign
    /// everyone out.
    fn refresh(&self) {
        let on_disk = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        let Ok(seen) = self.loaded.read().map(|loaded| loaded.modified) else {
            return;
        };
        if seen == on_disk {
            return;
        }
        match Self::read_file(&self.path) {
            Ok((modified, live)) => {
                if let Ok(mut loaded) = self.loaded.write()
                    && loaded.modified == seen
                {
                    *loaded = LoadedSessions { modified, live };
                }
            }
            Err(error) => {
                if let Ok(mut loaded) = self.loaded.write()
                    && loaded.modified == seen
                {
                    // Remember the stamp of the attempt, so the failure is
                    // reported once and retried only when the file moves again.
                    loaded.modified = on_disk;
                    eprintln!(
                        "pgokf-web: the session store did not reload; keeping the last good \
                         set: {error:#}"
                    );
                }
            }
        }
    }

    /// The file's contents and the modification time they belong to. The
    /// stamp is taken *before* the read: a text older than its recorded
    /// stamp is then impossible, so a reload can never install a stale map
    /// under a fresh stamp and stop noticing later changes.
    fn read_file(path: &Path) -> Result<(Option<SystemTime>, HashMap<String, LiveSession>)> {
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the session store {}", path.display()))?;
        Ok((modified, Self::parse(&text)?))
    }

    /// Parse `nonce:subject:expires` lines; `#` comments and blank lines
    /// are ignored, and a malformed line is an error naming it, so the
    /// store is never silently half-read.
    fn parse(text: &str) -> Result<HashMap<String, LiveSession>> {
        let mut live = HashMap::new();
        for (index, raw) in text.lines().enumerate() {
            let entry = raw.trim();
            if entry.is_empty() || entry.starts_with('#') {
                continue;
            }
            let mut parts = entry.splitn(3, ':');
            let (Some(nonce), Some(subject), Some(expires)) =
                (parts.next(), parts.next(), parts.next())
            else {
                bail!(
                    "line {} of the session store is not nonce:subject:expires",
                    index + 1
                );
            };
            let expires: u64 = expires.trim().parse().with_context(|| {
                format!("line {} of the session store has a bad expiry", index + 1)
            })?;
            if nonce.is_empty() || subject.is_empty() {
                bail!("line {} of the session store has an empty field", index + 1);
            }
            live.insert(
                nonce.to_owned(),
                LiveSession {
                    subject: subject.to_owned(),
                    expires,
                },
            );
        }
        Ok(live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pgokf-sessions-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn temp_store(tag: &str) -> (PathBuf, SessionStore) {
        let dir = temp_dir(tag);
        let store = SessionStore::open(&dir.join("sessions")).expect("opens");
        (dir, store)
    }

    #[test]
    fn a_session_is_live_only_between_being_added_and_being_ended() {
        // Arrange
        let (dir, store) = temp_store("lifecycle");
        let now = 1_000;

        // Act
        let before = store.is_live("n1", "alice", now);
        store.add("n1", "alice", now + 100, now).expect("adds");
        let after_add = store.is_live("n1", "alice", now);
        let wrong_subject = store.is_live("n1", "bob", now);
        store.remove("n1", now).expect("ends");
        let after_end = store.is_live("n1", "alice", now);

        // Assert
        assert!(!before, "unknown until added");
        assert!(after_add);
        assert!(!wrong_subject, "a nonce belongs to one subject");
        assert!(!after_end, "ended sessions are refused");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ending_everything_for_one_person_leaves_the_others() {
        // Arrange: alice on two devices, bob on one.
        let (dir, store) = temp_store("revoke-all");
        let now = 1_000;
        store.add("a1", "alice", now + 100, now).expect("adds");
        store.add("a2", "alice", now + 100, now).expect("adds");
        store.add("b1", "bob", now + 100, now).expect("adds");

        // Act
        store.remove_all_for("alice", now).expect("ends");

        // Assert
        assert!(!store.is_live("a1", "alice", now));
        assert!(!store.is_live("a2", "alice", now));
        assert!(store.is_live("b1", "bob", now), "bob is untouched");
        assert_eq!(store.count_for("alice", now), 0);
        assert_eq!(store.count_for("bob", now), 1);
        assert_eq!(store.subjects(now), vec![("bob".to_owned(), 1)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_sessions_are_refused_and_pruned_on_the_next_write() {
        // Arrange
        let (dir, store) = temp_store("expiry");
        store.add("old", "alice", 500, 100).expect("adds");
        store.add("new", "alice", 5_000, 100).expect("adds");

        // Act: time passes past the first expiry; a write prunes it.
        let stale = store.is_live("old", "alice", 1_000);
        store.add("x", "carol", 9_000, 1_000).expect("adds");
        let text = std::fs::read_to_string(dir.join("sessions")).expect("reads");

        // Assert
        assert!(!stale, "past its expiry it is refused even while on disk");
        assert!(!text.contains("old:"), "pruned on write: {text}");
        assert!(text.contains("new:alice:5000"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_store_survives_a_restart_and_notices_an_outside_change() {
        // Arrange: a session recorded by one process instance.
        let (dir, store) = temp_store("restart");
        // A far-future expiry: reopening prunes what has expired by the real
        // clock, and a session that outlives the restart must not be one.
        store.add("n1", "alice", 9_999_999_999, 100).expect("adds");
        let path = dir.join("sessions");

        // Act: a fresh instance (a restart) reads the same file; then the
        // file is rewritten behind its back (an operator revoking by hand),
        // with its stamp moved past what a coarse filesystem could blur.
        let restarted = SessionStore::open(&path).expect("reopens");
        let survived = restarted.is_live("n1", "alice", 100);
        write_private(&path, "# emptied\n").expect("rewrites");
        let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        let _ = std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(bumped));
        let after_outside_edit = restarted.is_live("n1", "alice", 100);

        // Assert
        assert!(survived, "sessions outlive a restart");
        assert!(!after_outside_edit, "an outside edit is picked up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_rewrites_the_file_so_an_unwritable_store_fails_at_startup() {
        // Arrange: a raw file as an operator might leave it - no header, and
        // one session long expired.
        let dir = temp_dir("boot-rewrite");
        let path = dir.join("sessions");
        std::fs::write(&path, "gone:alice:1\nkeep:bob:9999999999\n").expect("seed");

        // Act
        let store = SessionStore::open(&path).expect("opens");
        let text = std::fs::read_to_string(&path).expect("reads");

        // Assert: the file was rewritten at open (so an unwritable directory
        // would have failed here, not at the first sign-in), pruned of the
        // expired session, and the live one kept.
        assert!(text.starts_with("# pgokf-web live sessions"), "{text}");
        assert!(!text.contains("gone:"), "expired on boot: {text}");
        assert!(store.is_live("keep", "bob", 100));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_store_that_does_not_parse_refuses_to_open() {
        // Arrange
        let dir = temp_dir("malformed");
        let path = dir.join("sessions");
        std::fs::write(&path, "n1:alice\n").expect("seed");

        // Act
        let outcome = SessionStore::open(&path);

        // Assert: fail closed, naming the line, rather than a half-read set.
        let error = outcome.expect_err("refused").to_string();
        assert!(error.contains("line 1"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_is_private_and_every_line_is_checked() {
        // Arrange
        let (dir, store) = temp_store("format");
        store.add("n1", "alice", 5_000, 100).expect("adds");
        let path = dir.join("sessions");

        // Act
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777
        };
        let bad_nonce = store.add("a:b", "alice", 5_000, 100);
        let bad_subject = store.add("n2", "al ice\n", 5_000, 100);
        let malformed = SessionStore::parse("n1:alice\n");
        let bad_expiry = SessionStore::parse("n1:alice:soon\n");
        let comments = SessionStore::parse("# a comment\n\nn1:alice:5000\n").expect("parses");

        // Assert
        #[cfg(unix)]
        assert_eq!(mode, 0o600, "the store is private to the process user");
        assert!(bad_nonce.is_err());
        assert!(bad_subject.is_err());
        assert!(malformed.is_err());
        assert!(bad_expiry.is_err());
        assert_eq!(comments.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
