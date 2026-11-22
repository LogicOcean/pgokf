// SPDX-License-Identifier: AGPL-3.0-only
//! The pooled connections the long-running companions share.
//!
//! A server process that holds one connection for its whole life has to stop
//! when the link drops: an idle-timeout proxy, a database restart or a
//! failover all close it, and nothing would open it again. A [`Pool`] opens
//! connections as they are needed, sets each one up from a [`Session`]
//! before handing it out, and replaces one the server has closed instead of
//! handing it out again, so a dropped link costs one reconnect, not a
//! process restart.

use std::fmt;
use std::ops::DerefMut as _;
use std::time::Duration;

use anyhow::{Context, Result};
use deadpool_postgres::{
    Hook, HookError, Manager, ManagerConfig, Object, RecyclingMethod, Runtime,
};
use tokio_postgres::{Client, IsolationLevel, NoTls, Transaction};

/// How every connection a [`Pool`] opens is set up before it is handed out:
/// the tenant it is scoped to and the bounds on its statements and lock
/// waits. A setting left unset keeps the server's default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Session {
    tenant: Option<String>,
    statement_timeout: Option<Duration>,
    lock_timeout: Option<Duration>,
}

impl Session {
    /// Scope the session to one pgokf tenant (see [`crate::set_tenant`]).
    #[must_use]
    pub fn tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = Some(tenant.into());
        self
    }

    /// Bound every statement the session runs. Cancelling a request's future
    /// does not cancel its query on the server; this does.
    #[must_use]
    pub fn statement_timeout(mut self, timeout: Duration) -> Self {
        self.statement_timeout = Some(timeout);
        self
    }

    /// Bound how long any statement waits for a lock.
    #[must_use]
    pub fn lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = Some(timeout);
        self
    }

    /// Apply the settings to `client` for the rest of its session.
    ///
    /// # Errors
    ///
    /// Returns an error if any `set_config` call fails.
    pub async fn apply(&self, client: &Client) -> Result<()> {
        if let Some(timeout) = self.statement_timeout {
            set_milliseconds(client, "statement_timeout", timeout).await?;
        }
        if let Some(timeout) = self.lock_timeout {
            set_milliseconds(client, "lock_timeout", timeout).await?;
        }
        if let Some(tenant) = &self.tenant {
            crate::set_tenant(client, tenant).await?;
        }
        Ok(())
    }
}

/// Set a millisecond setting for the session. The value travels as a bound
/// parameter, never as SQL text; one beyond PostgreSQL's integer range is
/// clamped to the longest it accepts.
async fn set_milliseconds(client: &Client, setting: &str, value: Duration) -> Result<()> {
    let millis = i32::try_from(value.as_millis()).unwrap_or(i32::MAX);
    client
        .execute(
            "SELECT pg_catalog.set_config($1, $2, false)",
            &[&setting, &millis.to_string()],
        )
        .await
        .with_context(|| format!("setting {setting}"))?;
    Ok(())
}

/// What a [`Pool`] connects to and how many connections it may hold.
pub struct PoolSettings<'a> {
    /// The libpq-style connection string.
    pub database_url: &'a str,
    /// Require TLS whatever the string's `sslmode` says, as
    /// [`crate::connection_config`] does for a single connection.
    pub force_tls: bool,
    /// The most connections open at once.
    pub max_size: usize,
    /// How long a checkout waits for a free connection; `None` waits as long
    /// as it takes.
    pub wait_timeout: Option<Duration>,
    /// The setup every new connection gets before it is handed out.
    pub session: Session,
}

/// A pool of catalog connections, each set up from one [`Session`].
///
/// A checkout that finds the connection it would reuse already closed by the
/// server opens a new one instead, so a link that drops while idle costs one
/// reconnect on the next request.
#[derive(Clone)]
pub struct Pool {
    inner: deadpool_postgres::Pool,
}

