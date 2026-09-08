// SPDX-License-Identifier: AGPL-3.0-only
//! The sessions this site has issued and not yet ended, so a session can be
//! *ended* - by its owner signing out, by "sign out everywhere", or by an
//! admin - rather than merely left to expire.
//!
//! A session cookie is signed, so on its own the server can only *verify*
//! one; it has no way to *forget* one, and two validly signed cookies look
//! alike. This store is the server's memory of which sessions are live: a
//! cookie that verifies but whose nonce is not here is refused, so ending a
//! session ends every copy of it at once. It is a lever, not a detector: a
//! copied cookie works until its session is ended or expires.
//!
//! The memory is a table in the catalog, `pgokf_web.sessions`, reached
//! through the UI's identity connection (the table is granted to
//! `pgokf_writer` only). That is what makes ending a session one `DELETE` -
//! transactional, shared by every UI instance, and dumped with the catalog -
//! rather than a file to race or half-write. Expiry is judged by the
//! catalog's clock. A failure to consult the table is an error the caller
//! surfaces as such (a 503), never a session quietly admitted or refused.

use anyhow::{Context, Result};

use crate::db::Db;

/// Where issued sessions are remembered (see the module doc).
pub(crate) enum SessionStore {
    /// The catalog, through the identity connection.
    Pg(Db),
    /// In memory, for tests of the session seam.
    #[cfg(test)]
    Memory(std::sync::Mutex<std::collections::HashMap<String, MemorySession>>),
}

/// One live session, as the in-memory test store keeps it.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct MemorySession {
    pub subject: String,
    pub mode: String,
    pub expires: u64,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pg(_) => f.write_str("SessionStore::Pg"),
            #[cfg(test)]
            Self::Memory(_) => f.write_str("SessionStore::Memory"),
        }
    }
}

