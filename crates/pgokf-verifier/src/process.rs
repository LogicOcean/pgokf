//! Bounded argv execution for public verification commands.
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, process::Output, time::Duration};

const PRIVATE_VERIFICATION_ENV: [&str; 9] = [
    "PGOKF_VERIFICATION_RECEIPT",
    "PGOKF_VERIFICATION_NONCE",
    "PGOKF_VERIFICATION_CAMPAIGN",
    "PGOKF_VERIFICATION_CASE",
    "PGOKF_VERIFICATION_DEVICE",
    "PGOKF_VERIFICATION_INODE",
    "PGOKF_VERIFICATION_SERVICE_RECEIPTS",
    "PGOKF_VERIFICATION_SERVICE_RECEIPTS_DEVICE",
    "PGOKF_VERIFICATION_SERVICE_RECEIPTS_INODE",
];

#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<OsString, OsString>,
    /// Remove inherited variables before applying explicit environment overrides.
    pub env_remove: Vec<OsString>,
    pub stdin: Vec<u8>,
    pub timeout: Duration,
    pub output_cap: usize,
    /// Create-new files that receive fsynced stream chunks while the command runs.
    pub durable_stdout: Option<PathBuf>,
    pub durable_stderr: Option<PathBuf>,
    /// Explicitly delegate the containing launchd broker to this command.
    ///
    /// This is authority to create separately contained nested jobs and must
    /// only be granted to trusted verifier workers, never arbitrary tools.
    pub delegate_nested_supervision: bool,
}

/// Machinery-controlled drain chunk for every supervised command stream.
///
/// Contained stdio under launchd is an OS-fixed 8 KiB FIFO (measured 8,192
/// bytes on this host; there is no macOS knob to enlarge it), while the
/// pre-containment `std::process` pipes this code was measured under hold
/// 65,536 bytes. No behavior may depend on a stdio capacity below the
/// pre-containment size: the supervised pump keeps the bounded streams
/// drained, and every buffer this machinery controls is exactly the
/// pre-containment pipe capacity, never a platform default.
pub(crate) const COMMAND_DRAIN_CHUNK: usize = 64 * 1024;

