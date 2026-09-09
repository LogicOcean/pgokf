// SPDX-License-Identifier: AGPL-3.0-only
//! The people the `users` identity mode signs in, kept in the catalog
//! (`pgokf_web.users`) and reached through the UI's identity connection -
//! the table is granted to `pgokf_writer` only, so a reader never sees a
//! password hash.
//!
//! Every request looks a person up afresh (the session cookie carries no
//! role), so adding, demoting, or removing someone takes effect at once and
//! every UI instance agrees. Mutations are single statements: there is no
//! file to rewrite, lock, or half-write. A failure to reach the table is an
//! error the caller surfaces, never a wrong password.

#[cfg(test)]
use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use tokio_postgres::error::SqlState;

use crate::auth::{Role, UserRecord, UserSummary};
use crate::db::{Db, db_message, sql_state};

/// Where people are kept (see the module doc).
pub(crate) enum UserStore {
    /// The catalog, through the identity connection.
    Pg(Db),
    /// In memory, for tests of the sign-in seam.
    #[cfg(test)]
    Memory(std::sync::Mutex<HashMap<String, UserRecord>>),
}

impl std::fmt::Debug for UserStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pg(_) => f.write_str("UserStore::Pg"),
            #[cfg(test)]
            Self::Memory(_) => f.write_str("UserStore::Memory"),
        }
    }
}

impl UserStore {
    /// One person's role and password hash, or `None` for an unknown name.
    ///
    /// # Errors
    ///
    /// The catalog cannot be read, or holds a role this build does not know.
    pub(crate) async fn record(&self, name: &str) -> Result<Option<UserRecord>> {
        match self {
            Self::Pg(db) => {
                let row = db
                    .query_opt(
                        "SELECT role, password_hash FROM pgokf_web.users WHERE name = $1",
                        &[&name],
                    )
                    .await
                    .context("looking a person up")?;
                row.map(|row| {
                    let role: String = row.try_get(0)?;
                    let hash: Option<String> = row.try_get(1)?;
                    Ok(UserRecord {
                        role: Role::parse(&role)
                            .ok_or_else(|| anyhow!("the catalog holds an unknown role {role:?}"))?,
                        hash,
                    })
                })
                .transpose()
            }
            #[cfg(test)]
            Self::Memory(users) => Ok(users.lock().ok().and_then(|u| u.get(name).cloned())),
        }
    }