impl SessionStore {
    /// Whether the session `nonce` was issued to `subject` under `mode` and
    /// has not been ended or expired.
    ///
    /// # Errors
    ///
    /// The catalog cannot be consulted - which the caller must surface,
    /// since neither admitting nor refusing on a guess is right.
    pub(crate) async fn is_live(&self, nonce: &str, subject: &str, mode: &str) -> Result<bool> {
        match self {
            Self::Pg(db) => {
                let row = db
                    .query_one(
                        "SELECT EXISTS (
                             SELECT 1 FROM pgokf_web.sessions
                             WHERE nonce = $1 AND subject = $2 AND mode = $3
                               AND expires_at > now()
                         )",
                        &[&nonce, &subject, &mode],
                    )
                    .await
                    .context("looking a session up")?;
                Ok(row.try_get(0)?)
            }
            #[cfg(test)]
            Self::Memory(live) => Ok(live.lock().is_ok_and(|live| {
                live.get(nonce).is_some_and(|s| {
                    s.subject == subject && s.mode == mode && s.expires > crate::auth::now_unix()
                })
            })),
        }
    }

    /// Record a newly issued session, expiring at `expires` (Unix seconds).
    /// Sessions that have expired are pruned on the way, so the table never
    /// grows past what is live.
    ///
    /// # Errors
    ///
    /// The catalog cannot be written.
    pub(crate) async fn add(
        &self,
        nonce: &str,
        subject: &str,
        mode: &str,
        expires: u64,
    ) -> Result<()> {
        match self {
            Self::Pg(db) => {
                let expires_at = i64::try_from(expires).context("a session expiry past i64")?;
                db.execute(
                    "DELETE FROM pgokf_web.sessions WHERE expires_at < now()",
                    &[],
                )
                .await
                .context("pruning expired sessions")?;
                db.execute(
                    "INSERT INTO pgokf_web.sessions (nonce, subject, mode, expires_at)
                     VALUES ($1, $2, $3, to_timestamp($4::bigint))",
                    &[&nonce, &subject, &mode, &expires_at],
                )
                .await
                .context("recording a session")?;
                Ok(())
            }
            #[cfg(test)]
            Self::Memory(live) => {
                let now = crate::auth::now_unix();
                let mut live = live.lock().expect("session lock");
                live.retain(|_, s| s.expires > now);
                live.insert(
                    nonce.to_owned(),
                    MemorySession {
                        subject: subject.to_owned(),
                        mode: mode.to_owned(),
                        expires,
                    },
                );
                Ok(())
            }
        }
    }

    /// End one session. Ending one that is already gone is not an error.
    ///
    /// # Errors
    ///
    /// The catalog cannot be written.
    pub(crate) async fn remove(&self, nonce: &str) -> Result<()> {
        match self {
            Self::Pg(db) => {
                db.execute("DELETE FROM pgokf_web.sessions WHERE nonce = $1", &[&nonce])
                    .await
                    .context("ending a session")?;
                Ok(())
            }
            #[cfg(test)]
            Self::Memory(live) => {
                live.lock().expect("session lock").remove(nonce);
                Ok(())
            }
        }
    }

    /// End every session of `subject`: "sign out everywhere", an admin
    /// ending someone's sessions, a changed password, a removed person.
    ///
    /// # Errors
    ///
    /// The catalog cannot be written.
    pub(crate) async fn remove_all_for(&self, subject: &str) -> Result<()> {
        match self {
            Self::Pg(db) => {
                db.execute(
                    "DELETE FROM pgokf_web.sessions WHERE subject = $1",
                    &[&subject],
                )
                .await
                .context("ending every session of a person")?;
                Ok(())
            }
            #[cfg(test)]
            Self::Memory(live) => {
                live.lock()
                    .expect("session lock")
                    .retain(|_, s| s.subject != subject);
                Ok(())
            }
        }
    }

    /// Forget every session `mode` opened.
    ///
    /// # Errors
    ///
    /// The catalog cannot be written.
    pub(crate) async fn remove_mode(&self, mode: &str) -> Result<()> {
        match self {
            Self::Pg(db) => {
                db.execute("DELETE FROM pgokf_web.sessions WHERE mode = $1", &[&mode])
                    .await
                    .context("ending every session a mode opened")?;
                Ok(())
            }
            #[cfg(test)]
            Self::Memory(live) => {
                live.lock()
                    .expect("session lock")
                    .retain(|_, s| s.mode != mode);
                Ok(())
            }
        }
    }

    /// How many live sessions `subject` holds.
    ///
    /// # Errors
    ///
    /// The catalog cannot be read.
    pub(crate) async fn count_for(&self, subject: &str) -> Result<usize> {
        match self {
            Self::Pg(db) => {
                let row = db
                    .query_one(
                        "SELECT count(*) FROM pgokf_web.sessions
                         WHERE subject = $1 AND expires_at > now()",
                        &[&subject],
                    )
                    .await
                    .context("counting a person's sessions")?;
                let count: i64 = row.try_get(0)?;
                Ok(usize::try_from(count).unwrap_or(0))
            }
            #[cfg(test)]
            Self::Memory(live) => {
                let now = crate::auth::now_unix();
                Ok(live.lock().map_or(0, |live| {
                    live.values()
                        .filter(|s| s.subject == subject && s.expires > now)
                        .count()
                }))
            }
        }
    }

    /// Everyone holding a live session, with how many, sorted by subject -
    /// so an admin can see whom there is to sign out even where no users
    /// table lists people (`oidc` mode).
    ///
    /// # Errors
    ///
    /// The catalog cannot be read.
    pub(crate) async fn subjects(&self) -> Result<Vec<(String, usize)>> {
        match self {
            Self::Pg(db) => {
                let rows = db
                    .query(
                        "SELECT subject, count(*) FROM pgokf_web.sessions
                         WHERE expires_at > now()
                         GROUP BY subject ORDER BY subject",
                        &[],
                    )
                    .await
                    .context("listing live sessions")?;
                rows.iter()
                    .map(|row| {
                        let subject: String = row.try_get(0)?;
                        let count: i64 = row.try_get(1)?;
                        Ok((subject, usize::try_from(count).unwrap_or(0)))
                    })
                    .collect()
            }
            #[cfg(test)]
            Self::Memory(live) => {
                let now = crate::auth::now_unix();
                let mut counts: std::collections::HashMap<String, usize> =
                    std::collections::HashMap::new();
                if let Ok(live) = live.lock() {
                    for session in live.values().filter(|s| s.expires > now) {
                        *counts.entry(session.subject.clone()).or_insert(0) += 1;
                    }
                }
                let mut subjects: Vec<(String, usize)> = counts.into_iter().collect();
                subjects.sort();
                Ok(subjects)
            }
        }
    }
}

