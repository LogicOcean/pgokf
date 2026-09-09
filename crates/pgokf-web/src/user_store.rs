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
//!
//! A name belongs to exactly one way in: a password, or one identity
//! provider (the table's own constraint says so too). The statements here
//! keep it that way without a read before the write.

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

/// Which people the Admin page's People tab asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeopleQuery {
    /// A substring of the name or display name, case aside; empty for
    /// everyone.
    pub search: String,
    pub offset: usize,
    pub limit: usize,
}

/// One page of people, and how many there are in all that the query
/// matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeoplePage {
    pub people: Vec<UserSummary>,
    pub total: usize,
}

/// The columns [`UserRecord`] is read from.
const RECORD_COLUMNS: &str = "role, password_hash, display_name, provider";

impl UserStore {
    /// One person's record, or `None` for an unknown name.
    ///
    /// # Errors
    ///
    /// The catalog cannot be read, or holds a role this build does not know.
    pub(crate) async fn record(&self, name: &str) -> Result<Option<UserRecord>> {
        match self {
            Self::Pg(db) => {
                let row = db
                    .query_opt(
                        &format!("SELECT {RECORD_COLUMNS} FROM pgokf_web.users WHERE name = $1"),
                        &[&name],
                    )
                    .await
                    .context("looking a person up")?;
                row.map(|row| {
                    let role: String = row.try_get(0)?;
                    Ok(UserRecord {
                        role: parse_role(&role)?,
                        hash: row.try_get(1)?,
                        display: row.try_get(2)?,
                        provider: row.try_get(3)?,
                    })
                })
                .transpose()
            }
            #[cfg(test)]
            Self::Memory(users) => Ok(users.lock().ok().and_then(|u| u.get(name).cloned())),
        }
    }