impl Pool {
    /// Build the pool. No connection is opened until the first checkout.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection string cannot be parsed or the TLS
    /// trust store cannot be built.
    pub fn new(settings: PoolSettings<'_>) -> Result<Self> {
        let config = crate::connection_config(settings.database_url, settings.force_tls)?;
        let manager_config = ManagerConfig {
            // A closed connection is detected without a round trip; one the
            // network dropped silently fails its first statement instead.
            recycling_method: RecyclingMethod::Fast,
        };
        let manager = if crate::should_use_tls(config.get_ssl_mode(), settings.force_tls) {
            Manager::from_config(config, crate::rustls_connector()?, manager_config)
        } else {
            Manager::from_config(config, NoTls, manager_config)
        };
        let session = settings.session;
        let inner = deadpool_postgres::Pool::builder(manager)
            .max_size(settings.max_size)
            .wait_timeout(settings.wait_timeout)
            .runtime(Runtime::Tokio1)
            .post_create(Hook::async_fn(move |client, _| {
                let session = session.clone();
                Box::pin(async move {
                    session
                        .apply(client)
                        .await
                        .map_err(|error| HookError::message(format!("{error:#}")))
                })
            }))
            .build()
            .context("building the PostgreSQL connection pool")?;
        Ok(Self { inner })
    }

    /// Borrow a connection: a pooled one the server has not closed, or a new
    /// one set up from the pool's [`Session`].
    ///
    /// # Errors
    ///
    /// Returns an error if no connection frees up within the wait timeout, or
    /// a new connection cannot be opened or set up.
    pub async fn checkout(&self) -> Result<Borrowed> {
        let object = self
            .inner
            .get()
            .await
            .context("checking out a PostgreSQL connection")?;
        Ok(Borrowed {
            object: Some(object),
            done: false,
        })
    }
}

/// A connection borrowed from a [`Pool`] for one piece of work.
///
/// Work that completes (or fails) calls [`Borrowed::finish`] and the
/// connection goes back to the pool at once; work in a transaction runs
/// through [`Borrowed::transaction`], which does both. Work abandoned
/// mid-flight - a request that timed out, or a client that went away - drops
/// it unfinished, and the connection is taken out of the pool and closed
/// instead: closing the socket is what makes the server cancel the running
/// statement, and no later request inherits a connection in an unknown state.
pub struct Borrowed {
    object: Option<Object>,
    done: bool,
}

/// Why [`Borrowed::transaction`] could not run or keep its work: the
/// transaction did not open, or did not commit. Either way its connection
/// was closed rather than returned to the pool.
#[derive(Debug)]
pub struct TransactionError {
    stage: &'static str,
    source: tokio_postgres::Error,
}

impl fmt::Display for TransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "could not {} the transaction", self.stage)
    }
}

impl std::error::Error for TransactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Why a [`Borrowed`] always holds its connection: only `Drop` takes it out.
const PRESENT: &str = "a borrowed connection is present until it is dropped";

impl Borrowed {
    /// The client, for running statements.
    ///
    /// # Panics
    ///
    /// Never: the connection is taken out only when the guard is dropped.
    #[must_use]
    pub fn client(&self) -> &Client {
        self.object.as_ref().expect(PRESENT)
    }

    /// The client mutably, for opening a transaction. Finish only after the
    /// transaction has ended, so the connection goes back in autocommit.
    ///
    /// # Panics
    ///
    /// Never: the connection is taken out only when the guard is dropped.
    pub fn client_mut(&mut self) -> &mut Client {
        self.object.as_mut().expect(PRESENT).deref_mut()
    }

    /// The work is complete: give the connection back to the pool now. It
    /// takes the guard, so a finished connection cannot stay checked out
    /// while the same work borrows another, and a transaction opened on it
    /// has to have ended first.
    pub fn finish(mut self) {
        self.done = true;
    }

