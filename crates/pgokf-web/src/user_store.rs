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

use crate::auth::{Role, UserRecord};
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
                    let hash: String = row.try_get(1)?;
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
    pub(crate) async fn list(&self) -> Result<Vec<(String, Role)>> {
        match self {
            Self::Pg(db) => {
                let rows = db
                    .query("SELECT name, role FROM pgokf_web.users ORDER BY name", &[])
                    .await
                    .context("listing people")?;
                rows.iter()
                    .map(|row| {
                        let name: String = row.try_get(0)?;
                        let role: String = row.try_get(1)?;
                        let role = Role::parse(&role)
                            .ok_or_else(|| anyhow!("the catalog holds an unknown role {role:?}"))?;
                        Ok((name, role))
                    })
                    .collect()
            }
            #[cfg(test)]
            Self::Memory(users) => {
                let mut list: Vec<(String, Role)> = users
                    .lock()
                    .map(|u| u.iter().map(|(n, r)| (n.clone(), r.role)).collect())
                    .unwrap_or_default();
                list.sort();
                Ok(list)
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
                        hash: hash.to_owned(),
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

    /// Replace a person's password hash.
    ///
    /// # Errors
    ///
    /// No such person, or the catalog cannot be written.
    pub(crate) async fn set_hash(&self, name: &str, hash: &str) -> Result<()> {
        match self {
            Self::Pg(db) => {
                let changed = db
                    .execute(
                        "UPDATE pgokf_web.users SET password_hash = $2, updated_at = now()
                         WHERE name = $1",
                        &[&name, &hash],
                    )
                    .await
                    .map_err(|error| without_row_detail(error, "changing a password"))?;
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
                record.hash = hash.to_owned();
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