    /// A page of people matching `query`, sorted by name, with the total.
    ///
    /// # Errors
    ///
    /// The catalog cannot be read.
    pub(crate) async fn people(&self, query: &PeopleQuery) -> Result<PeoplePage> {
        match self {
            Self::Pg(db) => {
                let pattern = like_pattern(&query.search);
                let limit = i64::try_from(query.limit).unwrap_or(i64::MAX);
                let offset = i64::try_from(query.offset).unwrap_or(i64::MAX);
                let rows = db
                    .query(
                        "SELECT name, role, display_name, provider, count(*) OVER ()
                         FROM pgokf_web.users
                         WHERE $1 = ''
                            OR name ILIKE $2 ESCAPE '\\'
                            OR display_name ILIKE $2 ESCAPE '\\'
                         ORDER BY name
                         LIMIT $3 OFFSET $4",
                        &[&query.search, &pattern, &limit, &offset],
                    )
                    .await
                    .context("listing people")?;
                let total = if let Some(row) = rows.first() {
                    usize::try_from(row.try_get::<_, i64>(4)?).unwrap_or(0)
                } else {
                    // Past the last page: how many there are takes one more
                    // question.
                    let row = db
                        .query_one(
                            "SELECT count(*) FROM pgokf_web.users
                             WHERE $1 = ''
                                OR name ILIKE $2 ESCAPE '\\'
                                OR display_name ILIKE $2 ESCAPE '\\'",
                            &[&query.search, &pattern],
                        )
                        .await
                        .context("counting people")?;
                    usize::try_from(row.try_get::<_, i64>(0)?).unwrap_or(0)
                };
                let people = rows
                    .iter()
                    .map(|row| {
                        let role: String = row.try_get(1)?;
                        Ok(UserSummary {
                            name: row.try_get(0)?,
                            role: parse_role(&role)?,
                            display: row.try_get(2)?,
                            provider: row.try_get(3)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(PeoplePage { people, total })
            }
            #[cfg(test)]
            Self::Memory(users) => {
                let needle = query.search.to_lowercase();
                let mut all: Vec<UserSummary> = users
                    .lock()
                    .map(|u| {
                        u.iter()
                            .filter(|(name, record)| {
                                needle.is_empty()
                                    || name.to_lowercase().contains(&needle)
                                    || record
                                        .display
                                        .as_deref()
                                        .is_some_and(|d| d.to_lowercase().contains(&needle))
                            })
                            .map(|(name, record)| UserSummary {
                                name: name.clone(),
                                role: record.role,
                                display: record.display.clone(),
                                provider: record.provider.clone(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                all.sort_by(|a, b| a.name.cmp(&b.name));
                let total = all.len();
                let people = all
                    .into_iter()
                    .skip(query.offset)
                    .take(query.limit)
                    .collect();
                Ok(PeoplePage { people, total })
            }
        }
    }

    /// Give a person an identity provider signed in a row of their own, so
    /// an admin sees them and can set their role: `false` when a row with
    /// that name exists already (theirs from an earlier sign-in, or somebody
    /// else's), which is left as it is.
    ///
    /// # Errors
    ///
    /// The catalog cannot be written.
    pub(crate) async fn provision(
        &self,
        name: &str,
        role: Role,
        display: Option<&str>,
        provider: &str,
    ) -> Result<bool> {
        match self {
            Self::Pg(db) => Ok(db
                .execute(
                    "INSERT INTO pgokf_web.users (name, role, password_hash, display_name, provider)
                     VALUES ($1, $2, NULL, $3, $4) ON CONFLICT (name) DO NOTHING",
                    &[&name, &role.id(), &display, &provider],
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
                users.insert(
                    name.to_owned(),
                    UserRecord {
                        role,
                        hash: None,
                        display: display.map(str::to_owned),
                        provider: Some(provider.to_owned()),
                    },
                );
                Ok(true)
            }
        }
    }

    /// Keep what a provider calls a person current: their display name, as
    /// the provider reports it at this sign-in. Only that provider's own
    /// row changes, and only when the name differs.
    ///
    /// # Errors
    ///
    /// The catalog cannot be written.
    pub(crate) async fn refresh_display(
        &self,
        name: &str,
        provider: &str,
        display: Option<&str>,
    ) -> Result<()> {
        match self {
            Self::Pg(db) => {
                db.execute(
                    "UPDATE pgokf_web.users SET display_name = $3, updated_at = now()
                     WHERE name = $1 AND provider = $2 AND display_name IS DISTINCT FROM $3",
                    &[&name, &provider, &display],
                )
                .await
                .context("keeping a person's name current")?;
                Ok(())
            }
            #[cfg(test)]
            Self::Memory(users) => {
                if let Some(record) = users
                    .lock()
                    .expect("users lock")
                    .get_mut(name)
                    .filter(|record| record.provider.as_deref() == Some(provider))
                {
                    record.display = display.map(str::to_owned);
                }
                Ok(())
            }
        }
    }

    /// Put every person `provider` brought back at the bottom of the
    /// ladder - when the provider is re-registered as another, so a role an
    /// admin granted the old provider's people is not lent to whoever the
    /// new one calls by the same names. How many rows changed comes back.
    ///
    /// # Errors
    ///
    /// The catalog cannot be written.
    pub(crate) async fn demote_people_of(&self, provider: &str) -> Result<u64> {
        match self {
            Self::Pg(db) => db
                .execute(
                    "UPDATE pgokf_web.users SET role = $2, updated_at = now()
                     WHERE provider = $1 AND role <> $2",
                    &[&provider, &Role::Viewer.id()],
                )
                .await
                .context("demoting the people of a provider"),
            #[cfg(test)]
            Self::Memory(users) => {
                let mut changed = 0;
                for record in
                    users.lock().expect("users lock").values_mut().filter(|r| {
                        r.provider.as_deref() == Some(provider) && r.role != Role::Viewer
                    })
                {
                    record.role = Role::Viewer;
                    changed += 1;
                }
                Ok(changed)
            }
        }
    }

    /// Add a person who signs in with a password. A name already taken is
    /// an error.
    ///
    /// # Errors
    ///
    /// The name exists, or the catalog cannot be written.
    pub(crate) async fn insert(
        &self,
        name: &str,
        role: Role,
        hash: &str,
        display: Option<&str>,
    ) -> Result<()> {
        match self {
            Self::Pg(db) => {
                let outcome = db
                    .execute(
                        "INSERT INTO pgokf_web.users (name, role, password_hash, display_name)
                         VALUES ($1, $2, $3, $4)",
                        &[&name, &role.id(), &hash, &display],
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
                        display: display.map(str::to_owned),
                        provider: None,
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

    /// Replace a person's password hash. Only a password person's: one an
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

fn parse_role(role: &str) -> Result<Role> {
    Role::parse(role).ok_or_else(|| anyhow!("the catalog holds an unknown role {role:?}"))
}

/// `search` as a `LIKE` pattern matching it anywhere, with its own
/// wildcards escaped so a person can search for an underscore.
fn like_pattern(search: &str) -> String {
    let mut pattern = String::with_capacity(search.len() + 2);
    pattern.push('%');
    for c in search.chars() {
        if matches!(c, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    pattern
}

/// Why a person an identity provider brought cannot be given a password.
fn no_password_here(name: &str) -> String {
    format!(
        "{name} signs in through an identity provider and has no password here; to give them \
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
    /// An in-memory store holding `people` (name, record).
    pub(crate) fn memory(people: impl IntoIterator<Item = (String, UserRecord)>) -> Self {
        Self::Memory(std::sync::Mutex::new(people.into_iter().collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_is_matched_anywhere_with_its_wildcards_escaped() {
        // Arrange
        let plain = "ali";
        let tricky = "a_b%c\\d";

        // Act + Assert
        assert_eq!(like_pattern(plain), "%ali%");
        assert_eq!(like_pattern(tricky), "%a\\_b\\%c\\\\d%");
        assert_eq!(like_pattern(""), "%%");
    }

    #[tokio::test]
    async fn people_are_paged_and_searched_by_name_or_display_name() {
        // Arrange
        let store = UserStore::memory([
            (
                "alice".to_owned(),
                UserRecord::with_password(Role::Editor, "h"),
            ),
            (
                "bob".to_owned(),
                UserRecord::with_password(Role::Viewer, "h"),
            ),
            (
                "583231".to_owned(),
                UserRecord::from_provider(Role::Viewer, "github", Some("Alicia Octocat")),
            ),
        ]);

        // Act
        let first = store
            .people(&PeopleQuery {
                search: String::new(),
                offset: 0,
                limit: 2,
            })
            .await
            .expect("lists");
        let second = store
            .people(&PeopleQuery {
                search: String::new(),
                offset: 2,
                limit: 2,
            })
            .await
            .expect("lists");
        let searched = store
            .people(&PeopleQuery {
                search: "ALI".to_owned(),
                offset: 0,
                limit: 10,
            })
            .await
            .expect("lists");

        // Assert
        assert_eq!(first.total, 3);
        assert_eq!(
            first
                .people
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["583231", "alice"],
            "sorted by name, two to a page"
        );
        assert_eq!(
            second
                .people
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["bob"]
        );
        assert_eq!(
            searched.total, 2,
            "alice by name, the octocat by display name"
        );
        assert!(searched.people.iter().all(|p| p.name != "bob"));
    }
}
