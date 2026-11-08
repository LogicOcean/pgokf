//! Foreground owned services with durable receipts and bounded nonblocking pipes.
use crate::{owned_docker::publish, process::CommandSpec};
use nix::{
    fcntl::{FcntlArg, OFlag, fcntl},
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde_json::json;
use std::{
    io::{Read, Write},
    os::{fd::AsFd, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub struct Service {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    stderr: ChildStderr,
    pending: Vec<u8>,
    errors: Vec<u8>,
    total: usize,
    cap: usize,
    deadline: Instant,
    receipt: PathBuf,
    stopped: bool,
}
fn nonblocking(fd: &impl AsFd) -> Result<(), String> {
    let bits = fcntl(fd, FcntlArg::F_GETFL).map_err(|e| e.to_string())?;
    fcntl(
        fd,
        FcntlArg::F_SETFL(OFlag::from_bits_truncate(bits) | OFlag::O_NONBLOCK),
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
impl Service {
    /// Spawn a foreground process in its own process group. Receipt paths must be new.
    /// # Errors
    /// Fails on I/O, receipt collision, process exit, or exceeded execution bounds.
    pub fn spawn(spec: &CommandSpec, receipt: &Path) -> Result<Self, String> {
        if spec.output_cap == 0 || spec.timeout.is_zero() {
            return Err("service bounds must be positive".into());
        }
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis();
        let identity = json!({"program":spec.program,"args":spec.args.iter().map(|a|a.to_string_lossy()).collect::<Vec<_>>(),"started_unix_ms":started,"nonce":uuid::Uuid::new_v4().to_string()});
        publish(receipt, &json!({"state":"intent","identity":identity}))?;
        let mut command = Command::new(&spec.program);
        for name in &spec.env_remove {
            command.env_remove(name);
        }
        command
            .args(&spec.args)
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or("stdout unavailable")?;
        let stderr = child.stderr.take().ok_or("stderr unavailable")?;
        let mut service = Self {
            child,
            stdin,
            stdout,
            stderr,
            pending: Vec::new(),
            errors: Vec::new(),
            total: 0,
            cap: spec.output_cap,
            deadline: Instant::now() + spec.timeout,
            receipt: receipt.to_path_buf(),
            stopped: false,
        };
        publish(
            &receipt.with_extension("started.json"),
            &json!({"state":"started","pid":service.child.id(),"identity":identity}),
        )?;
        nonblocking(&service.stdout)?;
        nonblocking(&service.stderr)?;
        if let Some(input) = &service.stdin {
            nonblocking(input)?;
        }
        if !spec.stdin.is_empty() {
            service.write(&spec.stdin)?;
        }
        Ok(service)
    }
    fn pump(&mut self) -> Result<(), String> {
        if Instant::now() >= self.deadline {
            return Err("service lifetime deadline exceeded".into());
        }
        let mut buf = [0u8; 8192];
        for stream in 0..2 {
            loop {
                if Instant::now() >= self.deadline {
                    return Err("service lifetime deadline exceeded".into());
                }
                let read = if stream == 0 {
                    self.stdout.read(&mut buf)
                } else {
                    self.stderr.read(&mut buf)
                };
                match read {
                    Ok(0) => break,
                    Ok(n) => {
                        self.total += n;
                        if self.total > self.cap {
                            return Err("service output cap exceeded".into());
                        }
                        if stream == 0 {
                            self.pending.extend_from_slice(&buf[..n]);
                        } else {
                            self.errors.extend_from_slice(&buf[..n]);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e.to_string()),
                }
            }
        }
        Ok(())
    }
    /// # Errors
    /// Fails on I/O, receipt collision, process exit, or exceeded execution bounds.
    pub fn check(&mut self) -> Result<(), String> {
        self.pump()?;
        if let Some(status) = self.child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!(
                "service exited {status}: {}",
                String::from_utf8_lossy(&self.errors)
            ));
        }
        Ok(())
    }
    /// # Errors
    /// Fails on I/O, receipt collision, process exit, or exceeded execution bounds.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        let deadline = (Instant::now() + Duration::from_secs(30)).min(self.deadline);
        let mut remaining = bytes;
        while !remaining.is_empty() {
            self.check()?;
            if Instant::now() >= deadline {
                return Err("service stdin deadline exceeded".into());
            }
            match self
                .stdin
                .as_mut()
                .ok_or("service stdin closed")?
                .write(remaining)
            {
                Ok(0) => return Err("service stdin closed".into()),
                Ok(n) => remaining = &remaining[n..],
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    }
    /// # Errors
    /// Fails on I/O, receipt collision, process exit, or exceeded execution bounds.
    pub fn line(&mut self, timeout: Duration) -> Result<Vec<u8>, String> {
        let deadline = (Instant::now() + timeout).min(self.deadline);
        loop {
            self.pump()?;
            if let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
                return Ok(self.pending.drain(..=end).collect());
            }
            self.check()?;
            if Instant::now() >= deadline {
                return Err("service response deadline exceeded".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    /// # Errors
    /// Fails on I/O, receipt collision, process exit, or exceeded execution bounds.
    pub fn stop(&mut self) -> Result<(), String> {
        if self.stopped {
            return Ok(());
        }
        let pid = Pid::from_raw(i32::try_from(self.child.id()).map_err(|e| e.to_string())?);
        let _ = killpg(pid, Signal::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self.child.try_wait().map_err(|e| e.to_string())?.is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = killpg(pid, Signal::SIGKILL);
                self.child.wait().map_err(|e| e.to_string())?;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = killpg(pid, Signal::SIGKILL);
        self.stopped = true;
        publish(
            &self.receipt.with_extension("stopped.json"),
            &json!({"state":"stopped","pid":self.child.id()}),
        )
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        if self.stop().is_err() {
            if let Ok(pid) = i32::try_from(self.child.id()) {
                let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
            }
            let _ = self.child.wait();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interactive_and_receipts() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = CommandSpec::new("cat");
        let path = tmp.path().join("service.json");
        let mut s = Service::spawn(&spec, &path).unwrap();
        s.write(b"hello\n").unwrap();
        assert_eq!(s.line(Duration::from_secs(1)).unwrap(), b"hello\n");
        s.stop().unwrap();
        assert!(path.with_extension("started.json").exists());
        assert!(path.with_extension("stopped.json").exists());
        assert!(Service::spawn(&spec, &path).is_err());
    }
    #[test]
    fn failed_publication_cleans_child() {
        let tmp = tempfile::tempdir().unwrap();
        let receipt = tmp.path().join("collision.json");
        std::fs::write(receipt.with_extension("started.json"), b"occupied").unwrap();
        assert!(Service::spawn(&CommandSpec::new("cat"), &receipt).is_err());
        let stopped: serde_json::Value = crate::json::from_slice(
            &std::fs::read(receipt.with_extension("stopped.json")).unwrap(),
        )
        .unwrap();
        let pid = i32::try_from(stopped["pid"].as_i64().unwrap()).unwrap();
        assert!(nix::sys::signal::kill(Pid::from_raw(pid), None).is_err());
        assert_eq!(
            std::fs::read(receipt.with_extension("started.json")).unwrap(),
            b"occupied"
        );
    }
    #[test]
    fn bounded_output_and_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let mut spec = CommandSpec::new("yes");
        spec.output_cap = 64;
        let mut s = Service::spawn(&spec, &tmp.path().join("output.json")).unwrap();
        assert!(s.line(Duration::from_secs(1)).is_err());
        s.stop().unwrap();
        let mut spec = CommandSpec::new("cat");
        spec.timeout = Duration::from_millis(20);
        let mut s = Service::spawn(&spec, &tmp.path().join("timeout.json")).unwrap();
        assert!(s.line(Duration::from_secs(1)).is_err());
    }
}