impl CommandSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            env_remove: PRIVATE_VERIFICATION_ENV.map(OsString::from).into(),
            stdin: Vec::new(),
            timeout: Duration::from_mins(2),
            output_cap: 16 * 1024 * 1024,
            durable_stdout: None,
            durable_stderr: None,
            delegate_nested_supervision: false,
        }
    }

    /// Execute without a shell, retaining at most `output_cap` bytes per stream.
    /// # Errors
    /// Spawn, I/O, timeout, unsupported platform, and output overflow fail closed.
    pub fn run(&self) -> Result<Output, String> {
        #[cfg(unix)]
        {
            unix::run(self, &mut || Ok(()))
                .map_err(|error| format!("{}: {error}", self.program.display()))
        }
        #[cfg(not(unix))]
        {
            Err("bounded process groups require Unix".into())
        }
    }

    /// Keep an owned service supervised while a dependent command is running.
    pub(crate) fn checked_supervised(
        &self,
        supervise: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<Output, String> {
        let output = unix::run(self, supervise)
            .map_err(|error| format!("{}: {error}", self.program.display()))?;
        if !output.status.success() {
            return Err(format!(
                "{}: {}: {}",
                self.program.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(output)
    }

    /// Execute without a shell while a supervise closure keeps owned services
    /// drained, returning the command's own exit status unexamined.
    /// # Errors
    /// Spawn, I/O, supervise, timeout, and cleanup failures fail closed.
    pub(crate) fn run_supervised(
        &self,
        supervise: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<Output, String> {
        #[cfg(unix)]
        {
            unix::run(self, supervise)
                .map_err(|error| format!("{}: {error}", self.program.display()))
        }
        #[cfg(not(unix))]
        {
            let _ = supervise;
            Err("bounded process groups require Unix".into())
        }
    }

    /// Execute and require a successful status.
    /// # Errors
    /// Process errors and nonzero exit status are verification failures.
    pub fn checked(&self) -> Result<Output, String> {
        let output = self.run()?;
        if !output.status.success() {
            return Err(format!(
                "{}: {}: {}",
                self.program.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(output)
    }
}

#[cfg(target_os = "macos")]
pub(crate) mod launchd;

#[cfg(unix)]
#[cfg_attr(target_os = "macos", allow(dead_code, unused_imports))]
mod unix {
    use super::{CommandSpec, Duration, Output};
    use nix::{
        fcntl::{FcntlArg, OFlag, fcntl},
        sys::signal::{Signal, killpg},
        unistd::Pid,
    };
    use rustix::process::{Pid as RustixPid, WaitId, WaitIdOptions, waitid};
    use std::{
        collections::{BTreeSet, HashMap},
        fs::{File, OpenOptions},
        io::{Read, Seek, Write},
        os::fd::AsFd,
        process::{Child, Command, ExitStatus},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread::JoinHandle,
        time::Instant,
    };
    #[cfg(not(target_os = "macos"))]
    use std::{os::unix::process::CommandExt, process::Stdio};
    use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};

    const PROCESS_OWNER_ENV: &str = "PGOKF_PROCESS_OWNER";
    const PROCESS_GATE_ENV: &str = "PGOKF_PROCESS_GATE";
    const PROCESS_GATE_SCRIPT: &str =
        "while [ ! -e \"$PGOKF_PROCESS_GATE\" ]; do :; done; exec \"$@\"";

    type ProcessStart = u64;

    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    struct ProcessIdentity {
        pid: u32,
        started: ProcessStart,
    }

    fn process_start(_: u32, process: &sysinfo::Process) -> ProcessStart {
        process.start_time()
    }

    fn identity_handle_after_authentication<H: Eq>(
        capture: &mut impl FnMut() -> Result<Option<H>, String>,
        authenticate: impl FnOnce() -> bool,
        unauthenticated_error: impl FnOnce() -> String,
    ) -> Result<Option<H>, String> {
        let Some(before) = capture()? else {
            return Ok(None);
        };
        let authenticated = authenticate();
        let Some(after) = capture()? else {
            return Ok(None);
        };
        if before != after {
            return Ok(None);
        }
        if !authenticated {
            return Err(unauthenticated_error());
        }
        Ok(Some(before))
    }

    #[cfg(target_os = "macos")]
    mod identity_signal {
        use nix::sys::signal::Signal;
        use pgokf_process_handle::{ProcessHandle, SignalResult};

        #[derive(Debug, Eq, PartialEq)]
        pub(super) struct IdentityHandle(ProcessHandle);

        impl IdentityHandle {
            pub(super) fn capture(pid: u32) -> Result<Option<Self>, String> {
                ProcessHandle::capture(pid).map(|handle| handle.map(Self))
            }

            pub(super) fn signal(self, signal: Signal) -> Result<(), String> {
                match self.0.signal(signal as i32)? {
                    SignalResult::Delivered | SignalResult::Gone => Ok(()),
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    mod identity_signal {
        use nix::sys::signal::Signal;
        use rustix::process::{
            Pid, PidfdFlags, Signal as RustixSignal, pidfd_open, pidfd_send_signal,
        };
        use std::{
            fs,
            os::fd::{AsRawFd as _, OwnedFd},
        };

        #[derive(Debug)]
        pub(super) struct IdentityHandle {
            pid: u32,
            started: u64,
            fd: OwnedFd,
        }

        impl PartialEq for IdentityHandle {
            fn eq(&self, other: &Self) -> bool {
                self.pid == other.pid && self.started == other.started
            }
        }

        impl Eq for IdentityHandle {}

        impl IdentityHandle {
            pub(super) fn capture(pid: u32) -> Result<Option<Self>, String> {
                let raw = i32::try_from(pid).map_err(|error| error.to_string())?;
                let pid_value = Pid::from_raw(raw).ok_or("process pid must be positive")?;
                let fd = match pidfd_open(pid_value, PidfdFlags::empty()) {
                    Ok(fd) => fd,
                    Err(error) if error == rustix::io::Errno::SRCH => return Ok(None),
                    Err(error) => return Err(format!("pidfd_open failed for {pid}: {error}")),
                };
                let started = match process_start_ticks(pid) {
                    Ok(started) => started,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => {
                        return Err(format!("failed to read process {pid} start: {error}"));
                    }
                };
                let fdinfo = fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd()))
                    .map_err(|error| format!("failed to inspect pidfd for {pid}: {error}"))?;
                let bound_pid = fdinfo
                    .lines()
                    .find_map(|line| line.strip_prefix("Pid:\t"))
                    .ok_or_else(|| format!("pidfd for {pid} omitted its bound pid"))?;
                if bound_pid == "-1" {
                    return Ok(None);
                }
                if bound_pid.parse::<u32>().ok() != Some(pid) {
                    return Err(format!(
                        "pidfd for {pid} resolved to unexpected pid {bound_pid}"
                    ));
                }
                Ok(Some(Self { pid, started, fd }))
            }

            pub(super) fn signal(self, signal: Signal) -> Result<(), String> {
                let signal = match signal {
                    Signal::SIGKILL => RustixSignal::KILL,
                    Signal::SIGTERM => RustixSignal::TERM,
                    other => return Err(format!("unsupported identity-bound signal {other:?}")),
                };
                match pidfd_send_signal(&self.fd, signal) {
                    Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
                    Err(error) => Err(format!("pidfd signal failed for {}: {error}", self.pid)),
                }
            }
        }

        fn process_start_ticks(pid: u32) -> std::io::Result<u64> {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
            let fields = stat
                .rsplit_once(") ")
                .ok_or_else(|| std::io::Error::other("malformed proc stat"))?
                .1
                .split_whitespace()
                .collect::<Vec<_>>();
            fields
                .get(19)
                .ok_or_else(|| std::io::Error::other("proc stat omitted start time"))?
                .parse()
                .map_err(std::io::Error::other)
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    mod identity_signal {
        use nix::sys::signal::Signal;

        #[derive(Debug, Eq, PartialEq)]
        pub(super) struct IdentityHandle;

        impl IdentityHandle {
            pub(super) fn capture(_: u32) -> Result<Option<Self>, String> {
                Err("identity-bound process signaling is unsupported on this platform".into())
            }

            pub(super) fn signal(self, _: Signal) -> Result<(), String> {
                Err("identity-bound process signaling is unsupported on this platform".into())
            }
        }
    }

    use identity_signal::IdentityHandle;

    /// A per-command ownership tag and pre-exec gate. The gate keeps the root
    /// process from forking until lineage monitoring is active. The environment
    /// tag is a second, independent way to rediscover descendants after reparenting.
    pub(crate) struct ProcessOwner {
        token: String,
        environment: std::ffi::OsString,
        gate: tempfile::TempDir,
    }

    impl ProcessOwner {
        pub(crate) fn new() -> Result<Self, String> {
            let token = uuid::Uuid::new_v4().to_string();
            let mut environment = std::env::var_os(PROCESS_OWNER_ENV).unwrap_or_default();
            if !environment.is_empty() {
                environment.push(":");
            }
            environment.push(&token);
            Ok(Self {
                token,
                environment,
                gate: tempfile::tempdir().map_err(|error| error.to_string())?,
            })
        }

        pub(crate) fn command(&self, spec: &CommandSpec) -> Command {
            let mut command = Command::new("/bin/sh");
            for name in &spec.env_remove {
                command.env_remove(name);
            }
            command
                .arg("-c")
                .arg(PROCESS_GATE_SCRIPT)
                .arg("pgokf-process-gate")
                .arg(&spec.program)
                .args(&spec.args)
                .envs(&spec.env)
                // Apply after user-supplied entries so callers cannot replace
                // either ownership control.
                .env(PROCESS_OWNER_ENV, &self.environment)
                .env(PROCESS_GATE_ENV, self.gate.path().join("release"));
            if let Some(cwd) = &spec.cwd {
                command.current_dir(cwd);
            }
            command
        }

        pub(crate) fn monitor(&self, root: u32) -> Result<ProcessMonitor, String> {
            ProcessMonitor::new(root, self.token.clone())
        }

        pub(crate) fn release(&self) -> Result<(), String> {
            std::fs::write(self.gate.path().join("release"), b"release")
                .map_err(|error| error.to_string())
        }
    }

    fn scan_owned_processes(
        root: Option<u32>,
        token: &str,
        tracked: &Arc<Mutex<BTreeSet<ProcessIdentity>>>,
        include_environment: bool,
    ) -> Vec<ProcessIdentity> {
        let mut system = System::new();
        let refresh = if include_environment {
            ProcessRefreshKind::nothing().with_environ(UpdateKind::Always)
        } else {
            ProcessRefreshKind::nothing()
        };
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        let current = system
            .processes()
            .iter()
            .map(|(pid, process)| {
                let pid = pid.as_u32();
                (
                    pid,
                    (
                        process_start(pid, process),
                        process.parent().map(sysinfo::Pid::as_u32),
                        process.status(),
                    ),
                )
            })
            .collect::<HashMap<_, _>>();
        let mut identities = tracked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(root) = root
            && let Some((started, _, _)) = current.get(&root)
        {
            identities.insert(ProcessIdentity {
                pid: root,
                started: *started,
            });
        }
        loop {
            let additions = current
                .iter()
                .filter_map(|(pid, (started, parent, _))| {
                    let parent = *parent.as_ref()?;
                    let (parent_started, _, _) = current.get(&parent)?;
                    identities
                        .contains(&ProcessIdentity {
                            pid: parent,
                            started: *parent_started,
                        })
                        .then_some(ProcessIdentity {
                            pid: *pid,
                            started: *started,
                        })
                })
                .filter(|identity| !identities.contains(identity))
                .collect::<Vec<_>>();
            if additions.is_empty() {
                break;
            }
            identities.extend(additions);
        }
        if include_environment {
            let prefix = format!("{PROCESS_OWNER_ENV}=");
            identities.extend(
                system
                    .processes()
                    .iter()
                    .filter(|(_, process)| {
                        process
                            .environ()
                            .iter()
                            .filter_map(|entry| entry.to_str())
                            .filter_map(|entry| entry.strip_prefix(&prefix))
                            .any(|owners| owners.split(':').any(|owner| owner == token))
                    })
                    .map(|(pid, process)| {
                        let pid = pid.as_u32();
                        ProcessIdentity {
                            pid,
                            started: process_start(pid, process),
                        }
                    }),
            );
        }
        identities
            .iter()
            .filter_map(|identity| {
                current
                    .get(&identity.pid)
                    .is_some_and(|(current_started, _, status)| {
                        current_started == &identity.started && *status != ProcessStatus::Zombie
                    })
                    .then_some(*identity)
            })
            .collect()
    }

    #[cfg(target_os = "macos")]
    fn process_watcher(pid: u32) -> Result<Option<kqueue::Watcher>, String> {
        let mut process_watcher = kqueue::Watcher::new().map_err(|error| error.to_string())?;
        let raw_pid = i32::try_from(pid).map_err(|error| error.to_string())?;
        if let Err(error) = process_watcher.add_pid(
            raw_pid,
            kqueue::EventFilter::EVFILT_PROC,
            kqueue::FilterFlag::NOTE_FORK | kqueue::FilterFlag::NOTE_EXIT,
        ) {
            return if error.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32) {
                Ok(None)
            } else {
                Err(error.to_string())
            };
        }
        if let Err(error) = process_watcher.watch() {
            return if error.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32) {
                Ok(None)
            } else {
                Err(error.to_string())
            };
        }
        Ok(Some(process_watcher))
    }

    #[cfg(target_os = "macos")]
    fn poll_process_watchers(process_watchers: &[(u32, kqueue::Watcher)]) -> Result<bool, String> {
        let mut events = Vec::new();
        if let Some((_, root_watcher)) = process_watchers.first()
            && let Some(event) = root_watcher.poll(Some(Duration::from_millis(50)))
        {
            events.push(event);
        }
        events.extend(
            process_watchers
                .iter()
                .skip(1)
                .filter_map(|(_, process_watcher)| process_watcher.poll(None)),
        );
        for event in &events {
            if let kqueue::EventData::Error(error) = &event.data {
                return Err(error.to_string());
            }
        }
        Ok(!events.is_empty())
    }

    #[cfg(target_os = "macos")]
    fn monitor_owned_processes(
        root: u32,
        token: &str,
        tracked: &Arc<Mutex<BTreeSet<ProcessIdentity>>>,
        stop: &Arc<AtomicBool>,
        root_watcher: kqueue::Watcher,
    ) -> Result<(), String> {
        let mut process_watchers = vec![(root, root_watcher)];
        while !stop.load(Ordering::Acquire) {
            if !poll_process_watchers(&process_watchers)? {
                continue;
            }
            let live = scan_owned_processes(None, token, tracked, false);
            let registered_pids = process_watchers
                .iter()
                .map(|(pid, _)| *pid)
                .collect::<BTreeSet<_>>();
            for pid in live
                .into_iter()
                .map(|identity| identity.pid)
                .filter(|pid| !registered_pids.contains(pid))
            {
                if let Some(new_watcher) = process_watcher(pid)? {
                    process_watchers.push((pid, new_watcher));
                }
            }
        }
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    fn monitor_owned_processes(
        token: &str,
        tracked: &Arc<Mutex<BTreeSet<ProcessIdentity>>>,
        stop: &Arc<AtomicBool>,
    ) {
        let started = Instant::now();
        while !stop.load(Ordering::Acquire) {
            scan_owned_processes(None, token, tracked, false);
            let delay = if started.elapsed() < Duration::from_millis(500) {
                Duration::from_millis(5)
            } else {
                Duration::from_millis(50)
            };
            std::thread::sleep(delay);
        }
    }

    pub(crate) struct ProcessMonitor {
        token: String,
        tracked: Arc<Mutex<BTreeSet<ProcessIdentity>>>,
        stop: Arc<AtomicBool>,
        error: Arc<Mutex<Option<String>>>,
        thread: Option<JoinHandle<()>>,
    }

    impl ProcessMonitor {
        fn new(root: u32, token: String) -> Result<Self, String> {
            if !sysinfo::IS_SUPPORTED_SYSTEM {
                return Err("process containment inspection is unsupported".into());
            }
            let tracked = Arc::new(Mutex::new(BTreeSet::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let error = Arc::new(Mutex::new(None));
            if !scan_owned_processes(Some(root), &token, &tracked, true)
                .iter()
                .any(|identity| identity.pid == root)
            {
                return Err("process containment could not authenticate the gated root".into());
            }
            #[cfg(target_os = "macos")]
            let root_watcher = process_watcher(root)?
                .ok_or_else(|| "process containment lost the gated root".to_string())?;
            let thread_tracked = Arc::clone(&tracked);
            let thread_stop = Arc::clone(&stop);
            #[cfg(target_os = "macos")]
            let thread_error = Arc::clone(&error);
            let thread_token = token.clone();
            let thread = std::thread::Builder::new()
                .name("pgokf-process-containment".into())
                .spawn(move || {
                    #[cfg(target_os = "macos")]
                    if let Err(monitor_error) = monitor_owned_processes(
                        root,
                        &thread_token,
                        &thread_tracked,
                        &thread_stop,
                        root_watcher,
                    ) {
                        *thread_error
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(monitor_error);
                    }
                    #[cfg(not(target_os = "macos"))]
                    monitor_owned_processes(&thread_token, &thread_tracked, &thread_stop);
                })
                .map_err(|error| error.to_string())?;
            Ok(Self {
                token,
                tracked,
                stop,
                error,
                thread: Some(thread),
            })
        }

        fn stop_monitor(&mut self) -> Result<(), String> {
            #[cfg(target_os = "macos")]
            std::thread::sleep(Duration::from_millis(25));
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                thread
                    .join()
                    .map_err(|_| "process containment monitor panicked".to_string())?;
            }
            if let Some(error) = self
                .error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                return Err(format!("process containment monitor failed: {error}"));
            }
            Ok(())
        }

        fn survivors(&self) -> Vec<ProcessIdentity> {
            let mut survivors = scan_owned_processes(None, &self.token, &self.tracked, true);
            survivors.sort_unstable();
            survivors.dedup();
            survivors
        }

        fn token_authenticated(&self) -> BTreeSet<ProcessIdentity> {
            let authenticated = Arc::new(Mutex::new(BTreeSet::new()));
            scan_owned_processes(None, &self.token, &authenticated, true)
                .into_iter()
                .collect()
        }

        fn authenticated_handle(
            &self,
            identity: ProcessIdentity,
        ) -> Result<Option<IdentityHandle>, String> {
            identity_handle_after_authentication(
                &mut || IdentityHandle::capture(identity.pid),
                || self.token_authenticated().contains(&identity),
                || format!("process containment refused unauthenticated survivor: {identity:?}"),
            )
        }

        pub(crate) fn terminate_and_ensure_released(&mut self) -> Result<(), String> {
            let monitor_error = self.stop_monitor().err();
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut empty_since = None;
            loop {
                let survivors = self.survivors();
                if survivors.is_empty() {
                    let observed_empty = *empty_since.get_or_insert_with(Instant::now);
                    if observed_empty.elapsed() >= Duration::from_millis(250) {
                        return monitor_error.map_or(Ok(()), Err);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                empty_since = None;
                let mut pass_errors = Vec::new();
                for identity in &survivors {
                    match self.authenticated_handle(*identity) {
                        Ok(Some(handle)) => {
                            if let Err(error) = handle.signal(Signal::SIGKILL) {
                                pass_errors.push(error);
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            pass_errors.push(error);
                        }
                    }
                }
                if !pass_errors.is_empty() {
                    let error = pass_errors.join("; ");
                    return Err(monitor_error
                        .map_or(error.clone(), |monitor| format!("{monitor}; {error}")));
                }
                if Instant::now() >= deadline {
                    let error = format!(
                        "process containment cleanup timed out; owned descendants remain: {survivors:?}"
                    );
                    return Err(monitor_error
                        .map_or(error.clone(), |monitor| format!("{monitor}; {error}")));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for ProcessMonitor {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    struct OwnedChild {
        child: Child,
        group: Pid,
        status: Option<ExitStatus>,
        #[cfg(test)]
        signals_sent: usize,
    }
    impl OwnedChild {
        fn new(child: Child, group: Pid) -> Self {
            Self {
                child,
                group,
                status: None,
                #[cfg(test)]
                signals_sent: 0,
            }
        }
        fn observe_exit(&self) -> Result<bool, String> {
            if self.status.is_some() {
                return Ok(true);
            }
            waitid(
                WaitId::Pid(RustixPid::from_child(&self.child)),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            )
            .map(|status| status.is_some())
            .map_err(|error| error.to_string())
        }
        fn signal_group(&mut self, signal: Signal) {
            if self.status.is_none() {
                #[cfg(test)]
                {
                    self.signals_sent += 1;
                }
                let _ = killpg(self.group, signal);
            }
        }
        fn reap(&mut self) -> Result<ExitStatus, String> {
            if let Some(status) = self.status {
                return Ok(status);
            }
            let status = self.child.wait().map_err(|error| error.to_string())?;
            self.status = Some(status);
            Ok(status)
        }
    }
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if self.status.is_none() {
                // Keep the leader unreaped so the process-group identity stays
                // pinned across the final non-reaping observation and signal.
                let _ = self.observe_exit();
                self.signal_group(Signal::SIGKILL);
                self.status = self.child.wait().ok();
            }
        }
    }

    fn nonblocking(fd: &impl AsFd) -> Result<(), String> {
        let flags = fcntl(fd, FcntlArg::F_GETFL).map_err(|e| e.to_string())?;
        fcntl(
            fd,
            FcntlArg::F_SETFL(OFlag::from_bits_retain(flags) | OFlag::O_NONBLOCK),
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum DrainState {
        Data,
        Pending,
        Eof,
    }

    fn durable_file(path: Option<&std::path::Path>) -> Result<Option<File>, String> {
        let Some(path) = path else {
            return Ok(None);
        };
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        if let Some(parent) = path.parent() {
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| error.to_string())?;
        }
        Ok(Some(file))
    }

    fn drain(
        reader: &mut impl Read,
        output: &mut Vec<u8>,
        cap: usize,
        mut durable: Option<&mut File>,
    ) -> Result<DrainState, String> {
        // One bounded read per iteration ensures a noisy stream cannot starve
        // its sibling or bypass the deadline. The chunk is the explicit
        // pre-containment pipe capacity, never a platform default.
        let mut bytes = vec![0_u8; super::COMMAND_DRAIN_CHUNK];
        match reader.read(&mut bytes) {
            Ok(0) => Ok(DrainState::Eof),
            Ok(count) => {
                if count > cap.saturating_sub(output.len()) {
                    return Err("output cap exceeded".into());
                }
                output.extend_from_slice(&bytes[..count]);
                if let Some(file) = durable.as_mut() {
                    file.write_all(&bytes[..count])
                        .and_then(|()| file.sync_data())
                        .map_err(|error| error.to_string())?;
                }
                Ok(DrainState::Data)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(DrainState::Pending)
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn drain_to_eof(
        stdout: &mut impl Read,
        stderr: &mut impl Read,
        output: (&mut Vec<u8>, &mut Vec<u8>),
        durable: (&mut Option<File>, &mut Option<File>),
        cap: usize,
        start: Instant,
        timeout: Duration,
    ) -> Result<(), String> {
        let (out, err) = output;
        let (durable_stdout, durable_stderr) = durable;
        let mut stdout_eof = false;
        let mut stderr_eof = false;
        while !(stdout_eof && stderr_eof) {
            if !stdout_eof {
                stdout_eof = drain(stdout, out, cap, durable_stdout.as_mut())? == DrainState::Eof;
            }
            if !stderr_eof {
                stderr_eof = drain(stderr, err, cap, durable_stderr.as_mut())? == DrainState::Eof;
            }
            if start.elapsed() >= timeout {
                return Err("timeout exceeded while draining".into());
            }
            if !(stdout_eof && stderr_eof) {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        Ok(())
    }

    fn prepared_input(spec: &CommandSpec) -> Result<File, String> {
        if spec.timeout.is_zero() || spec.output_cap == 0 {
            return Err("positive timeout and output cap required".into());
        }
        let mut input = tempfile::tempfile().map_err(|e| e.to_string())?;
        input.write_all(&spec.stdin).map_err(|e| e.to_string())?;
        input.rewind().map_err(|e| e.to_string())?;
        Ok(input)
    }

    fn finish_execution(
        execution: Result<ExitStatus, String>,
        cleanup_errors: &[String],
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    ) -> Result<Output, String> {
        match execution {
            Ok(status) if cleanup_errors.is_empty() => Ok(Output {
                status,
                stdout,
                stderr,
            }),
            Ok(_) => Err(cleanup_errors.join("; ")),
            Err(error) if cleanup_errors.is_empty() => Err(error),
            Err(error) => Err(format!(
                "{error}; cleanup failed: {}",
                cleanup_errors.join("; ")
            )),
        }
    }

    fn spawn_owned(mut command: Command) -> Result<OwnedChild, String> {
        let child = command.spawn().map_err(|error| error.to_string())?;
        let group = Pid::from_raw(i32::try_from(child.id()).map_err(|error| error.to_string())?);
        Ok(OwnedChild::new(child, group))
    }

    #[cfg(target_os = "macos")]
    #[allow(clippy::too_many_arguments)]
    fn drain_grace(
        stdout: &mut impl Read,
        stderr: &mut impl Read,
        stdout_eof: &mut bool,
        stderr_eof: &mut bool,
        out: &mut Vec<u8>,
        err: &mut Vec<u8>,
        durable_stdout: &mut Option<File>,
        durable_stderr: &mut Option<File>,
        cap: usize,
        deadline: Instant,
    ) -> Result<(), String> {
        while !(*stdout_eof && *stderr_eof) && Instant::now() < deadline {
            if !*stdout_eof {
                *stdout_eof = drain(stdout, out, cap, durable_stdout.as_mut())? == DrainState::Eof;
            }
            if !*stderr_eof {
                *stderr_eof = drain(stderr, err, cap, durable_stderr.as_mut())? == DrainState::Eof;
            }
            if !(*stdout_eof && *stderr_eof) {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[allow(clippy::too_many_lines)] // Keep the bounded spawn, I/O, and cleanup lifecycle linear.
    pub(super) fn run(
        spec: &CommandSpec,
        supervise: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<Output, String> {
        use super::launchd::LaunchdChild;

        let start = Instant::now();
        let deadline = start + spec.timeout;
        if spec.timeout.is_zero() || spec.output_cap == 0 {
            return Err("positive timeout and output cap required".into());
        }
        let mut durable_stdout = durable_file(spec.durable_stdout.as_deref())?;
        let mut durable_stderr = durable_file(spec.durable_stderr.as_deref())?;
        let mut owned = LaunchdChild::spawn(spec, deadline)?;
        let mut stdout = owned.take_stdout()?;
        let mut stderr = owned.take_stderr()?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let execution = (|| {
            nonblocking(&stdout)?;
            nonblocking(&stderr)?;
            let mut stdout_eof = false;
            let mut stderr_eof = false;
            loop {
                if !stdout_eof {
                    stdout_eof = drain(
                        &mut stdout,
                        &mut out,
                        spec.output_cap,
                        durable_stdout.as_mut(),
                    )? == DrainState::Eof;
                }
                if !stderr_eof {
                    stderr_eof = drain(
                        &mut stderr,
                        &mut err,
                        spec.output_cap,
                        durable_stderr.as_mut(),
                    )? == DrainState::Eof;
                }
                supervise().map_err(|error| {
                    format!(
                        "{error}; dependent child {} interrupted; stderr tail: {}",
                        owned.id(),
                        String::from_utf8_lossy(&err[err.len().saturating_sub(4096)..])
                    )
                })?;
                if owned.observe_exit()? {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "timeout exceeded; stderr tail: {}",
                        String::from_utf8_lossy(&err[err.len().saturating_sub(4096)..])
                    ));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            let status = owned.wait(deadline)?;
            let grace_deadline = (Instant::now() + Duration::from_millis(250)).min(deadline);
            drain_grace(
                &mut stdout,
                &mut stderr,
                &mut stdout_eof,
                &mut stderr_eof,
                &mut out,
                &mut err,
                &mut durable_stdout,
                &mut durable_stderr,
                spec.output_cap,
                grace_deadline,
            )?;
            owned.signal_group(Signal::SIGKILL)?;
            drain_to_eof(
                &mut stdout,
                &mut stderr,
                (&mut out, &mut err),
                (&mut durable_stdout, &mut durable_stderr),
                spec.output_cap,
                start,
                spec.timeout,
            )?;
            Ok(status)
        })();
        let mut cleanup_errors = Vec::new();
        if execution.is_err() {
            if let Err(error) = owned.signal_group(Signal::SIGKILL) {
                cleanup_errors.push(format!("coalition signal failed: {error}"));
            }
            if owned.observed_status().is_none()
                && let Err(error) = owned.wait(Instant::now() + Duration::from_secs(5))
            {
                cleanup_errors.push(format!("target cleanup wait failed: {error}"));
            }
            if let Err(error) = drain_to_eof(
                &mut stdout,
                &mut stderr,
                (&mut out, &mut err),
                (&mut durable_stdout, &mut durable_stderr),
                spec.output_cap,
                Instant::now(),
                Duration::from_millis(250),
            ) {
                cleanup_errors.push(format!("final output drain failed: {error}"));
            }
        }
        if let Err(error) = owned.terminate_and_ensure_released() {
            cleanup_errors.push(error);
        }
        finish_execution(execution, &cleanup_errors, out, err)
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn run(
        spec: &CommandSpec,
        supervise: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<Output, String> {
        run_lineage(spec, supervise)
    }

    #[cfg(not(target_os = "macos"))]
    fn run_lineage(
        spec: &CommandSpec,
        supervise: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<Output, String> {
        let start = Instant::now();
        let input = prepared_input(spec)?;
        let mut durable_stdout = durable_file(spec.durable_stdout.as_deref())?;
        let mut durable_stderr = durable_file(spec.durable_stderr.as_deref())?;
        let process_owner = ProcessOwner::new()?;
        let mut command = process_owner.command(spec);
        command
            .stdin(input)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut owned = spawn_owned(command)?;
        let mut monitor = process_owner.monitor(owned.child.id())?;
        let mut stdout = owned.child.stdout.take().ok_or("missing stdout pipe")?;
        let mut stderr = owned.child.stderr.take().ok_or("missing stderr pipe")?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let execution = (|| {
            process_owner.release()?;
            nonblocking(&stdout)?;
            nonblocking(&stderr)?;
            let mut stdout_eof = false;
            let mut stderr_eof = false;
            loop {
                if !stdout_eof {
                    stdout_eof = drain(
                        &mut stdout,
                        &mut out,
                        spec.output_cap,
                        durable_stdout.as_mut(),
                    )? == DrainState::Eof;
                }
                if !stderr_eof {
                    stderr_eof = drain(
                        &mut stderr,
                        &mut err,
                        spec.output_cap,
                        durable_stderr.as_mut(),
                    )? == DrainState::Eof;
                }
                supervise().map_err(|error| {
                    format!(
                        "{error}; dependent child {} interrupted; stderr tail: {}",
                        owned.child.id(),
                        String::from_utf8_lossy(&err[err.len().saturating_sub(4096)..])
                    )
                })?;
                if owned.observe_exit()? {
                    break;
                }
                if start.elapsed() >= spec.timeout {
                    return Err(format!(
                        "timeout exceeded; stderr tail: {}",
                        String::from_utf8_lossy(&err[err.len().saturating_sub(4096)..])
                    ));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            // The unreaped leader pins the process-group identity while owned
            // descendants are terminated. Reap only after the final group signal.
            owned.signal_group(Signal::SIGKILL);
            let status = owned.reap()?;
            drain_to_eof(
                &mut stdout,
                &mut stderr,
                (&mut out, &mut err),
                (&mut durable_stdout, &mut durable_stderr),
                spec.output_cap,
                start,
                spec.timeout,
            )?;
            Ok(status)
        })();
        let mut cleanup_errors = Vec::new();
        if execution.is_err() && owned.status.is_none() {
            owned.signal_group(Signal::SIGKILL);
            if let Err(error) = owned.reap() {
                cleanup_errors.push(format!("process-group cleanup failed: {error}"));
            }
        }
        if execution.is_err()
            && let Err(error) = drain_to_eof(
                &mut stdout,
                &mut stderr,
                (&mut out, &mut err),
                (&mut durable_stdout, &mut durable_stderr),
                spec.output_cap,
                Instant::now(),
                Duration::from_millis(250),
            )
        {
            cleanup_errors.push(format!("final output drain failed: {error}"));
        }
        let containment = monitor.terminate_and_ensure_released();
        if let Err(error) = containment {
            cleanup_errors.push(error);
        }
        finish_execution(execution, &cleanup_errors, out, err)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::process::CommandExt as _;

        #[test]
        fn identity_change_during_authentication_is_never_signaled() {
            let mut captures = [Some(1_u8), Some(2_u8)].into_iter();
            let handle = identity_handle_after_authentication(
                &mut || Ok(captures.next().expect("capture requested too many times")),
                || true,
                || "untrusted test process".into(),
            )
            .unwrap();
            assert_eq!(handle, None);
        }

        #[test]
        fn authentication_miss_after_identity_change_is_retryable() {
            let mut captures = [Some(1_u8), Some(2_u8)].into_iter();
            let handle = identity_handle_after_authentication(
                &mut || Ok(captures.next().expect("capture requested too many times")),
                || false,
                || "untrusted test process".into(),
            )
            .unwrap();
            assert_eq!(handle, None);
            assert_eq!(captures.next(), None, "identity was not recaptured");
        }

        #[test]
        fn stable_unauthenticated_identity_is_refused() {
            let mut captures = [Some(1_u8), Some(1_u8)].into_iter();
            let error = identity_handle_after_authentication(
                &mut || Ok(captures.next().expect("capture requested too many times")),
                || false,
                || "untrusted test process".into(),
            )
            .unwrap_err();
            assert_eq!(error, "untrusted test process");
        }

        #[test]
        fn reaped_command_identity_is_never_signaled_again() {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "exit 0"]).process_group(0);
            let child = command.spawn().unwrap();
            let group = Pid::from_raw(i32::try_from(child.id()).unwrap());
            let mut owned = OwnedChild::new(child, group);
            for _ in 0..100 {
                if owned.observe_exit().unwrap() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(owned.observe_exit().unwrap());
            owned.reap().unwrap();
            let signals = owned.signals_sent;
            owned.signal_group(Signal::SIGKILL);
            assert_eq!(owned.signals_sent, signals);
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) use unix::{ProcessMonitor, ProcessOwner};

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    use std::{
        fs::{self, File},
        io::Write as _,
        os::unix::process::CommandExt as _,
        process::{Command, Stdio},
        time::Instant,
    };

    fn shell(source: &str) -> CommandSpec {
        let mut spec = CommandSpec::new("/bin/sh");
        spec.args = ["-c", source].map(OsString::from).to_vec();
        spec.timeout = Duration::from_secs(2);
        spec
    }

    fn write_pid_marker(path: &std::path::Path, pid: u32) {
        let mut marker = File::create(path).unwrap();
        write!(marker, "{pid}").unwrap();
        marker.sync_all().unwrap();
        File::open(path.parent().unwrap())
            .unwrap()
            .sync_all()
            .unwrap();
    }

    fn wait_for_pid_marker(path: &std::path::Path) -> Pid {
        for _ in 0..500 {
            if let Ok(pid) = fs::read_to_string(path) {
                return Pid::from_raw(pid.trim().parse().unwrap());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("owned descendant did not publish its pid");
    }

    fn wait_until_gone(pid: Pid) -> bool {
        for _ in 0..200 {
            if kill(pid, None) == Err(nix::errno::Errno::ESRCH) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn ordinary_exit_terminates_nested_owned_process_groups() {
        match std::env::var("PGOKF_ORDINARY_EXIT_MODE").as_deref() {
            Ok("descendant") => {
                std::thread::sleep(Duration::from_secs(30));
                return;
            }
            Ok("launcher") => {
                let marker = PathBuf::from(std::env::var_os("PGOKF_ORDINARY_EXIT_MARKER").unwrap());
                let mut descendant = Command::new(std::env::current_exe().unwrap());
                descendant
                    .args([
                        "--exact",
                        "process::tests::ordinary_exit_terminates_nested_owned_process_groups",
                        "--nocapture",
                    ])
                    .env("PGOKF_ORDINARY_EXIT_MODE", "descendant")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .process_group(0);
                let child = descendant.spawn().unwrap();
                write_pid_marker(&marker, child.id());
                let code = std::env::var("PGOKF_ORDINARY_EXIT_CODE")
                    .unwrap()
                    .parse()
                    .unwrap();
                std::process::exit(code);
            }
            Ok("borrowed-launcher") => {
                let mut spec = CommandSpec::new(std::env::current_exe().unwrap());
                spec.args = [
                    "--exact",
                    "process::tests::ordinary_exit_terminates_nested_owned_process_groups",
                    "--nocapture",
                ]
                .map(OsString::from)
                .to_vec();
                spec.env
                    .insert("PGOKF_ORDINARY_EXIT_MODE".into(), "launcher".into());
                for name in ["PGOKF_ORDINARY_EXIT_MARKER", "PGOKF_ORDINARY_EXIT_CODE"] {
                    spec.env.insert(
                        name.into(),
                        std::env::var_os(name).expect("borrowed test control present"),
                    );
                }
                spec.timeout = Duration::from_secs(2);
                let status = spec.run().unwrap().status;
                std::process::exit(status.code().unwrap());
            }
            _ => {}
        }

        for code in [0, 17] {
            let directory = tempfile::tempdir().unwrap();
            let marker = directory.path().join("descendant.pid");
            let mut spec = CommandSpec::new(std::env::current_exe().unwrap());
            spec.args = [
                "--exact",
                "process::tests::ordinary_exit_terminates_nested_owned_process_groups",
                "--nocapture",
            ]
            .map(OsString::from)
            .to_vec();
            spec.env
                .insert("PGOKF_ORDINARY_EXIT_MODE".into(), "launcher".into());
            spec.env.insert(
                "PGOKF_ORDINARY_EXIT_MARKER".into(),
                marker.as_os_str().into(),
            );
            spec.env
                .insert("PGOKF_ORDINARY_EXIT_CODE".into(), code.to_string().into());
            spec.timeout = Duration::from_secs(2);

            let result = spec.run();
            let descendant = wait_for_pid_marker(&marker);
            let removed = wait_until_gone(descendant);
            if !removed {
                let _ = kill(descendant, Signal::SIGKILL);
            }
            assert!(
                removed,
                "nested owned process group survived ordinary exit {code}: {result:?}"
            );
            let output = result.unwrap();
            assert_eq!(output.status.code(), Some(code));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandboxed_nested_supervision_terminates_owned_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("descendant.pid");
        let mut spec = CommandSpec::new("/usr/bin/sandbox-exec");
        spec.args = [
            "-p".into(),
            "(version 1)(allow default)".into(),
            std::env::current_exe().unwrap().into_os_string(),
            "--exact".into(),
            "process::tests::ordinary_exit_terminates_nested_owned_process_groups".into(),
            "--nocapture".into(),
        ]
        .into();
        spec.env.insert(
            "PGOKF_ORDINARY_EXIT_MODE".into(),
            "borrowed-launcher".into(),
        );
        spec.env.insert(
            "PGOKF_ORDINARY_EXIT_MARKER".into(),
            marker.as_os_str().into(),
        );
        spec.env
            .insert("PGOKF_ORDINARY_EXIT_CODE".into(), "17".into());
        spec.timeout = Duration::from_secs(5);
        spec.delegate_nested_supervision = true;

        let result = spec.run();
        let descendant = wait_for_pid_marker(&marker);
        let removed = wait_until_gone(descendant);
        if !removed {
            let _ = kill(descendant, Signal::SIGKILL);
        }
        assert!(removed, "sandboxed nested descendant survived: {result:?}");
        assert_eq!(result.unwrap().status.code(), Some(17));
    }

    #[test]
    fn argument_bytes_and_stdin_are_not_shell_code() {
        let mut spec = CommandSpec::new("/bin/cat");
        spec.stdin = b"literal $(exit 9); & quotes\n".to_vec();
        assert_eq!(spec.checked().unwrap().stdout, spec.stdin);
        let mut spec = CommandSpec::new("/usr/bin/printf");
        spec.args = ["%s", "$(exit 9); spaces ' "].map(OsString::from).to_vec();
        assert_eq!(spec.checked().unwrap().stdout, b"$(exit 9); spaces ' ");
    }

    #[test]
    fn stderr_and_exit_status_are_preserved() {
        let spec = shell("printf err >&2; exit 7");
        let output = spec.run().unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stderr, b"err");
        assert!(spec.checked().unwrap_err().contains("err"));
    }

    #[test]
    fn timeout_and_both_output_caps_refuse() {
        let directory = tempfile::tempdir().unwrap();
        let stdout = directory.path().join("stdout.partial");
        let stderr = directory.path().join("stderr.partial");
        let mut spec = shell("printf partial-out; printf partial-err >&2; sleep 30");
        spec.timeout = Duration::from_secs(5);
        spec.durable_stdout = Some(stdout.clone());
        spec.durable_stderr = Some(stderr.clone());
        let start = Instant::now();
        assert!(spec.run().unwrap_err().contains("timeout"));
        assert!(start.elapsed() < Duration::from_secs(8));
        assert_eq!(fs::read(stdout).unwrap(), b"partial-out");
        assert_eq!(fs::read(stderr).unwrap(), b"partial-err");
        for source in [
            "while :; do printf 1234567890; done",
            "while :; do printf 1234567890 >&2; done",
        ] {
            let mut spec = shell(source);
            spec.output_cap = 20;
            assert!(spec.run().unwrap_err().contains("output cap"));
        }
    }

    #[test]
    fn inherited_pipes_cannot_extend_deadline() {
        let start = Instant::now();
        let result = shell("sleep 30 & printf done").checked();
        assert!(result.is_ok(), "{result:?}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    fn assert_nested_group_cleanup(directory: &std::path::Path) {
        let cleanup_ready = directory.join("cleanup.pid");
        let mut cleanup = CommandSpec::new(std::env::current_exe().unwrap());
        cleanup.args = [
            "--exact",
            "process::tests::delayed_detached_writer_is_captured_or_times_out",
            "--nocapture",
        ]
        .map(OsString::from)
        .to_vec();
        cleanup
            .env
            .insert("PGOKF_DELAYED_WRITER_MODE".into(), "cleanup-parent".into());
        cleanup.env.insert(
            "PGOKF_DELAYED_WRITER_READY".into(),
            cleanup_ready.as_os_str().into(),
        );
        cleanup.timeout = Duration::from_secs(1);
        let error = cleanup.run().unwrap_err();
        assert!(error.contains("timeout exceeded"), "{error}");
        let nested = Pid::from_raw(
            fs::read_to_string(&cleanup_ready)
                .unwrap()
                .trim()
                .parse()
                .unwrap(),
        );
        let mut removed = false;
        for _ in 0..100 {
            if kill(nested, None) == Err(nix::errno::Errno::ESRCH) {
                removed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if !removed {
            let _ = kill(nested, Signal::SIGKILL);
        }
        assert!(removed, "nested owned process group survived outer timeout");
    }

    #[test]
    fn delayed_detached_writer_is_captured_or_times_out() {
        let mode = std::env::var("PGOKF_DELAYED_WRITER_MODE").ok();
        if mode.as_deref() == Some("grandchild") {
            fs::write(
                std::env::var_os("PGOKF_DELAYED_WRITER_READY").unwrap(),
                b"ready",
            )
            .unwrap();
            std::thread::sleep(Duration::from_millis(150));
            print!("PGOKF_DELAYED_DESCENDANT_OUTPUT");
            return;
        }
        if matches!(mode.as_deref(), Some("parent" | "cleanup-parent")) {
            let cleanup = mode.as_deref() == Some("cleanup-parent");
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "process::tests::delayed_detached_writer_is_captured_or_times_out",
                    "--nocapture",
                ])
                .env(
                    "PGOKF_DELAYED_WRITER_MODE",
                    if cleanup {
                        "cleanup-grandchild"
                    } else {
                        "grandchild"
                    },
                )
                .env(
                    "PGOKF_DELAYED_WRITER_READY",
                    std::env::var_os("PGOKF_DELAYED_WRITER_READY").unwrap(),
                )
                .process_group(0);
            let mut detached = child.spawn().unwrap();
            assert!(detached.try_wait().unwrap().is_none());
            let ready = PathBuf::from(std::env::var_os("PGOKF_DELAYED_WRITER_READY").unwrap());
            if cleanup {
                write_pid_marker(&ready, detached.id());
                std::thread::sleep(Duration::from_secs(30));
                return;
            }
            for _ in 0..100 {
                if ready.exists() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            panic!("detached writer did not start");
        }
        if mode.as_deref() == Some("cleanup-grandchild") {
            std::thread::sleep(Duration::from_secs(30));
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let mut spec = CommandSpec::new(std::env::current_exe().unwrap());
        spec.args = [
            "--exact",
            "process::tests::delayed_detached_writer_is_captured_or_times_out",
            "--nocapture",
        ]
        .map(OsString::from)
        .to_vec();
        spec.env
            .insert("PGOKF_DELAYED_WRITER_MODE".into(), "parent".into());
        spec.env.insert(
            "PGOKF_DELAYED_WRITER_READY".into(),
            ready.as_os_str().into(),
        );
        spec.timeout = Duration::from_secs(1);
        match spec.run() {
            Ok(output) => assert!(
                output
                    .stdout
                    .windows(b"PGOKF_DELAYED_DESCENDANT_OUTPUT".len())
                    .any(|window| window == b"PGOKF_DELAYED_DESCENDANT_OUTPUT"),
                "successful command omitted delayed descendant output"
            ),
            Err(error) => assert!(error.contains("draining"), "{error}"),
        }

        assert_nested_group_cleanup(directory.path());
    }

    #[test]
    fn private_verification_environment_is_not_inherited_by_subprocesses() {
        if std::env::var_os("PGOKF_ENV_SCRUB_CHILD").is_some() {
            let output = CommandSpec::new("/usr/bin/env").checked().unwrap();
            let environment = String::from_utf8(output.stdout).unwrap();
            for name in PRIVATE_VERIFICATION_ENV {
                assert!(!environment.contains(&format!("{name}=")), "{name}");
            }
            let mut explicit = CommandSpec::new("/usr/bin/env");
            explicit
                .env
                .insert("PGOKF_VERIFICATION_NONCE".into(), "explicit".into());
            let environment = String::from_utf8(explicit.checked().unwrap().stdout).unwrap();
            assert!(environment.contains("PGOKF_VERIFICATION_NONCE=explicit"));
            #[cfg(target_os = "macos")]
            {
                let mut broker = CommandSpec::new("/usr/bin/env");
                broker.env.insert(
                    "PGOKF_LAUNCHD_BROKER".into(),
                    "/forged/broker/authority".into(),
                );
                let environment = String::from_utf8(broker.checked().unwrap().stdout).unwrap();
                assert!(!environment.contains("PGOKF_LAUNCHD_BROKER="));
            }
            return;
        }
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "process::tests::private_verification_environment_is_not_inherited_by_subprocesses",
                "--nocapture",
            ])
            .env("PGOKF_ENV_SCRUB_CHILD", "1");
        for name in PRIVATE_VERIFICATION_ENV {
            child.env(name, "inherited-forgery-material");
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