#[cfg(test)]
impl SessionStore {
    /// An empty in-memory store.
    pub(crate) fn memory() -> Self {
        Self::Memory(std::sync::Mutex::new(std::collections::HashMap::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An expiry safely in the future by the real clock.
    const LATER: u64 = 9_999_999_999;
    /// An expiry safely in the past by the real clock.
    const EARLIER: u64 = 1;

    #[tokio::test]
    async fn a_session_is_live_only_between_being_added_and_being_ended() {
        // Arrange
        let store = SessionStore::memory();

        // Act
        let before = store.is_live("n1", "alice", "users").await.expect("asks");
        store
            .add("n1", "alice", "users", LATER)
            .await
            .expect("adds");
        let after_add = store.is_live("n1", "alice", "users").await.expect("asks");
        let wrong_subject = store.is_live("n1", "bob", "users").await.expect("asks");
        let wrong_mode = store.is_live("n1", "alice", "oidc").await.expect("asks");
        store.remove("n1").await.expect("ends");
        let after_end = store.is_live("n1", "alice", "users").await.expect("asks");

        // Assert
        assert!(!before, "unknown until added");
        assert!(after_add);
        assert!(!wrong_subject, "a nonce belongs to one subject");
        assert!(!wrong_mode, "and to the mode that opened it");
        assert!(!after_end, "ended sessions are refused");
    }

    #[tokio::test]
    async fn ending_everything_for_one_person_leaves_the_others() {
        // Arrange: alice on two devices, bob on one.
        let store = SessionStore::memory();
        for (nonce, subject) in [("a1", "alice"), ("a2", "alice"), ("b1", "bob")] {
            store
                .add(nonce, subject, "users", LATER)
                .await
                .expect("adds");
        }

        // Act
        store.remove_all_for("alice").await.expect("ends");

        // Assert
        assert!(!store.is_live("a1", "alice", "users").await.expect("asks"));
        assert!(!store.is_live("a2", "alice", "users").await.expect("asks"));
        assert!(
            store.is_live("b1", "bob", "users").await.expect("asks"),
            "bob is untouched"
        );
        assert_eq!(store.count_for("alice").await.expect("counts"), 0);
        assert_eq!(store.count_for("bob").await.expect("counts"), 1);
        assert_eq!(
            store.subjects().await.expect("lists"),
            vec![("bob".to_owned(), 1)]
        );
    }

    #[tokio::test]
    async fn expired_sessions_are_refused_and_pruned_on_the_next_add() {
        // Arrange: a session whose expiry has already passed.
        let store = SessionStore::memory();
        store
            .add("old", "alice", "users", EARLIER)
            .await
            .expect("adds");

        // Act
        let stale = store.is_live("old", "alice", "users").await.expect("asks");
        store
            .add("new", "carol", "users", LATER)
            .await
            .expect("adds");
        let subjects = store.subjects().await.expect("lists");

        // Assert
        assert!(!stale, "past its expiry it is refused");
        assert_eq!(
            subjects,
            vec![("carol".to_owned(), 1)],
            "the expired one is gone"
        );
    }
}
