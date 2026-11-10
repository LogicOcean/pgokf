//! Foreground owned services with durable receipts and bounded nonblocking pipes.
use crate::process::CommandSpec;

/// Machinery-controlled pump chunk for every owned-service stream.
///
/// Contained stdio under launchd is an OS-fixed 8 KiB FIFO (measured 8,192
/// bytes on this host; there is no macOS knob to enlarge it), while the
/// pre-containment `std::process` pipes hold 65,536 bytes. The supervised
/// pump makes that platform capacity non-load-bearing; this buffer is the
/// explicit pre-containment capacity, never a platform default.
pub(crate) const SERVICE_PUMP_CHUNK: usize = 64 * 1024;

#[cfg(target_os = "macos")]
use crate::process::launchd::LaunchdChild;
#[cfg(not(target_os = "macos"))]
use crate::process::{ProcessMonitor, ProcessOwner};
use nix::{
    fcntl::{AtFlags, FcntlArg, OFlag, fcntl, openat},
    sys::signal::Signal,
    sys::stat::Mode,
    unistd::{UnlinkatFlags, linkat, unlinkat},
};
#[cfg(not(target_os = "macos"))]
use nix::{sys::signal::killpg, unistd::Pid};
#[cfg(not(target_os = "macos"))]
use rustix::process::{Pid as RustixPid, WaitId, WaitIdOptions, WaitIdStatus, waitid};
use serde_json::json;
use std::{
    fs::File,
    io::{Read, Write},
    os::fd::AsFd,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    process::ExitStatus,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
#[cfg(not(target_os = "macos"))]
use std::{
    os::unix::process::CommandExt,
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Stdio},
};

struct ReceiptDirectory {
    directory: File,
}
impl ReceiptDirectory {
    fn capture(receipt: &Path) -> Result<Self, String> {
        let path = receipt.parent().ok_or("service receipt parent missing")?;
        let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("service receipt parent must be a real directory".into());
        }
        let directory = File::open(path).map_err(|error| error.to_string())?;
        let opened = directory.metadata().map_err(|error| error.to_string())?;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err("service receipt parent changed while opening".into());
        }
        Ok(Self { directory })
    }
    fn publish(&self, path: &Path, value: &serde_json::Value) -> Result<(), String> {
        let name = path.file_name().ok_or("service receipt filename missing")?;
        let temp = format!(".{}.{}.tmp", name.to_string_lossy(), uuid::Uuid::new_v4());
        let descriptor = openat(
            &self.directory,
            temp.as_str(),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(|error| error.to_string())?;
        let mut file = File::from(descriptor);
        let result = (|| {
            file.write_all(&serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?)
                .and_then(|()| file.sync_all())
                .map_err(|error| error.to_string())?;
            linkat(
                &self.directory,
                temp.as_str(),
                &self.directory,
                name,
                AtFlags::empty(),
            )
            .map_err(|error| error.to_string())?;
            self.directory.sync_all().map_err(|error| error.to_string())
        })();
        let _ = unlinkat(&self.directory, temp.as_str(), UnlinkatFlags::NoRemoveDir);
        result
    }
}

