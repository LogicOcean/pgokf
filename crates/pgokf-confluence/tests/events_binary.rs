// SPDX-License-Identifier: AGPL-3.0-only
//! The `pgokf-confluence-events` binary as an operator runs it.
//!
//! Every request here is refused before a signing key is needed, so the
//! binary never reaches Atlassian while these tests run.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

const BINARY: &str = env!("CARGO_BIN_EXE_pgokf-confluence-events");
const APP_ID: &str = "ari:cloud:ecosystem::app/8db33809-1f32-48bb-8c52-5877dab48107";
const CLOUD_ID: &str = "f349edd6-f44e-48b8-b6ed-61150c06d9c5";

/// The binary with this test's configuration, on an ephemeral loopback port.
fn command(app_id: &str) -> Command {
    let mut command = Command::new(BINARY);
    command
        .args(["--listen", "127.0.0.1:0"])
        .env("FORGE_APP_ID", app_id)
        .env("CONFLUENCE_CLOUD_ID", CLOUD_ID)
        .env_remove("PGOKF_CONFLUENCE_EVENTS_LISTEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    command
}

/// A running receiver, killed when dropped.
struct Running {
    child: Child,
    announcement: String,
    log: mpsc::Receiver<String>,
}

impl Running {
    /// Start the binary and wait for the line naming where it receives.
    fn start() -> Self {
        let mut child = command(APP_ID)
            .stderr(Stdio::piped())
            .spawn()
            .expect("the binary starts");
        let stderr = child.stderr.take().expect("piped standard error");
        let (lines, log) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        let announcement = log
            .recv_timeout(Duration::from_mins(1))
            .expect("the binary announces where it receives deliveries");
        Self {
            child,
            announcement,
            log,
        }
    }

    /// The `host:port` the announcement names.
    fn address(&self) -> &str {
        self.announcement
            .split_once("http://")
            .and_then(|(_, rest)| rest.split_once('/'))
            .map_or_else(
                || panic!("no address in {:?}", self.announcement),
                |(address, _)| address,
            )
    }

    /// Send one HTTP/1.1 request; the whole answer.
    fn request(&self, head: &str, body: &str) -> String {
        let address = self.address();
        let mut stream = TcpStream::connect(address)
            .unwrap_or_else(|error| panic!("connecting to {address}: {error}"));
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        write!(
            stream,
            "{head}\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        answer
    }

    /// Every log line written so far, once the log has been quiet briefly.
    fn drain_log(&self) -> Vec<String> {
        let mut lines = Vec::new();
        while let Ok(line) = self.log.recv_timeout(Duration::from_millis(1500)) {
            lines.push(line);
        }
        lines
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn status_line(answer: &str) -> &str {
    answer.lines().next().unwrap_or_default()
}

#[test]
fn the_binary_names_the_port_it_bound_and_answers_the_health_probe_there() {
    // Act
    let running = Running::start();

    // Assert: asked for an ephemeral port, it must say which one it got.
    assert!(
        running.address().starts_with("127.0.0.1:") && !running.address().ends_with(":0"),
        "{:?}",
        running.announcement
    );
    let answer = running.request("GET /healthz HTTP/1.1", "");
    assert_eq!(status_line(&answer), "HTTP/1.1 200 OK", "{answer}");
}

#[test]
fn the_binary_refuses_unverifiable_deliveries_and_logs_each_on_one_line() {
    // Arrange: a token whose header smuggles a forged log line into the
    // algorithm it names.
    let running = Running::start();
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256\nFORGED delivery accepted","kid":"k"}"#);
    let token = format!("{header}.e30.c2ln");
    let post = "POST /forge/confluence/events HTTP/1.1\r\nContent-Type: application/json";

    // Act
    let without_token = running.request(post, "{}");
    let with_token = running.request(&format!("{post}\r\nAuthorization: Bearer {token}"), "{}");
    let log = running.drain_log();

    // Assert
    assert_eq!(status_line(&without_token), "HTTP/1.1 401 Unauthorized");
    assert_eq!(status_line(&with_token), "HTTP/1.1 401 Unauthorized");
    assert!(
        log.iter().any(|line| line.contains("delivery refused")),
        "{log:#?}"
    );
    assert!(
        log.iter()
            .all(|line| line.starts_with("pgokf-confluence-events: ")),
        "{log:#?}"
    );
    assert!(log.iter().all(|line| !line.contains(&token)), "{log:#?}");
}

#[test]
fn the_binary_will_not_start_for_an_app_id_in_another_form() {
    // Act
    let output = command("8db33809-1f32-48bb-8c52-5877dab48107")
        .stderr(Stdio::piped())
        .output()
        .expect("the binary runs");

    // Assert
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ari:cloud:ecosystem::app/"), "{stderr}");
}
