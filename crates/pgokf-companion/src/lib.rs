// SPDX-License-Identifier: AGPL-3.0-only
//! Shared runtime for the `pgokf` companion binaries.
//!
//! The network companions (`pgokf-ingest`, `pgokf-embed`, `pgokf-mcp`) are
//! small, separately deployed processes that talk to the catalog only through
//! its public SQL surface. This crate holds the pieces of that runtime that no
//! single companion should own:
//!
//! - [`daemon::run`], an error-tolerant fixed-interval loop behind each
//!   `--watch` mode: a pass that fails is reported and the loop keeps going,
//!   so a transient outage (the database restarting, an object store or
//!   embeddings server redeploying) never kills the daemon - the next tick
//!   simply retries.
//! - [`daemon::shutdown_signal`], a future that resolves on **SIGINT or
//!   SIGTERM**, with the handlers installed the moment it is created, so both
//!   an interactive Ctrl-C and a container runtime's `docker stop` end the
//!   loop cleanly even when they arrive during the very first pass.
//! - [`cli::non_empty`], the one rule for optional command-line values that
//!   also come from the environment: an empty value means "not set".
//!
//! The loop is generic over the pass and the shutdown future, which is what
//! makes it unit-testable with paused time and a hand-triggered shutdown.

pub mod cli {
    //! Command-line conventions shared by the companions.

    /// Treat an empty optional value as absent.
    ///
    /// Every optional companion flag also reads an environment variable, and
    /// a container or compose stack cannot leave a variable *out* - it can
    /// only leave it *empty*. `clap` faithfully hands an empty variable
    /// through as `Some("")`, which would then be sent as an empty bearer
    /// token, an empty object-store endpoint, or an empty static access key
    /// that overrides an instance profile. Normalizing here keeps "unset" and
    /// "empty" equivalent everywhere.
    #[must_use]
    pub fn non_empty(value: Option<String>) -> Option<String> {
        value.filter(|text| !text.is_empty())
    }

    #[cfg(test)]
    mod tests {
        use super::non_empty;

        #[test]
        fn non_empty_maps_an_empty_value_to_none() {
            // Arrange
            let value = Some(String::new());

            // Act
            let normalized = non_empty(value);

            // Assert
            assert_eq!(normalized, None);
        }

        #[test]
        fn non_empty_keeps_a_present_value_and_none() {
            // Arrange / Act / Assert
            assert_eq!(non_empty(Some("key".to_owned())), Some("key".to_owned()));
            assert_eq!(non_empty(None), None);
        }

        #[test]
        fn non_empty_does_not_trim_whitespace() {
            // Arrange: a whitespace-only value is not empty and is passed on
            // untouched - trimming secrets or paths silently would be worse.
            let value = Some(" ".to_owned());

            // Act / Assert
            assert_eq!(non_empty(value), Some(" ".to_owned()));
        }
    }
}

pub mod daemon {
    //! The `--watch` loop and its shutdown signal.

    use std::future::Future;
    use std::time::Duration;

    use anyhow::{Context, Result};
    use tokio::time::sleep;

    /// Drive `pass` once immediately and then once per `interval` until
    /// `shutdown` resolves, returning the shutdown future's result.
    ///
    /// A pass that returns an error is handed to `on_error` and the loop
    /// continues; only the shutdown future ends it. The shutdown is observed
    /// while the loop is sleeping (and preferred over the next tick when both
    /// are ready), so a stop request is honored within the current pass's
    /// duration at most - a pass in flight is never interrupted halfway
    /// through its work.
    ///
    /// # Errors
    ///
    /// Returns whatever error the `shutdown` future resolves with; pass errors
    /// never propagate.
    pub async fn run<Shutdown>(
        interval: Duration,
        mut pass: impl AsyncFnMut() -> Result<()>,
        shutdown: Shutdown,
        mut on_error: impl FnMut(&anyhow::Error),
    ) -> Result<()>
    where
        Shutdown: Future<Output = Result<()>>,
    {
        tokio::pin!(shutdown);
        loop {
            if let Err(error) = pass().await {
                on_error(&error);
            }

            // Sleep until the next interval, but wake immediately on shutdown
            // so stopping is prompt regardless of the interval length. Biased:
            // a shutdown that arrived during the pass wins over a tick that is
            // ready at the same instant, so no extra pass runs.
            tokio::select! {
                biased;
                result = &mut shutdown => return result,
                () = sleep(interval) => {}
            }
        }
    }