pub struct Service {
    #[cfg(target_os = "macos")]
    child: LaunchdChild,
    #[cfg(not(target_os = "macos"))]
    child: Child,
    #[cfg(not(target_os = "macos"))]
    _owner: ProcessOwner,
    #[cfg(not(target_os = "macos"))]
    monitor: ProcessMonitor,
    #[cfg(target_os = "macos")]
    stdin: Option<File>,
    #[cfg(not(target_os = "macos"))]
    stdin: Option<ChildStdin>,
    #[cfg(target_os = "macos")]
    stdout: File,
    #[cfg(not(target_os = "macos"))]
    stdout: ChildStdout,
    #[cfg(target_os = "macos")]
    stderr: File,
    #[cfg(not(target_os = "macos"))]
    stderr: ChildStderr,
    pending: Vec<u8>,
    errors: Vec<u8>,
    total: usize,
    cap: usize,
    deadline: Instant,
    receipt: PathBuf,
    receipt_directory: ReceiptDirectory,
    identity: serde_json::Value,
    stopped: bool,
    stop_error: Option<String>,
    runtime_error: Option<String>,
    status: Option<ExitStatus>,
    observed_exit: Option<String>,
    stdout_eof: bool,
    stderr_eof: bool,
    #[cfg(test)]
    signals_sent: usize,
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
        let identity = json!({
            "program": spec.program,
            "args": spec.args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>(),
            "started_unix_ms": started,
            "nonce": uuid::Uuid::new_v4().to_string(),
            "campaign_id": std::env::var("PGOKF_VERIFICATION_CAMPAIGN")
                .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string()),
            "case_id": std::env::var("PGOKF_VERIFICATION_CASE")
                .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string()),
        });
        let receipt_directory = ReceiptDirectory::capture(receipt)?;
        receipt_directory.publish(receipt, &json!({"state":"intent","identity":identity}))?;
        #[cfg(target_os = "macos")]
        let spawn_deadline = Instant::now() + Duration::from_secs(5);
        #[cfg(target_os = "macos")]
        let (child, stdin, stdout, stderr) = {
            let mut child = LaunchdChild::spawn(spec, spawn_deadline)?;
            let stdin = child.take_stdin();
            let stdout = child.take_stdout()?;
            let stderr = child.take_stderr()?;
            (child, stdin, stdout, stderr)
        };
        #[cfg(not(target_os = "macos"))]
        let (child, owner, monitor, stdin, stdout, stderr) = {
            let owner = ProcessOwner::new()?;
            let mut command = owner.command(spec);
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0);
            let mut child = command.spawn().map_err(|e| e.to_string())?;
            let pid = Pid::from_raw(i32::try_from(child.id()).map_err(|error| error.to_string())?);
            let monitor = match owner.monitor(child.id()) {
                Ok(monitor) => monitor,
                Err(error) => {
                    let _ = killpg(pid, Signal::SIGKILL);
                    let _ = child.wait();
                    return Err(error);
                }
            };
            if let Err(error) = owner.release() {
                let _ = killpg(pid, Signal::SIGKILL);
                let _ = child.wait();
                return Err(error);
            }
            let stdin = child.stdin.take();
            let stdout = child.stdout.take().ok_or("stdout unavailable")?;
            let stderr = child.stderr.take().ok_or("stderr unavailable")?;
            (child, owner, monitor, stdin, stdout, stderr)
        };
        let mut service = Self {
            child,
            #[cfg(not(target_os = "macos"))]
            _owner: owner,
            #[cfg(not(target_os = "macos"))]
            monitor,
            stdin,
            stdout,
            stderr,
            pending: Vec::new(),
            errors: Vec::new(),
            total: 0,
            cap: spec.output_cap,
            deadline: Instant::now() + spec.timeout,
            receipt: receipt.to_path_buf(),
            receipt_directory,
            identity: identity.clone(),
            stopped: false,
            stop_error: None,
            runtime_error: None,
            status: None,
            observed_exit: None,
            stdout_eof: false,
            stderr_eof: false,
            #[cfg(test)]
            signals_sent: 0,
        };
        service.receipt_directory.publish(
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
    fn pump(&mut self, enforce_lifetime: bool) -> Result<(), String> {
        if enforce_lifetime && Instant::now() >= self.deadline {
            return Err("service lifetime deadline exceeded".into());
        }
        let mut buf = vec![0u8; SERVICE_PUMP_CHUNK];
        for stream in 0..2 {
            if (stream == 0 && self.stdout_eof) || (stream == 1 && self.stderr_eof) {
                continue;
            }
            loop {
                if enforce_lifetime && Instant::now() >= self.deadline {
                    return Err("service lifetime deadline exceeded".into());
                }
                let read = if stream == 0 {
                    self.stdout.read(&mut buf)
                } else {
                    self.stderr.read(&mut buf)
                };
                match read {
                    Ok(0) => {
                        if stream == 0 {
                            self.stdout_eof = true;
                        } else {
                            self.stderr_eof = true;
                        }
                        break;
                    }
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
    fn pump_recorded(&mut self, enforce_lifetime: bool) -> Result<(), String> {
        let result = self.pump(enforce_lifetime);
        if let Err(error) = &result
            && error != "service lifetime deadline exceeded"
        {
            self.runtime_error.get_or_insert_with(|| error.clone());
        }
        result
    }
    #[cfg(not(target_os = "macos"))]
    fn describe_exit(status: &WaitIdStatus) -> String {
        if let Some(code) = status.exit_status() {
            format!("exit status: {code}")
        } else if let Some(signal) = status.terminating_signal() {
            format!("signal: {signal:?}")
        } else {
            format!("{status:?}")
        }
    }
    fn refresh_exit_observation(&mut self) -> Result<Option<String>, String> {
        #[cfg(target_os = "macos")]
        if self.observed_exit.is_none() && self.status.is_none() && self.child.observe_exit()? {
            let status = self
                .child
                .observed_status()
                .ok_or("launchd target status missing after exit observation")?;
            self.observed_exit = Some(status.to_string());
        }
        #[cfg(not(target_os = "macos"))]
        if self.observed_exit.is_none()
            && self.status.is_none()
            && let Some(status) = waitid(
                WaitId::Pid(RustixPid::from_child(&self.child)),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            )
            .map_err(|error| error.to_string())?
        {
            self.observed_exit = Some(Self::describe_exit(&status));
        }
        Ok(self.observed_exit.clone())
    }
    fn signal_group(&mut self, signal: Signal) -> Result<(), String> {
        if self.status.is_none() {
            #[cfg(test)]
            {
                self.signals_sent += 1;
            }
            #[cfg(target_os = "macos")]
            self.child.signal_group(signal)?;
            #[cfg(not(target_os = "macos"))]
            {
                let pid = Pid::from_raw(
                    i32::try_from(self.child.id()).map_err(|error| error.to_string())?,
                );
                let _ = killpg(pid, signal);
            }
        }
        Ok(())
    }
    fn reap_child(&mut self, deadline: Instant) -> Result<ExitStatus, String> {
        #[cfg(target_os = "macos")]
        {
            self.child.wait(deadline)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = deadline;
            self.child.wait().map_err(|error| error.to_string())
        }
    }
    fn release_containment(&mut self) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            self.child.terminate_and_ensure_released()
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.monitor.terminate_and_ensure_released()
        }
    }
    fn snapshot(&mut self, enforce_lifetime: bool) -> (serde_json::Value, Result<(), String>) {
        let pump_error = self.pump_recorded(enforce_lifetime).err();
        let status = self.refresh_exit_observation();
        let state = match status {
            Ok(Some(ref status)) => format!("exited {status}"),
            Ok(None) if self.status.is_some() => "exited (reaped)".into(),
            Ok(None) => "running".into(),
            Err(ref error) => format!("unknown: {error}"),
        };
        let health = pump_error.clone().map_or_else(
            || match status {
                Ok(Some(status)) => Err(format!(
                    "service exited {status}: {}",
                    String::from_utf8_lossy(&self.errors[self.errors.len().saturating_sub(4096)..])
                )),
                Ok(None) if self.status.is_some() => Err("service exited (reaped)".into()),
                Ok(None) => Ok(()),
                Err(error) => Err(error),
            },
            Err,
        );
        (
            json!({
                "pid": self.child.id(),
                "state": state,
                "stdout_tail": String::from_utf8_lossy(&self.pending[self.pending.len().saturating_sub(4096)..]),
                "stderr_tail": String::from_utf8_lossy(&self.errors[self.errors.len().saturating_sub(4096)..]),
                "output_bytes": self.total,
                "pump_error": pump_error,
            }),
            health,
        )
    }
    pub(crate) fn identity(&self) -> &serde_json::Value {
        &self.identity
    }
    pub(crate) fn observation(&mut self) -> serde_json::Value {
        self.snapshot(false).0
    }
    pub(crate) fn checked_observation(&mut self) -> (serde_json::Value, Result<(), String>) {
        self.snapshot(true)
    }
    /// # Errors
    /// Fails on I/O, receipt collision, process exit, or exceeded execution bounds.
    pub fn check(&mut self) -> Result<(), String> {
        self.pump_recorded(true)?;
        if let Some(status) = self.refresh_exit_observation()? {
            return Err(format!(
                "service exited {status}: {}",
                String::from_utf8_lossy(&self.errors[self.errors.len().saturating_sub(4096)..])
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
            self.pump_recorded(true)?;
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
            return self.stop_error.clone().map_or(Ok(()), Err);
        }
        self.refresh_exit_observation()?;
        if self.observed_exit.is_none() {
            self.signal_group(Signal::SIGTERM)?;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut drain_error = self.runtime_error.clone();
        loop {
            let drained_before = self.total;
            if let Err(error) = self.pump(false) {
                drain_error.get_or_insert(error);
            }
            let progressed = self.total > drained_before;
            self.refresh_exit_observation()?;
            if self.observed_exit.is_some() && self.status.is_none() {
                // The zombie leader pins the process-group identity while any
                // surviving descendants are terminated. Reap only afterwards.
                self.signal_group(Signal::SIGKILL)?;
                self.status = Some(self.reap_child(deadline)?);
            }
            if self.status.is_some() && self.stdout_eof && self.stderr_eof {
                break;
            }
            if Instant::now() >= deadline {
                if self.status.is_none() {
                    self.signal_group(Signal::SIGKILL)?;
                    self.status = Some(self.reap_child(Instant::now() + Duration::from_secs(5))?);
                    self.observed_exit
                        .get_or_insert_with(|| "forced shutdown".into());
                    continue;
                }
                drain_error.get_or_insert_with(|| "service output drain deadline exceeded".into());
                break;
            }
            // Host timer granularity can inflate a fixed sleep by an order of
            // magnitude under load. Keep draining while output is flowing and
            // yield only when both streams are momentarily empty, so the
            // bounded stop deadline is not spent waiting between chunks.
            if !progressed {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let containment_error = self.release_containment().err();
        let stop_error = match (drain_error, containment_error) {
            (Some(drain), Some(containment)) => Some(format!("{drain}; {containment}")),
            (Some(error), None) | (None, Some(error)) => Some(error),
            (None, None) => None,
        };
        let observation = self.observation();
        let state = if stop_error.is_some() {
            "cleanup-failed"
        } else {
            "stopped"
        };
        let publication = self.receipt_directory.publish(
            &self.receipt.with_extension("stopped.json"),
            &json!({
                "state":state,
                "pid":self.child.id(),
                "identity":self.identity,
                "observation":observation,
                "error":stop_error,
            }),
        );
        let result = stop_error.map_or(Ok(()), Err).and(publication);
        self.stop_error = result.as_ref().err().cloned();
        self.stopped = true;
        result
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        if self.stop().is_err() && self.status.is_none() {
            let _ = self.refresh_exit_observation();
            let _ = self.signal_group(Signal::SIGKILL);
            self.status = self
                .reap_child(Instant::now() + Duration::from_secs(5))
                .ok();
            let _ = self.release_containment();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use nix::unistd::Pid;
    use std::{
        fs::File,
        os::unix::process::CommandExt as _,
        process::{Command, Stdio},
    };

    fn write_pid_marker(path: &Path, pid: u32) {
        let mut marker = File::create(path).unwrap();
        write!(marker, "{pid}").unwrap();
        marker.sync_all().unwrap();
        File::open(path.parent().unwrap())
            .unwrap()
            .sync_all()
            .unwrap();
    }

    fn wait_for_pid_marker(path: &Path) -> Pid {
        for _ in 0..500 {
            if let Ok(pid) = std::fs::read_to_string(path) {
                return Pid::from_raw(pid.trim().parse().unwrap());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("owned service descendant did not publish its pid");
    }

    fn wait_until_gone(pid: Pid) -> bool {
        for _ in 0..200 {
            if nix::sys::signal::kill(pid, None) == Err(nix::errno::Errno::ESRCH) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

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

        let owned = tmp.path().join("owned");
        let displaced = tmp.path().join("displaced");
        std::fs::create_dir(&owned).unwrap();
        let receipt = owned.join("service.json");
        let mut service = Service::spawn(&CommandSpec::new("cat"), &receipt).unwrap();
        std::fs::rename(&owned, &displaced).unwrap();
        std::fs::create_dir(&owned).unwrap();
        service.stop().unwrap();
        assert!(!owned.join("service.stopped.json").exists());
        assert!(displaced.join("service.stopped.json").exists());
        assert!(service.status.is_some());
    }
    #[test]
    fn bounded_output_and_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let mut spec = CommandSpec::new("yes");
        spec.output_cap = 64;
        let mut s = Service::spawn(&spec, &tmp.path().join("output.json")).unwrap();
        assert!(s.line(Duration::from_secs(1)).is_err());
        assert!(s.stop().unwrap_err().contains("output cap"));
        assert!(tmp.path().join("output.stopped.json").exists());
        let mut spec = CommandSpec::new("cat");
        spec.timeout = Duration::from_millis(20);
        let mut s = Service::spawn(&spec, &tmp.path().join("timeout.json")).unwrap();
        assert!(s.line(Duration::from_secs(1)).is_err());
    }
    #[test]
    fn reaped_service_identity_is_never_signaled_again() {
        let tmp = tempfile::tempdir().unwrap();
        let mut spec = CommandSpec::new("/bin/sh");
        spec.args = ["-c", "exit 17"].map(Into::into).into();
        let mut service = Service::spawn(&spec, &tmp.path().join("reaped.json")).unwrap();
        for _ in 0..100 {
            if service.check().is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(service.status, None);
        assert!(
            service
                .observed_exit
                .as_deref()
                .is_some_and(|status| status.contains("17"))
        );
        assert_eq!(service.signals_sent, 0);
        service.stop().unwrap();
        assert_eq!(service.status.and_then(|status| status.code()), Some(17));
        let signals = service.signals_sent;
        service.stop().unwrap();
        assert_eq!(service.signals_sent, signals);
    }

    #[test]
    fn exited_leader_cannot_hide_a_surviving_owned_descendant() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("descendant.pid");
        let mut spec = CommandSpec::new("/bin/sh");
        spec.args = [
            "-c".into(),
            format!(
                "(trap '' TERM; exec sleep 30 >/dev/null 2>&1) & echo $! > {}; exit 17",
                marker.display()
            )
            .into(),
        ]
        .into();
        let mut service = Service::spawn(&spec, &tmp.path().join("descendant.json")).unwrap();
        for _ in 0..100 {
            if service.check().is_err() && marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let pid = Pid::from_raw(
            std::fs::read_to_string(&marker)
                .unwrap()
                .trim()
                .parse()
                .unwrap(),
        );
        service.stop().unwrap();
        for _ in 0..100 {
            if nix::sys::signal::kill(pid, None) == Err(nix::errno::Errno::ESRCH) {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = nix::sys::signal::kill(pid, Signal::SIGKILL);
        panic!("owned descendant survived successful cleanup");
    }

    #[test]
    fn stop_terminates_nested_owned_process_groups_after_leader_exit() {
        match std::env::var("PGOKF_SERVICE_NESTED_MODE").as_deref() {
            Ok("descendant") => {
                std::thread::sleep(Duration::from_secs(30));
                return;
            }
            Ok("launcher") => {
                let marker =
                    PathBuf::from(std::env::var_os("PGOKF_SERVICE_NESTED_MARKER").unwrap());
                let mut descendant = Command::new(std::env::current_exe().unwrap());
                descendant
                    .args([
                        "--exact",
                        "service::tests::stop_terminates_nested_owned_process_groups_after_leader_exit",
                        "--nocapture",
                    ])
                    .env("PGOKF_SERVICE_NESTED_MODE", "descendant")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .process_group(0);
                let child = descendant.spawn().unwrap();
                write_pid_marker(&marker, child.id());
                std::process::exit(17);
            }
            _ => {}
        }

        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("descendant.pid");
        let receipt = directory.path().join("service.json");
        let mut spec = CommandSpec::new(std::env::current_exe().unwrap());
        spec.args = [
            "--exact",
            "service::tests::stop_terminates_nested_owned_process_groups_after_leader_exit",
            "--nocapture",
        ]
        .map(Into::into)
        .into();
        spec.env
            .insert("PGOKF_SERVICE_NESTED_MODE".into(), "launcher".into());
        spec.env.insert(
            "PGOKF_SERVICE_NESTED_MARKER".into(),
            marker.as_os_str().into(),
        );
        spec.timeout = Duration::from_secs(2);

        let mut service = Service::spawn(&spec, &receipt).unwrap();
        let descendant = wait_for_pid_marker(&marker);
        for _ in 0..500 {
            if service.check().is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let stop = service.stop();
        let removed = wait_until_gone(descendant);
        if !removed {
            let _ = nix::sys::signal::kill(descendant, Signal::SIGKILL);
        }
        assert!(
            removed,
            "nested owned service process group survived stop: {stop:?}"
        );
    }
}