    /// Run `work` in one transaction on this connection and end it: commit
    /// once `work` succeeded, roll back once it returned an error. The
    /// connection goes back to the pool only after the transaction has ended
    /// cleanly, so an ordinary refusal costs no reconnect; if the commit or
    /// rollback fails, or the work is abandoned mid-flight, it is closed
    /// instead, which ends the transaction on the server as well.
    ///
    /// `isolation` replaces the server's default isolation level.
    ///
    /// # Errors
    ///
    /// The transaction did not open or commit ([`TransactionError`]), or
    /// `work` failed, in which case its own error is returned after the
    /// rollback.
    pub async fn transaction<T, E>(
        mut self,
        isolation: Option<IsolationLevel>,
        work: impl AsyncFnOnce(&Transaction<'_>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<TransactionError>,
    {
        let mut builder = self.client_mut().build_transaction();
        if let Some(level) = isolation {
            builder = builder.isolation_level(level);
        }
        let transaction = builder.start().await.map_err(|source| TransactionError {
            stage: "open",
            source,
        })?;
        let outcome = work(&transaction).await;
        match outcome {
            Ok(value) => {
                transaction
                    .commit()
                    .await
                    .map_err(|source| TransactionError {
                        stage: "commit",
                        source,
                    })?;
                self.finish();
                Ok(value)
            }
            Err(error) => {
                if transaction.rollback().await.is_ok() {
                    self.finish();
                }
                Err(error)
            }
        }
    }
}

impl Drop for Borrowed {
    fn drop(&mut self) {
        if !self.done
            && let Some(object) = self.object.take()
        {
            // Detach from the pool and drop the client, closing the socket.
            drop(Object::take(object));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// How long a test waits for something the server or the driver does
    /// asynchronously before it calls the wait a failure.
    const SETTLE: Duration = Duration::from_secs(5);

    /// The scratch server these tests run against: `PGOKF_PGCONN_TEST_DB`
    /// when set, else the local default. `None`, with a notice, when no
    /// server answers - the same convention as pgokf-web's scratch tests, so
    /// they run where a scratch server exists and skip where none does.
    async fn scratch_url() -> Option<String> {
        let url = std::env::var("PGOKF_PGCONN_TEST_DB").unwrap_or_else(|_| {
            let user = std::env::var("USER").unwrap_or_else(|_| "postgres".to_owned());
            format!("host=localhost dbname=postgres user={user}")
        });
        match crate::connect(&url, false).await {
            Ok(_) => Some(url),
            Err(error) => {
                eprintln!("skipping a pool test: no scratch PostgreSQL answers ({error:#})");
                None
            }
        }
    }

    fn settings(database_url: &str, max_size: usize, session: Session) -> PoolSettings<'_> {
        PoolSettings {
            database_url,
            force_tls: false,
            max_size,
            wait_timeout: Some(SETTLE),
            session,
        }
    }

    async fn backend_pid(connection: &Borrowed) -> i32 {
        connection
            .client()
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .expect("the borrowed connection answers")
            .get(0)
    }

    /// Wait until `done` holds, polling; fail the test if it never does.
    async fn wait_until(mut done: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + SETTLE;
        while !done() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the condition did not hold within {SETTLE:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_connection_the_server_closed_is_replaced_by_the_next_checkout() {
        let Some(url) = scratch_url().await else {
            return;
        };
        // Arrange: a one-connection pool whose connection the server closes,
        // as an idle-timeout proxy or a database restart does.
        let pool = Pool::new(settings(&url, 1, Session::default())).expect("build the pool");
        let first = pool.checkout().await.expect("first checkout");
        let closed_pid = backend_pid(&first).await;
        let (admin, _driver) = crate::connect(&url, false).await.expect("admin connection");
        admin
            .execute("SELECT pg_terminate_backend($1)", &[&closed_pid])
            .await
            .expect("terminate the pooled backend");
        wait_until(|| first.client().is_closed()).await;
        first.finish();

        // Act
        let second = pool.checkout().await.expect("checkout after the close");
        let pid = backend_pid(&second).await;

        // Assert
        assert_ne!(
            pid, closed_pid,
            "a fresh connection replaced the closed one"
        );
    }

    #[tokio::test]
    async fn every_connection_the_pool_opens_carries_the_session_settings() {
        let Some(url) = scratch_url().await else {
            return;
        };
        // Arrange
        let session = Session::default()
            .tenant("acme")
            .statement_timeout(Duration::from_millis(1234))
            .lock_timeout(Duration::from_millis(567));
        let pool = Pool::new(settings(&url, 2, session)).expect("build the pool");

        // Act: hold two at once, so the pool has to open two connections.
        let first = pool.checkout().await.expect("first checkout");
        let second = pool.checkout().await.expect("second checkout");

        // Assert
        assert_ne!(backend_pid(&first).await, backend_pid(&second).await);
        for connection in [&first, &second] {
            let row = connection
                .client()
                .query_one(
                    "SELECT current_setting('pgokf.tenant'), \
                     current_setting('statement_timeout'), current_setting('lock_timeout')",
                    &[],
                )
                .await
                .expect("read the session settings");
            assert_eq!(row.get::<_, String>(0), "acme");
            assert_eq!(row.get::<_, String>(1), "1234ms");
            assert_eq!(row.get::<_, String>(2), "567ms");
        }
    }

    #[tokio::test]
    async fn a_connection_left_unfinished_is_closed_rather_than_reused() {
        let Some(url) = scratch_url().await else {
            return;
        };
        // Arrange: a request that is cancelled mid-statement drops its
        // connection without finishing it.
        let pool = Pool::new(settings(&url, 1, Session::default())).expect("build the pool");
        let abandoned = pool.checkout().await.expect("first checkout");
        let abandoned_pid = backend_pid(&abandoned).await;

        // Act
        drop(abandoned);
        let next = pool.checkout().await.expect("checkout after the abandon");

        // Assert
        assert_ne!(backend_pid(&next).await, abandoned_pid);
    }

    #[tokio::test]
    async fn a_finished_connection_is_reused() {
        let Some(url) = scratch_url().await else {
            return;
        };
        // Arrange
        let pool = Pool::new(settings(&url, 1, Session::default())).expect("build the pool");
        let finished = pool.checkout().await.expect("first checkout");
        let finished_pid = backend_pid(&finished).await;

        // Act
        finished.finish();
        let next = pool.checkout().await.expect("second checkout");

        // Assert
        assert_eq!(backend_pid(&next).await, finished_pid);
    }

    /// A one-connection pool whose connection has a session-local table the
    /// transaction tests write to, and that connection's backend pid.
    async fn pool_with_scratch_table(url: &str) -> (Pool, i32) {
        let pool = Pool::new(settings(url, 1, Session::default())).expect("build the pool");
        let connection = pool.checkout().await.expect("first checkout");
        connection
            .client()
            .batch_execute("CREATE TEMPORARY TABLE written (n int)")
            .await
            .expect("create the scratch table");
        let pid = backend_pid(&connection).await;
        connection.finish();
        (pool, pid)
    }

    async fn rows_written(connection: &Borrowed) -> i64 {
        connection
            .client()
            .query_one("SELECT count(*) FROM written", &[])
            .await
            .expect("count the scratch rows")
            .get(0)
    }

    #[tokio::test]
    async fn a_transaction_whose_work_succeeds_commits_and_returns_its_connection() {
        let Some(url) = scratch_url().await else {
            return;
        };
        // Arrange
        let (pool, pid) = pool_with_scratch_table(&url).await;
        let connection = pool.checkout().await.expect("checkout");

        // Act
        let answered: Result<&str> = connection
            .transaction(None, async |transaction| {
                transaction
                    .execute("INSERT INTO written VALUES (1)", &[])
                    .await?;
                Ok("written")
            })
            .await;

        // Assert
        assert_eq!(answered.expect("the work succeeds"), "written");
        let next = pool.checkout().await.expect("checkout after the commit");
        assert_eq!(backend_pid(&next).await, pid, "the connection was reused");
        assert_eq!(rows_written(&next).await, 1, "the write was committed");
    }

    #[tokio::test]
    async fn a_transaction_whose_work_fails_rolls_back_and_returns_its_connection() {
        let Some(url) = scratch_url().await else {
            return;
        };
        // Arrange
        let (pool, pid) = pool_with_scratch_table(&url).await;
        let connection = pool.checkout().await.expect("checkout");

        // Act: an ordinary refusal after the work has written something.
        let answered: Result<()> = connection
            .transaction(None, async |transaction| {
                transaction
                    .execute("INSERT INTO written VALUES (1)", &[])
                    .await?;
                anyhow::bail!("refused")
            })
            .await;

        // Assert
        assert_eq!(answered.expect_err("the work fails").to_string(), "refused");
        let next = pool.checkout().await.expect("checkout after the rollback");
        assert_eq!(backend_pid(&next).await, pid, "the connection was reused");
        assert_eq!(rows_written(&next).await, 0, "the write was rolled back");
    }

    #[tokio::test]
    async fn a_transaction_abandoned_mid_flight_closes_its_connection() {
        let Some(url) = scratch_url().await else {
            return;
        };
        // Arrange: a request cancelled while its statement still runs.
        let (pool, pid) = pool_with_scratch_table(&url).await;
        let connection = pool.checkout().await.expect("checkout");
        let work = connection.transaction(None, async |transaction| -> Result<()> {
            transaction.execute("SELECT pg_sleep(30)", &[]).await?;
            Ok(())
        });

        // Act
        let cancelled = tokio::time::timeout(Duration::from_millis(200), work).await;

        // Assert
        assert!(cancelled.is_err(), "the work was still running");
        let next = pool.checkout().await.expect("checkout after the abandon");
        assert_ne!(backend_pid(&next).await, pid, "the connection was closed");
    }

    #[tokio::test]
    async fn a_transaction_runs_at_the_isolation_level_it_asks_for() {
        let Some(url) = scratch_url().await else {
            return;
        };
        // Arrange
        let pool = Pool::new(settings(&url, 1, Session::default())).expect("build the pool");
        let connection = pool.checkout().await.expect("checkout");

        // Act
        let level: Result<String> = connection
            .transaction(Some(IsolationLevel::RepeatableRead), async |transaction| {
                Ok(transaction
                    .query_one("SHOW transaction_isolation", &[])
                    .await?
                    .get(0))
            })
            .await;

        // Assert
        assert_eq!(level.expect("the work succeeds"), "repeatable read");
    }

    #[tokio::test]
    async fn a_pool_that_requires_tls_refuses_a_server_that_declines_encryption() {
        // Arrange: a local endpoint that refuses TLS, reached with a mode that
        // would otherwise permit a plaintext session.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let port = listener.local_addr().expect("bound address").port();
        let server = tokio::spawn(crate::test_support::decline_tls_once(listener));
        let url = format!(
            "host=localhost hostaddr=127.0.0.1 port={port} user=writer dbname=catalog \
             sslmode=prefer connect_timeout=5"
        );
        let pool = Pool::new(PoolSettings {
            force_tls: true,
            ..settings(&url, 1, Session::default())
        })
        .expect("build the pool");

        // Act
        let result = pool.checkout().await;

        // Assert: no connection, and nothing followed the declined request.
        assert!(result.is_err(), "the pool must not fall back to plaintext");
        let sent_plaintext = tokio::time::timeout(SETTLE, server)
            .await
            .expect("server observes the client closing")
            .expect("server task completes");
        assert!(!sent_plaintext, "the pool sent a plaintext startup");
    }
}