    /// A future that resolves when the process receives SIGINT (Ctrl-C) or,
    /// on Unix, SIGTERM (what `docker stop`, systemd, and Kubernetes send).
    ///
    /// The handlers are installed **now**, not when the future is first
    /// polled: a signal that arrives while the daemon is still busy with its
    /// first pass is therefore queued and honored right after it, instead of
    /// terminating the process with the default action.
    ///
    /// # Errors
    ///
    /// Returns an error if a signal handler cannot be installed.
    pub fn shutdown_signal() -> Result<impl Future<Output = Result<()>>> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            let mut interrupt =
                signal(SignalKind::interrupt()).context("failed to install the SIGINT handler")?;
            let mut terminate =
                signal(SignalKind::terminate()).context("failed to install the SIGTERM handler")?;
            Ok(async move {
                tokio::select! {
                    _ = interrupt.recv() => Ok(()),
                    _ = terminate.recv() => Ok(()),
                }
            })
        }
        #[cfg(not(unix))]
        {
            Ok(async {
                tokio::signal::ctrl_c()
                    .await
                    .context("failed to install the SIGINT handler")
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use std::cell::Cell;
        use std::rc::Rc;

        use anyhow::anyhow;
        use tokio::sync::oneshot;
        use tokio::time::advance;

        use super::*;

        const INTERVAL: Duration = Duration::from_secs(10);

        /// A shutdown future driven by the test: resolves with `result` once
        /// the sender fires.
        fn triggered_shutdown(
            result: Result<()>,
        ) -> (oneshot::Sender<()>, impl Future<Output = Result<()>>) {
            let (tx, rx) = oneshot::channel();
            let shutdown = async move {
                let _ = rx.await;
                result
            };
            (tx, shutdown)
        }

        /// A single-threaded runtime with paused time, so intervals are
        /// advanced deterministically instead of waited for.
        fn paused_runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .start_paused(true)
                .build()
                .expect("runtime")
        }

        #[test]
        fn run_counts_one_immediate_pass_plus_one_per_interval() {
            // Arrange
            let runtime = paused_runtime();
            let local = tokio::task::LocalSet::new();
            let passes = Rc::new(Cell::new(0_u32));
            let counted = Rc::clone(&passes);
            let (stop, shutdown) = triggered_shutdown(Ok(()));

            // Act
            let outcome = local.block_on(&runtime, async move {
                let daemon = tokio::task::spawn_local(run(
                    INTERVAL,
                    async move || {
                        counted.set(counted.get() + 1);
                        Ok(())
                    },
                    shutdown,
                    |_| panic!("no pass should fail"),
                ));
                // Let the first pass run, then elapse three intervals one at a
                // time (each tick must wake the sleep and run exactly one pass).
                tokio::task::yield_now().await;
                for _ in 0..3 {
                    advance(INTERVAL).await;
                    tokio::task::yield_now().await;
                }
                stop.send(()).expect("daemon is still waiting");
                daemon.await.expect("daemon task completes")
            });

            // Assert
            assert!(outcome.is_ok());
            assert_eq!(passes.get(), 4, "one immediate pass + three intervals");
        }

        #[test]
        fn run_keeps_going_after_a_failing_pass_and_reports_the_error() {
            // Arrange: the first pass fails, later passes succeed.
            let runtime = paused_runtime();
            let local = tokio::task::LocalSet::new();
            let passes = Rc::new(Cell::new(0_u32));
            let counted = Rc::clone(&passes);
            let reported = Rc::new(Cell::new(0_u32));
            let recorded = Rc::clone(&reported);
            let (stop, shutdown) = triggered_shutdown(Ok(()));

            // Act
            let outcome = local.block_on(&runtime, async move {
                let daemon = tokio::task::spawn_local(run(
                    INTERVAL,
                    async move || {
                        counted.set(counted.get() + 1);
                        if counted.get() == 1 {
                            Err(anyhow!("transient outage"))
                        } else {
                            Ok(())
                        }
                    },
                    shutdown,
                    move |error| {
                        assert_eq!(error.to_string(), "transient outage");
                        recorded.set(recorded.get() + 1);
                    },
                ));
                tokio::task::yield_now().await;
                advance(INTERVAL).await;
                tokio::task::yield_now().await;
                stop.send(()).expect("daemon is still waiting");
                daemon.await.expect("daemon task completes")
            });

            // Assert: the failure was reported once and did not end the loop.
            assert!(outcome.is_ok());
            assert_eq!(reported.get(), 1);
            assert_eq!(passes.get(), 2);
        }

        #[test]
        fn run_returns_promptly_mid_sleep_and_propagates_the_shutdown_result() {
            // Arrange: a shutdown that resolves with an error, fired mid-interval.
            let runtime = paused_runtime();
            let local = tokio::task::LocalSet::new();
            let passes = Rc::new(Cell::new(0_u32));
            let counted = Rc::clone(&passes);
            let (stop, shutdown) = triggered_shutdown(Err(anyhow!("handler failed")));

            // Act: stop halfway through the first sleep.
            let outcome = local.block_on(&runtime, async move {
                let daemon = tokio::task::spawn_local(run(
                    INTERVAL,
                    async move || {
                        counted.set(counted.get() + 1);
                        Ok(())
                    },
                    shutdown,
                    |_| panic!("no pass should fail"),
                ));
                tokio::task::yield_now().await;
                advance(INTERVAL / 2).await;
                stop.send(()).expect("daemon is still waiting");
                daemon.await.expect("daemon task completes")
            });

            // Assert: no second pass ran, and the shutdown's error surfaced.
            assert_eq!(passes.get(), 1);
            assert_eq!(
                outcome.expect_err("shutdown error propagates").to_string(),
                "handler failed"
            );
        }

        #[test]
        fn run_prefers_a_shutdown_that_arrived_during_the_pass_over_the_next_tick() {
            // Arrange: the shutdown fires while the first pass is running.
            let runtime = paused_runtime();
            let local = tokio::task::LocalSet::new();
            let passes = Rc::new(Cell::new(0_u32));
            let counted = Rc::clone(&passes);
            let (stop, shutdown) = triggered_shutdown(Ok(()));
            let stop = Rc::new(Cell::new(Some(stop)));
            let stopper = Rc::clone(&stop);

            // Act: the pass itself requests the stop, then the whole interval
            // elapses before the loop gets to choose between tick and shutdown.
            let outcome = local.block_on(&runtime, async move {
                let daemon = tokio::task::spawn_local(run(
                    INTERVAL,
                    async move || {
                        counted.set(counted.get() + 1);
                        if let Some(tx) = stopper.take() {
                            tx.send(()).expect("daemon is running");
                        }
                        Ok(())
                    },
                    shutdown,
                    |_| panic!("no pass should fail"),
                ));
                tokio::task::yield_now().await;
                advance(INTERVAL).await;
                daemon.await.expect("daemon task completes")
            });

            // Assert: exactly one pass; the ready tick did not win.
            assert!(outcome.is_ok());
            assert_eq!(passes.get(), 1);
        }

        #[cfg(unix)]
        #[test]
        fn shutdown_signal_installs_handlers_eagerly_and_resolves_on_sigterm() {
            // Arrange: create the future (handlers install now), then raise
            // SIGTERM at ourselves BEFORE the future is ever polled.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let outcome = runtime.block_on(async {
                let shutdown = shutdown_signal().expect("handlers install");
                // Deliver SIGTERM to ourselves through the platform `kill`
                // utility: no unsafe code (the workspace forbids it), and the
                // signal arrives before the future below is first polled.
                let delivered = std::process::Command::new("kill")
                    .args(["-TERM", &std::process::id().to_string()])
                    .status()
                    .expect("kill utility runs");
                assert!(delivered.success(), "kill must deliver SIGTERM");

                // Act
                tokio::time::timeout(Duration::from_secs(5), shutdown).await
            });

            // Assert: the queued signal resolved the future cleanly.
            assert!(
                matches!(outcome, Ok(Ok(()))),
                "SIGTERM must resolve the shutdown future"
            );
        }
    }
}