    /// Everyone with their role, sorted by name (for the admin page).
    ///
    /// # Errors
    ///
    /// The catalog cannot be read.
    pub(crate) async fn list(&self) -> Result<Vec<UserSummary>> {
        match self {
            Self::Pg(db) => {
                let rows = db
                    .query(
                        "SELECT name, role, password_hash IS NOT NULL
                         FROM pgokf_web.users ORDER BY name",
                        &[],
                    )
                    .await
                    .context("listing people")?;
                rows.iter()
                    .map(|row| {
                        let name: String = row.try_get(0)?;
                        let role: String = row.try_get(1)?;
                        let role = Role::parse(&role)
                            .ok_or_else(|| anyhow!("the catalog holds an unknown role {role:?}"))?;
                        Ok(UserSummary {
                            name,
                            role,
                            has_password: row.try_get(2)?,
                        })
                    })
                    .collect()
            }
            #[cfg(test)]
            Self::Memory(users) => {
                let mut list: Vec<UserSummary> = users
                    .lock()
                    .map(|u| {
                        u.iter()
                            .map(|(n, r)| UserSummary {
                                name: n.clone(),
                                role: r.role,
                                has_password: r.hash.is_some(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                list.sort_by(|a, b| a.name.cmp(&b.name));
                Ok(list)
            }
        }
    }

    /// Give a person the identity provider signed in a row of their own, so
    /// an admin sees them and can set their role: `false` when a row with
    /// that name exists already (theirs from an earlier sign-in, or a
    /// password person's of the same name), which is left as it is.
    ///
    /// # Errors
    ///
    /// The catalog cannot be written.
    pub(crate) async fn provision(&self, name: &str, role: Role) -> Result<bool> {
        match self {
            Self::Pg(db) => Ok(db
                .execute(
                    "INSERT INTO pgokf_web.users (name, role, password_hash)
                     VALUES ($1, $2, NULL) ON CONFLICT (name) DO NOTHING",
                    &[&name, &role.id()],
                )
                .await
                .context("recording a person the provider signed in")?
                > 0),
            #[cfg(test)]
            Self::Memory(users) => {
                let mut users = users.lock().expect("users lock");
                if users.contains_key(name) {
                    return Ok(false);
                }
                users.insert(name.to_owned(), UserRecord { role, hash: None });
                Ok(true)
            }
        }
    }

    /// Add a person. A name already taken is an error.
    ///
    /// # Errors
    ///
    /// The name exists, or the catalog cannot be written.
    pub(crate) async fn insert(&self, name: &str, role: Role, hash: &str) -> Result<()> {
        match self {
            Self::Pg(db) => {
                let outcome = db
                    .execute(
                        "INSERT INTO pgokf_web.users (name, role, password_hash)
                         VALUES ($1, $2, $3)",
                        &[&name, &role.id(), &hash],
                    )
                    .await;
                match outcome {
                    Ok(_) => Ok(()),
                    Err(error) if sql_state(&error) == Some(SqlState::UNIQUE_VIOLATION) => {
                        bail!("{name} already exists")
                    }
                    Err(error) => Err(without_row_detail(error, "adding a person")),
                }
            }
            #[cfg(test)]
            Self::Memory(users) => {
                let mut users = users.lock().expect("users lock");
                if users.contains_key(name) {
                    bail!("{name} already exists");
                }
                users.insert(
                    name.to_owned(),
                    UserRecord {
                        role,
                        hash: Some(hash.to_owned()),
                    },
                );
                Ok(())
            }
        }
    }

    /// Change a person's role.
    ///
    /// # Errors
    ///
    /// No such person, or the catalog cannot be written.
    pub(crate) async fn set_role(&self, name: &str, role: Role) -> Result<()> {
        match self {
            Self::Pg(db) => {
                let changed = db
                    .execute(
                        "UPDATE pgokf_web.users SET role = $2, updated_at = now() WHERE name = $1",
                        &[&name, &role.id()],
                    )
                    .await
                    .context("changing a role")?;
                if changed == 0 {
                    bail!("{name} is not a user");
                }
                Ok(())
            }
            #[cfg(test)]
            Self::Memory(users) => {
                let mut users = users.lock().expect("users lock");
                let record = users
                    .get_mut(name)
                    .with_context(|| format!("{name} is not a user"))?;
                record.role = role;
                Ok(())
            }
        }
    }

    /// Replace a person's password hash. Only a password person's: one the
    /// identity provider brought has none and gets none here, in the same
    /// statement that would set it, so no sign-in can slip a row in between.
    ///
    /// # Errors
    ///
    /// No such person, a person without a password, or the catalog cannot
    /// be written.
    pub(crate) async fn set_hash(&self, name: &str, hash: &str) -> Result<()> {
        match self {
            Self::Pg(db) => {
                let changed = db
                    .execute(
                        "UPDATE pgokf_web.users SET password_hash = $2, updated_at = now()
                         WHERE name = $1 AND password_hash IS NOT NULL",
                        &[&name, &hash],
                    )
                    .await
                    .map_err(|error| without_row_detail(error, "changing a password"))?;
                if changed == 0 {
                    let row = db
                        .query_one(
                            "SELECT EXISTS (SELECT 1 FROM pgokf_web.users WHERE name = $1)",
                            &[&name],
                        )
                        .await
                        .context("checking a person")?;
                    let exists: bool = row.try_get(0)?;
                    if exists {
                        bail!("{}", no_password_here(name));
                    }
                    bail!("{name} is not a user");
                }
                Ok(())
            }
            #[cfg(test)]
            Self::Memory(users) => {
                let mut users = users.lock().expect("users lock");
                let record = users
                    .get_mut(name)
                    .with_context(|| format!("{name} is not a user"))?;
                if record.hash.is_none() {
                    bail!("{}", no_password_here(name));
                }
                record.hash = Some(hash.to_owned());
                Ok(())
            }
        }
    }

    /// Remove a person. The last person cannot be removed, so the site is
    /// not left with nobody able to sign in.
    ///
    /// # Errors
    ///
    /// No such person, they are the last one, or the catalog cannot be
    /// written.
    pub(crate) async fn remove(&self, name: &str) -> Result<()> {
        match self {
            Self::Pg(db) => {
                // The row goes only if it exists and is not the last. Two
                // admins removing the last two people at the same instant can
                // each see a count of two and empty the table (read
                // committed); that needs two concurrent admins, and
                // `pgokf-web user add` is the way back, so it is accepted
                // rather than serialised.
                let removed = db
                    .execute(
                        "DELETE FROM pgokf_web.users
                         WHERE name = $1
                           AND (SELECT count(*) FROM pgokf_web.users) > 1",
                        &[&name],
                    )
                    .await
                    .context("removing a person")?;
                if removed == 0 {
                    let row = db
                        .query_one(
                            "SELECT EXISTS (SELECT 1 FROM pgokf_web.users WHERE name = $1)",
                            &[&name],
                        )
                        .await
                        .context("checking a person")?;
                    let exists: bool = row.try_get(0)?;
                    if exists {
                        bail!("the last user cannot be removed");
                    }
                    bail!("{name} is not a user");
                }
                Ok(())
            }
            #[cfg(test)]
            Self::Memory(users) => {
                let mut users = users.lock().expect("users lock");
                if !users.contains_key(name) {
                    bail!("{name} is not a user");
                }
                if users.len() == 1 {
                    bail!("the last user cannot be removed");
                }
                users.remove(name);
                Ok(())
            }
        }
    }
}

/// Why a person the identity provider brought cannot be given a password.
fn no_password_here(name: &str) -> String {
    format!(
        "{name} signs in through the identity provider and has no password here; to give them \
         one, remove them and add them again"
    )
}

/// A statement that carried a password hash failed. A `PostgreSQL` error's
/// full text can quote the failing row - the hash included - so for these
/// two statements only the `SQLSTATE` and the server's one-line message are
/// kept; anything that is not a server error (a pool timeout) keeps its
/// chain, so it is still classified as "busy" for the caller.
fn without_row_detail(error: anyhow::Error, what: &str) -> anyhow::Error {
    match sql_state(&error) {
        Some(state) => anyhow!(
            "{what} failed ({}: {})",
            state.code(),
            db_message(&error).unwrap_or_default()
        ),
        None => error.context(what.to_owned()),
    }
}

#[cfg(test)]
impl UserStore {
    /// An in-memory store holding `people` (name, role, password hash).
    pub(crate) fn memory(people: impl IntoIterator<Item = (String, UserRecord)>) -> Self {
        Self::Memory(std::sync::Mutex::new(people.into_iter().collect()))
    }
}
