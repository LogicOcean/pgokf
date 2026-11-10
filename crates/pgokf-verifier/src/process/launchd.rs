//! macOS launchd-coalition process containment.
use super::CommandSpec;
use kqueue::{EventData, EventFilter, FilterFlag, Proc, Watcher};
use nix::{
    sys::{signal::Signal, stat::Mode},
    unistd::{Uid, mkfifo},
};
use pgokf_process_handle::{CoalitionHandle, ProcessHandle, SignalResult};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::{DirBuilder, File, OpenOptions},
    io::{Read, Seek, Write},
    os::unix::{
        fs::{DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _},
        process::ExitStatusExt as _,
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

const COALITION_ID_ENV: &str = "PGOKF_LAUNCHD_COALITION_ID";
const BROKER_ENV: &str = "PGOKF_LAUNCHD_BROKER";

/// Machinery-controlled stdin chunk for contained command/service input.
///
/// Contained stdin is an OS-fixed 8 KiB FIFO (measured 8,192 bytes on this
/// host; there is no macOS knob to enlarge it), while the pre-containment
/// `std::process` pipes hold 65,536 bytes. The nonblocking writer advances
/// on partial progress, so the chunk is the explicit pre-containment
/// capacity, never a platform default, and no behavior depends on the
/// platform stream being smaller.
pub(crate) const STDIN_CHUNK: usize = 64 * 1024;

const LAUNCHD_CONTROL_ENV: [&str; 11] = [
    "PGOKF_LAUNCHD_READY",
    "PGOKF_LAUNCHD_RELEASE",
    "PGOKF_LAUNCHD_STDIN",
    "PGOKF_LAUNCHD_STDOUT",
    "PGOKF_LAUNCHD_STDERR",
    "PGOKF_LAUNCHD_TARGET_PID",
    "PGOKF_LAUNCHD_STATUS",
    "PGOKF_LAUNCHD_CLEANUP",
    "PGOKF_LAUNCHD_COALITION",
    "PGOKF_LAUNCHD_BROKER_PID",
    "PGOKF_LAUNCHD_TARGET_RELEASE",
];

const HELPER_SCRIPT: &str = r#"
ready=$PGOKF_LAUNCHD_READY
release=$PGOKF_LAUNCHD_RELEASE
input=$PGOKF_LAUNCHD_STDIN
stdout=$PGOKF_LAUNCHD_STDOUT
stderr=$PGOKF_LAUNCHD_STDERR
target_pid=$PGOKF_LAUNCHD_TARGET_PID
status=$PGOKF_LAUNCHD_STATUS
cleanup=$PGOKF_LAUNCHD_CLEANUP
coalition=$PGOKF_LAUNCHD_COALITION
broker=$PGOKF_LAUNCHD_BROKER
delegate_broker=${PGOKF_LAUNCHD_DELEGATE_BROKER:-0}
broker_pid=$PGOKF_LAUNCHD_BROKER_PID
target_release=$PGOKF_LAUNCHD_TARGET_RELEASE
allowed_domain="gui/$(/usr/bin/id -u)"

broker_loop() {
    broker_ready_tmp="$broker/ready.$$"
    printf '%s\n' ready >"$broker_ready_tmp" || exit 70
    /bin/mv "$broker_ready_tmp" "$broker/ready" || exit 71
    while :; do
        request_name=
        IFS= read -r request_name <"$broker/notify" || continue
        [ "$request_name" = stop ] && break
        case "$request_name" in
            *[!A-Fa-f0-9-]*|'') continue ;;
        esac
        request="$broker/requests/$request_name"
        [ -d "$request" ] || continue
        [ -e "$request/ready" ] || continue
        [ ! -e "$request/done" ] || continue
        /bin/mkdir "$request/claimed" 2>/dev/null || continue

        action=
        domain=
        label=
        plist=
        IFS= read -r action <"$request/action" || action=
        IFS= read -r domain <"$request/domain" || domain=
        IFS= read -r label <"$request/label" || label=
        IFS= read -r plist <"$request/plist" || plist=
        error_tmp="$request/error.$$"
        : >"$error_tmp" || continue
        result=0

        case "$domain" in
            "$allowed_domain") ;;
            *) printf '%s\n' 'broker rejected launchd domain' >"$error_tmp"; result=64 ;;
        esac
        case "$label" in
            com.nous.pgokf.verifier.*) ;;
            *) printf '%s\n' 'broker rejected launchd label' >"$error_tmp"; result=65 ;;
        esac
        case "$label" in
            *[!A-Za-z0-9._-]*) printf '%s\n' 'broker rejected unsafe launchd label' >"$error_tmp"; result=65 ;;
        esac

        if [ "$result" -eq 0 ]; then
            case "$action" in
                bootstrap)
                    if [ ! -f "$plist" ]; then
                        printf '%s\n' 'broker rejected missing launchd plist' >"$error_tmp"
                        result=66
                    else
                        plist_label=$(/usr/bin/plutil -extract Label raw -- "$plist" 2>"$error_tmp")
                        result=$?
                        if [ "$result" -eq 0 ] && [ "$plist_label" != "$label" ]; then
                            printf '%s\n' 'broker rejected mismatched launchd plist label' >"$error_tmp"
                            result=66
                        fi
                        if [ "$result" -eq 0 ]; then
                            /bin/launchctl bootstrap "$domain" "$plist" >"$error_tmp" 2>&1
                            result=$?
                            if [ "$result" -eq 0 ]; then
                                record_tmp="$broker/owned/$label.$$"
                                if printf '%s\n' "$domain" >"$record_tmp" && /bin/mv "$record_tmp" "$broker/owned/$label"; then
                                    :
                                else
                                    /bin/launchctl bootout "$domain/$label" >/dev/null 2>&1
                                    /bin/rm -f "$record_tmp"
                                    printf '%s\n' 'broker could not record bootstrapped job' >"$error_tmp"
                                    result=67
                                fi
                            fi
                        fi
                    fi
                    ;;
                bootout)
                    /bin/launchctl bootout "$domain/$label" >"$error_tmp" 2>&1
                    result=$?
                    if [ "$result" -eq 0 ]; then
                        /bin/rm -f "$broker/owned/$label"
                    fi
                    ;;
                *)
                    printf '%s\n' 'broker rejected launchd action' >"$error_tmp"
                    result=68
                    ;;
            esac
        fi

        status_tmp="$request/status.$$"
        done_tmp="$request/done.$$"
        printf '%s\n' "$result" >"$status_tmp" || continue
        /bin/mv "$status_tmp" "$request/status" || continue
        /bin/mv "$error_tmp" "$request/error" || continue
        printf '%s\n' done >"$done_tmp" || continue
        /bin/mv "$done_tmp" "$request/done" || continue
    done

    cleanup_result=0
    for record in "$broker"/owned/*; do
        [ -f "$record" ] || continue
        label=${record##*/}
        domain=
        IFS= read -r domain <"$record" || domain=
        case "$domain" in
            "$allowed_domain")
                if /bin/launchctl bootout "$domain/$label" >/dev/null 2>&1; then
                    /bin/rm -f "$record"
                else
                    cleanup_result=75
                fi
                ;;
            *) cleanup_result=75 ;;
        esac
    done
    return "$cleanup_result"
}

broker_process=
stop_broker() {
    if [ -n "$broker_process" ]; then
        printf '%s\n' stop >"$broker/notify"
        wait "$broker_process"
        broker_status=$?
        broker_process=
        return "$broker_status"
    fi
}
trap 'stop_broker' EXIT
trap 'stop_broker; exit 127' HUP INT TERM

broker_loop &
broker_process=$!
broker_pid_tmp="$broker_pid.$$"
printf '%s\n' "$broker_process" >"$broker_pid_tmp" || exit 72
/bin/mv "$broker_pid_tmp" "$broker_pid" || exit 73
while [ ! -e "$broker/ready" ]; do :; done

exec 3>"$stdout"
exec 4>"$stderr"
ready_tmp="$ready.$$"
printf '%s\n' "$$" >"$ready_tmp" || exit 120
/bin/mv "$ready_tmp" "$ready" || exit 121
while [ ! -e "$release" ]; do :; done
IFS= read -r PGOKF_LAUNCHD_COALITION_ID < "$coalition" || exit 126
export PGOKF_LAUNCHD_COALITION_ID
unset PGOKF_LAUNCHD_READY PGOKF_LAUNCHD_RELEASE PGOKF_LAUNCHD_STDIN PGOKF_LAUNCHD_STDOUT PGOKF_LAUNCHD_STDERR PGOKF_LAUNCHD_TARGET_PID PGOKF_LAUNCHD_STATUS PGOKF_LAUNCHD_CLEANUP PGOKF_LAUNCHD_COALITION PGOKF_LAUNCHD_BROKER_PID PGOKF_LAUNCHD_TARGET_RELEASE PGOKF_LAUNCHD_DELEGATE_BROKER
[ "$delegate_broker" = 1 ] || unset PGOKF_LAUNCHD_BROKER
remove_count=$1
shift
while [ "$remove_count" -gt 0 ]; do
    unset "$1"
    shift
    remove_count=$((remove_count - 1))
done
/bin/sh -c 'gate=$1; shift; while [ ! -e "$gate" ]; do :; done; exec "$@"' pgokf-launchd-target-gate "$target_release" "$@" <"$input" >&3 2>&4 &
target=$!
target_tmp="$target_pid.$$"
printf '%s\n' "$target" >"$target_tmp" || exit 122
/bin/mv "$target_tmp" "$target_pid" || exit 123
wait "$target"
code=$?
stop_broker || exit 74
status_tmp="$status.$$"
printf '%s\n' "$code" >"$status_tmp" || exit 124
/bin/mv "$status_tmp" "$status" || exit 125
exec 3>&-
exec 4>&-
while [ ! -e "$cleanup" ]; do :; done
exit "$code"
"#;

#[derive(Clone)]
struct BrokerClient {
    directory: PathBuf,
}

impl BrokerClient {
    fn from_environment() -> Result<Option<Self>, String> {
        let Some(directory) = std::env::var_os(BROKER_ENV) else {
            return Ok(None);
        };
        let client = Self {
            directory: PathBuf::from(directory),
        };
        client.authenticate()?;
        Ok(Some(client))
    }

    fn authenticate(&self) -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(&self.directory)
            .map_err(|error| format!("launchd broker metadata failed: {error}"))?;
        if !metadata.file_type().is_dir()
            || metadata.uid() != Uid::current().as_raw()
            || metadata.mode() & 0o777 != 0o700
        {
            return Err("inherited launchd broker directory is not private and user-owned".into());
        }
        let requests = std::fs::symlink_metadata(self.directory.join("requests"))
            .map_err(|error| format!("launchd broker requests metadata failed: {error}"))?;
        if !requests.file_type().is_dir()
            || requests.uid() != Uid::current().as_raw()
            || requests.mode() & 0o777 != 0o700
        {
            return Err("inherited launchd broker request directory is not private".into());
        }
        let notify = std::fs::symlink_metadata(self.directory.join("notify"))
            .map_err(|error| format!("launchd broker notification metadata failed: {error}"))?;
        if !notify.file_type().is_fifo()
            || notify.uid() != Uid::current().as_raw()
            || notify.mode() & 0o777 != 0o600
        {
            return Err("inherited launchd broker notification FIFO is not private".into());
        }
        if !self.directory.join("ready").is_file() {
            return Err("inherited launchd broker is not ready".into());
        }

        let authenticated = read_coalition_identity(&self.directory.join("coalition"))?;
        let current = CoalitionHandle::capture(std::process::id())?
            .ok_or("current process vanished during launchd broker authentication")?;
        if authenticated != (current.id().resource_id(), current.id().jetsam_id()) {
            return Err("inherited launchd broker belongs to a different coalition".into());
        }
        let broker_pid = read_pid_marker(&self.directory.join("broker.pid"))?;
        let broker = ProcessHandle::capture(broker_pid)?
            .ok_or("inherited launchd broker process is no longer present")?;
        let broker_coalition = CoalitionHandle::capture(broker.pid())?
            .ok_or("inherited launchd broker vanished during coalition authentication")?;
        if broker_coalition.id() != current.id() {
            return Err(
                "inherited launchd broker process escaped its authenticated coalition".into(),
            );
        }
        Ok(())
    }

    fn bootstrap(
        &self,
        domain: &str,
        label: &str,
        plist: &Path,
        deadline: Instant,
    ) -> Result<(), String> {
        self.request("bootstrap", domain, label, Some(plist), deadline)
    }

    fn bootout(&self, domain: &str, label: &str, deadline: Instant) -> Result<(), String> {
        self.request("bootout", domain, label, None, deadline)
    }

    fn request(
        &self,
        action: &str,
        domain: &str,
        label: &str,
        plist: Option<&Path>,
        deadline: Instant,
    ) -> Result<(), String> {
        self.authenticate()?;
        for value in [action, domain, label] {
            if value.contains(['\n', '\r']) {
                return Err("launchd broker request contains a line break".into());
            }
        }
        let plist = plist.map_or_else(OsString::new, |path| path.as_os_str().to_owned());
        let plist_text = plist
            .to_str()
            .ok_or("launchd broker requires a UTF-8 plist path")?;
        if plist_text.contains(['\n', '\r']) {
            return Err("launchd broker plist path contains a line break".into());
        }

        let request_id = uuid::Uuid::new_v4().to_string();
        let request = self.directory.join("requests").join(&request_id);
        DirBuilder::new()
            .mode(0o700)
            .create(&request)
            .map_err(|error| format!("launchd broker request creation failed: {error}"))?;
        let result = (|| {
            write_request_field(&request.join("action"), action.as_bytes())?;
            write_request_field(&request.join("domain"), domain.as_bytes())?;
            write_request_field(&request.join("label"), label.as_bytes())?;
            write_request_field(&request.join("plist"), plist_text.as_bytes())?;
            publish_marker(&request.join("ready"))?;
            notify_broker(&self.directory.join("notify"), &request_id, deadline)?;

            while !request.join("done").is_file() {
                if Instant::now() >= deadline {
                    return Err("timeout exceeded waiting for launchd broker reply".into());
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            let status = std::fs::read_to_string(request.join("status"))
                .map_err(|error| format!("launchd broker status read failed: {error}"))?
                .trim()
                .parse::<i32>()
                .map_err(|error| format!("launchd broker status is malformed: {error}"))?;
            if status == 0 {
                Ok(())
            } else {
                let error = std::fs::read_to_string(request.join("error"))
                    .unwrap_or_else(|read_error| format!("broker error unavailable: {read_error}"));
                Err(format!(
                    "launchd broker {action} failed ({status}): {error}"
                ))
            }
        })();
        let _ = std::fs::remove_dir_all(&request);
        result
    }
}

enum BootstrapMethod {
    Direct,
    Broker(BrokerClient),
}

impl BootstrapMethod {
    fn bootout(&self, domain: &str, label: &str, deadline: Instant) -> Result<(), String> {
        match self {
            Self::Direct => direct_bootout(domain, label, deadline),
            Self::Broker(broker) => broker.bootout(domain, label, deadline),
        }
    }
}

pub(crate) struct LaunchdChild {
    target_pid: u32,
    excluded: Vec<ProcessHandle>,
    coalition: CoalitionHandle,
    stdin: Option<File>,
    stdin_bytes: Vec<u8>,
    stdin_offset: usize,
    stdout: Option<File>,
    stderr: Option<File>,
    status: Option<ExitStatus>,
    exit_watcher: Watcher,
    directory: tempfile::TempDir,
    label: String,
    domain: String,
    bootstrap: Option<BootstrapMethod>,
    cleaned: bool,
}

impl LaunchdChild {
    // Keep the ordered bootstrap lifecycle linear so each failure retains its cleanup guard.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn spawn(spec: &CommandSpec, deadline: Instant) -> Result<Self, String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let ready = directory.path().join("helper.pid");
        let release = directory.path().join("release");
        let input = directory.path().join("stdin");
        let stdout_path = directory.path().join("stdout.fifo");
        let stderr_path = directory.path().join("stderr.fifo");
        let target_pid_path = directory.path().join("target.pid");
        let status_path = directory.path().join("status");
        let cleanup = directory.path().join("cleanup");
        let coalition_path = directory.path().join("coalition");
        let broker_directory = directory.path().join("broker");
        let broker_pid_path = broker_directory.join("broker.pid");
        let target_release = directory.path().join("target.release");
        let plist = directory.path().join("job.plist");

        create_broker_directory(&broker_directory)
            .map_err(|error| format!("launchd broker setup failed: {error}"))?;
        mkfifo(&input, Mode::S_IRUSR | Mode::S_IWUSR)
            .map_err(|error| format!("launchd stdin FIFO creation failed: {error}"))?;
        mkfifo(&stdout_path, Mode::S_IRUSR | Mode::S_IWUSR)
            .map_err(|error| format!("launchd stdout FIFO creation failed: {error}"))?;
        mkfifo(&stderr_path, Mode::S_IRUSR | Mode::S_IWUSR)
            .map_err(|error| format!("launchd stderr FIFO creation failed: {error}"))?;
        let stdout = open_fifo_reader(&stdout_path)
            .map_err(|error| format!("launchd stdout FIFO open failed: {error}"))?;
        let stderr = open_fifo_reader(&stderr_path)
            .map_err(|error| format!("launchd stderr FIFO open failed: {error}"))?;

        let label = format!("com.nous.pgokf.verifier.{}", uuid::Uuid::new_v4());
        let domain = format!("gui/{}", Uid::current().as_raw());
        let controls = [
            &ready,
            &release,
            &input,
            &stdout_path,
            &stderr_path,
            &target_pid_path,
            &status_path,
            &cleanup,
            &coalition_path,
            &broker_pid_path,
            &target_release,
        ];
        write_plist(&plist, &label, &broker_directory, spec, controls)
            .map_err(|error| format!("launchd property-list creation failed: {error}"))?;
        let mut bootstrap = Some(
            bootstrap_job(&domain, &label, &plist, deadline)
                .map_err(|error| format!("launchd job bootstrap failed: {error}"))?,
        );

        let result = (|| {
            let helper_pid = wait_pid_marker(&ready, deadline).map_err(|error| {
                format!(
                    "launchd helper readiness failed: {error}; launcher stdout: {}; launcher stderr: {}",
                    std::fs::read_to_string(directory.path().join("launchd.stdout"))
                        .unwrap_or_default(),
                    std::fs::read_to_string(directory.path().join("launchd.stderr"))
                        .unwrap_or_default()
                )
            })?;
            let helper = ProcessHandle::capture(helper_pid)?
                .ok_or_else(|| "launchd helper vanished before identity capture".to_owned())?;
            let coalition = CoalitionHandle::capture(helper_pid)?
                .ok_or_else(|| "launchd helper vanished before coalition capture".to_owned())?;
            let current = CoalitionHandle::capture(std::process::id())?.ok_or_else(|| {
                "verifier process vanished during coalition validation".to_owned()
            })?;
            if coalition.id() == current.id() {
                return Err("launchd did not isolate the command in a unique coalition".into());
            }

            let broker_pid = wait_pid_marker(&broker_pid_path, deadline)
                .map_err(|error| format!("launchd broker readiness failed: {error}"))?;
            let broker = ProcessHandle::capture(broker_pid)?
                .ok_or_else(|| "launchd broker vanished before identity capture".to_owned())?;
            let broker_coalition = CoalitionHandle::capture(broker_pid)?
                .ok_or_else(|| "launchd broker vanished before coalition capture".to_owned())?;
            if broker_coalition.id() != coalition.id() {
                return Err("launchd broker escaped the helper coalition".into());
            }

            write_coalition_identity(&coalition_path, coalition)
                .map_err(|error| format!("launchd helper coalition publication failed: {error}"))?;
            write_coalition_identity(&broker_directory.join("coalition"), coalition)
                .map_err(|error| format!("launchd broker coalition publication failed: {error}"))?;
            create_release(&release)
                .map_err(|error| format!("launchd helper release failed: {error}"))?;
            let stdin = open_fifo_writer(&input, deadline)
                .map_err(|error| format!("launchd stdin FIFO open failed: {error}"))?;
            let target_pid = wait_pid_marker(&target_pid_path, deadline)
                .map_err(|error| format!("launchd target readiness failed: {error}"))?;
            let exit_watcher = watch_target_exit(target_pid, deadline)
                .map_err(|error| format!("launchd target exit watcher failed: {error}"))?;
            if let Some(target_coalition) = CoalitionHandle::capture(target_pid)?
                && target_coalition.id() != coalition.id()
            {
                return Err("target escaped its launchd coalition before supervision".into());
            }
            create_release(&target_release)
                .map_err(|error| format!("launchd target release failed: {error}"))?;

            Ok(Self {
                target_pid,
                excluded: vec![helper, broker],
                coalition,
                stdin: Some(stdin),
                stdin_bytes: spec.stdin.clone(),
                stdin_offset: 0,
                stdout: Some(stdout),
                stderr: Some(stderr),
                status: None,
                exit_watcher,
                directory,
                label: label.clone(),
                domain: domain.clone(),
                bootstrap: bootstrap.take(),
                cleaned: false,
            })
        })();
        if result.is_err()
            && let Some(bootstrap) = bootstrap.as_ref()
        {
            let _ = bootstrap.bootout(&domain, &label, Instant::now() + Duration::from_secs(2));
        }
        result
    }

    pub(crate) fn id(&self) -> u32 {
        self.target_pid
    }

    pub(crate) fn take_stdout(&mut self) -> Result<File, String> {
        self.stdout
            .take()
            .ok_or_else(|| "stdout unavailable".into())
    }

    pub(crate) fn take_stderr(&mut self) -> Result<File, String> {
        self.stderr
            .take()
            .ok_or_else(|| "stderr unavailable".into())
    }

    pub(crate) fn take_stdin(&mut self) -> Option<File> {
        self.stdin_bytes.clear();
        self.stdin_offset = 0;
        self.stdin.take()
    }

    fn flush_stdin(&mut self) -> Result<(), String> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Ok(());
        };
        if self.stdin_offset == self.stdin_bytes.len() {
            self.stdin = None;
            return Ok(());
        }
        let end = (self.stdin_offset + STDIN_CHUNK).min(self.stdin_bytes.len());
        match stdin.write(&self.stdin_bytes[self.stdin_offset..end]) {
            Ok(0) => Err("launchd stdin made no write progress".into()),
            Ok(count) => {
                self.stdin_offset += count;
                if self.stdin_offset == self.stdin_bytes.len() {
                    self.stdin = None;
                }
                Ok(())
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                self.stdin = None;
                Ok(())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    fn poll_exit_watcher(&mut self) -> Result<bool, String> {
        let Some(event) = self.exit_watcher.poll(None) else {
            return Ok(false);
        };
        match event.data {
            EventData::Proc(Proc::Exit(raw)) => {
                let raw = i32::try_from(raw)
                    .map_err(|_| "launchd target exit status exceeded i32".to_owned())?;
                self.status = Some(ExitStatus::from_raw(raw));
                self.stdin = None;
                Ok(true)
            }
            EventData::Error(error) => Err(format!("launchd target watcher failed: {error}")),
            other => Err(format!(
                "launchd target watcher returned unexpected event: {other:?}"
            )),
        }
    }

    pub(crate) fn observe_exit(&mut self) -> Result<bool, String> {
        if self.status.is_some() {
            return Ok(true);
        }
        self.flush_stdin()?;
        if self.poll_exit_watcher()? {
            return Ok(true);
        }

        let status = self.directory.path().join("status");
        let bytes = match std::fs::read(&status) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.to_string()),
        };
        let code = std::str::from_utf8(&bytes)
            .map_err(|error| error.to_string())?
            .trim()
            .parse::<i32>()
            .map_err(|error| error.to_string())?;
        if code >= 128 {
            // The helper's POSIX shell status cannot distinguish signal
            // termination from a normal exit in the 128..=255 range. The
            // mandatory NOTE_EXITSTATUS watcher supplies the exact raw status;
            // leave the shell marker pending until that event is observable.
            return Ok(false);
        }
        self.status = Some(ExitStatus::from_raw(code << 8));
        self.stdin = None;
        Ok(true)
    }

    pub(crate) fn observed_status(&self) -> Option<ExitStatus> {
        self.status
    }

    fn signal_owned_members(&self, signal: Signal) -> Result<(), String> {
        for member in self.coalition.members()? {
            if !self.excluded.contains(&member) {
                match member.signal(signal as i32)? {
                    SignalResult::Delivered | SignalResult::Gone => {}
                }
            }
        }
        Ok(())
    }

    pub(crate) fn signal_group(&mut self, signal: Signal) -> Result<(), String> {
        match signal {
            Signal::SIGTERM if self.status.is_some() => Ok(()),
            Signal::SIGTERM | Signal::SIGKILL => self.signal_owned_members(signal),
            other => Err(format!("unsupported launchd containment signal {other:?}")),
        }
    }

    pub(crate) fn wait(&mut self, deadline: Instant) -> Result<ExitStatus, String> {
        while !self.observe_exit()? {
            if Instant::now() >= deadline {
                return Err("timeout exceeded while waiting for launchd target".into());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        self.status
            .ok_or_else(|| "missing launchd target status".into())
    }

    pub(crate) fn terminate_and_ensure_released(&mut self) -> Result<(), String> {
        if self.cleaned {
            return Ok(());
        }
        self.stdin = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut empty_since = None;
        loop {
            let members = self.coalition.members()?;
            let others = members
                .iter()
                .filter(|member| !self.excluded.contains(member))
                .copied()
                .collect::<Vec<_>>();
            let remaining_pids = others.iter().map(|member| member.pid()).collect::<Vec<_>>();
            if others.is_empty() {
                let since = *empty_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_millis(250) {
                    break;
                }
            } else {
                empty_since = None;
                for member in others {
                    match member.signal(Signal::SIGKILL as i32)? {
                        SignalResult::Delivered | SignalResult::Gone => {}
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "launchd coalition cleanup timed out for target {}; remaining process identities: {remaining_pids:?}",
                    self.target_pid
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        cleanup_broker_owned_jobs(
            &self.directory.path().join("broker").join("owned"),
            &self.domain,
            Instant::now() + Duration::from_secs(2),
        )?;

        create_release(&self.directory.path().join("cleanup"))?;
        let helper_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let members = self.coalition.members()?;
            let controls_present = self
                .excluded
                .iter()
                .any(|control| members.contains(control));
            if !controls_present {
                break;
            }
            if Instant::now() >= helper_deadline {
                return Err(
                    "launchd helper or broker did not exit after cleanup acknowledgement".into(),
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(bootstrap) = self.bootstrap.take() {
            bootstrap.bootout(
                &self.domain,
                &self.label,
                Instant::now() + Duration::from_secs(2),
            )?;
        }
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for LaunchdChild {
    fn drop(&mut self) {
        self.stdin = None;
        if !self.cleaned {
            let _ = self.signal_owned_members(Signal::SIGKILL);
            let _ = cleanup_broker_owned_jobs(
                &self.directory.path().join("broker").join("owned"),
                &self.domain,
                Instant::now() + Duration::from_secs(2),
            );
            let _ = create_release(&self.directory.path().join("cleanup"));
        }
        if let Some(bootstrap) = self.bootstrap.take() {
            let _ = bootstrap.bootout(
                &self.domain,
                &self.label,
                Instant::now() + Duration::from_secs(2),
            );
        }
    }
}

fn create_broker_directory(path: &Path) -> Result<(), String> {
    DirBuilder::new()
        .mode(0o700)
        .create(path)
        .and_then(|()| DirBuilder::new().mode(0o700).create(path.join("requests")))
        .and_then(|()| DirBuilder::new().mode(0o700).create(path.join("owned")))
        .map_err(|error| format!("launchd broker directory creation failed: {error}"))?;
    mkfifo(&path.join("notify"), Mode::S_IRUSR | Mode::S_IWUSR)
        .map_err(|error| format!("launchd broker notification FIFO creation failed: {error}"))
}

fn open_fifo_reader(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| error.to_string())
}

fn open_fifo_writer(path: &Path, deadline: Instant) -> Result<File, String> {
    loop {
        match OpenOptions::new()
            .write(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => return Ok(file),
            Err(error) if error.raw_os_error() == Some(nix::libc::ENXIO) => {}
            Err(error) => return Err(error.to_string()),
        }
        if Instant::now() >= deadline {
            return Err(format!("timeout exceeded opening {}", path.display()));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn notify_broker(path: &Path, request_id: &str, deadline: Instant) -> Result<(), String> {
    let notification = format!("{request_id}\n");
    loop {
        match OpenOptions::new()
            .write(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(path)
        {
            Ok(mut fifo) => match fifo.write_all(notification.as_bytes()) {
                Ok(()) => return Ok(()),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::Interrupted
                            | std::io::ErrorKind::BrokenPipe
                    ) => {}
                Err(error) => return Err(format!("launchd broker notification failed: {error}")),
            },
            Err(error) if error.raw_os_error() == Some(nix::libc::ENXIO) => {}
            Err(error) => return Err(format!("launchd broker notification open failed: {error}")),
        }
        if Instant::now() >= deadline {
            return Err("timeout exceeded notifying launchd broker".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn create_release(path: &Path) -> Result<(), String> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| error.to_string())
}

fn publish_marker(path: &Path) -> Result<(), String> {
    let temporary = path.with_extension(format!("tmp.{}", uuid::Uuid::new_v4()));
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(b"ready\n")?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&temporary, path))
        .map_err(|error| error.to_string())
}

fn write_request_field(path: &Path, value: &[u8]) -> Result<(), String> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .and_then(|mut file| {
            file.write_all(value)?;
            file.write_all(b"\n")?;
            file.sync_all()
        })
        .map_err(|error| error.to_string())
}

fn read_pid_marker(path: &Path) -> Result<u32, String> {
    std::fs::read_to_string(path)
        .map_err(|error| error.to_string())?
        .trim()
        .parse::<u32>()
        .map_err(|error| error.to_string())
}

fn wait_pid_marker(path: &Path, deadline: Instant) -> Result<u32, String> {
    loop {
        match std::fs::read_to_string(path) {
            Ok(value) => {
                return value
                    .trim()
                    .parse::<u32>()
                    .map_err(|error| error.to_string());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        if Instant::now() >= deadline {
            return Err(format!("timeout exceeded waiting for {}", path.display()));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn watch_target_exit(pid: u32, deadline: Instant) -> Result<Watcher, String> {
    let raw_pid = i32::try_from(pid).map_err(|error| error.to_string())?;
    loop {
        let mut watcher = Watcher::new().map_err(|error| error.to_string())?;
        watcher
            .add_pid(
                raw_pid,
                EventFilter::EVFILT_PROC,
                FilterFlag::NOTE_EXIT | FilterFlag::NOTE_EXITSTATUS,
            )
            .map_err(|error| error.to_string())?;
        match watcher.watch() {
            Ok(()) => return Ok(watcher),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(code)
                        if code == nix::errno::Errno::ESRCH as i32
                            || code == nix::errno::Errno::ENOENT as i32
                ) =>
            {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "timeout exceeded binding raw exit observation for launchd target {pid}: {error}"
                    ));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn effective_environment(spec: &CommandSpec) -> BTreeMap<OsString, OsString> {
    let mut environment = std::env::vars_os().collect::<BTreeMap<_, _>>();
    for name in &spec.env_remove {
        environment.remove(name);
    }
    environment.extend(spec.env.clone());
    environment
}

fn helper_arguments(spec: &CommandSpec) -> Vec<OsString> {
    let removals = spec
        .env_remove
        .iter()
        .filter(|name| !spec.env.contains_key(*name))
        .filter(|name| name.as_os_str() != OsStr::new(COALITION_ID_ENV))
        .filter(|name| name.as_os_str() != OsStr::new(BROKER_ENV))
        .cloned()
        .collect::<Vec<_>>();
    let mut arguments = vec![
        OsString::from("-c"),
        OsString::from(HELPER_SCRIPT),
        OsString::from("pgokf-launchd-helper"),
        removals.len().to_string().into(),
    ];
    arguments.extend(removals);
    arguments.push(spec.program.as_os_str().to_owned());
    arguments.extend(spec.args.clone());
    arguments
}

fn write_coalition_identity(path: &Path, coalition: CoalitionHandle) -> Result<(), String> {
    let identity = coalition.id();
    let value = format!("{}:{}\n", identity.resource_id(), identity.jetsam_id());
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .and_then(|mut file| {
            file.write_all(value.as_bytes())?;
            file.sync_all()
        })
        .map_err(|error| error.to_string())
}

fn read_coalition_identity(path: &Path) -> Result<(u64, u64), String> {
    let value = std::fs::read_to_string(path)
        .map_err(|error| format!("launchd coalition identity read failed: {error}"))?;
    let (resource, jetsam) = value
        .trim()
        .split_once(':')
        .ok_or("launchd coalition identity is malformed")?;
    let resource = resource.parse::<u64>().map_err(|error| error.to_string())?;
    let jetsam = jetsam.parse::<u64>().map_err(|error| error.to_string())?;
    Ok((resource, jetsam))
}

fn xml_string(value: &OsStr) -> Result<String, String> {
    let value = value.to_str().ok_or_else(|| {
        "launchd containment requires UTF-8 process arguments and environment".to_owned()
    })?;
    if value
        .chars()
        .any(|character| !matches!(character, '\t' | '\n' | '\r') && character < '\u{20}')
    {
        return Err("launchd property lists cannot encode control characters".into());
    }
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

fn push_string(xml: &mut String, value: &OsStr) -> Result<(), String> {
    xml.push_str("<string>");
    xml.push_str(&xml_string(value)?);
    xml.push_str("</string>");
    Ok(())
}

fn write_plist(
    path: &Path,
    label: &str,
    broker: &Path,
    spec: &CommandSpec,
    controls: [&PathBuf; 11],
) -> Result<(), String> {
    let mut arguments = vec![OsString::from("/bin/sh")];
    arguments.extend(helper_arguments(spec));

    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>",
    );
    xml.push_str("<key>Label</key>");
    push_string(&mut xml, OsStr::new(label))?;
    xml.push_str("<key>ProgramArguments</key><array>");
    for argument in &arguments {
        push_string(&mut xml, argument)?;
    }
    xml.push_str(
        "</array><key>RunAtLoad</key><true/><key>ProcessType</key><string>Interactive</string>",
    );
    xml.push_str("<key>EnvironmentVariables</key><dict>");
    let mut environment = effective_environment(spec);
    environment.insert(OsString::from(BROKER_ENV), broker.as_os_str().to_owned());
    environment.insert(
        OsString::from("PGOKF_LAUNCHD_DELEGATE_BROKER"),
        OsString::from(if spec.delegate_nested_supervision {
            "1"
        } else {
            "0"
        }),
    );
    for (name, control) in LAUNCHD_CONTROL_ENV.into_iter().zip(controls) {
        environment.insert(OsString::from(name), control.as_os_str().to_owned());
    }

    for (name, value) in environment {
        xml.push_str("<key>");
        xml.push_str(&xml_string(&name)?);
        xml.push_str("</key>");
        push_string(&mut xml, &value)?;
    }
    xml.push_str("</dict>");
    if let Some(cwd) = &spec.cwd {
        xml.push_str("<key>WorkingDirectory</key>");
        push_string(&mut xml, cwd.as_os_str())?;
    }
    xml.push_str("<key>StandardOutPath</key>");
    push_string(&mut xml, path.with_file_name("launchd.stdout").as_os_str())?;
    xml.push_str("<key>StandardErrorPath</key>");
    push_string(&mut xml, path.with_file_name("launchd.stderr").as_os_str())?;
    xml.push_str("</dict></plist>\n");
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .and_then(|mut file| {
            file.write_all(xml.as_bytes())?;
            file.sync_all()
        })
        .map_err(|error| error.to_string())
}

fn bounded_launchctl(
    mut command: Command,
    deadline: Instant,
) -> Result<(ExitStatus, String), String> {
    let mut stderr = tempfile::tempfile()
        .map_err(|error| format!("launchctl stderr capture failed: {error}"))?;
    command
        .stdout(Stdio::null())
        .stderr(stderr.try_clone().map_err(|error| error.to_string())?);
    let mut child = command
        .spawn()
        .map_err(|error| format!("launchctl spawn failed: {error}"))?;
    let handle = ProcessHandle::capture(child.id())?;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("launchctl wait failed: {error}"))?
        {
            stderr.rewind().map_err(|error| error.to_string())?;
            let mut diagnostics = String::new();
            stderr
                .read_to_string(&mut diagnostics)
                .map_err(|error| error.to_string())?;
            return Ok((status, diagnostics));
        }
        if Instant::now() >= deadline {
            let handle = handle
                .ok_or("launchctl exceeded its deadline after vanishing before identity capture")?;
            match handle.signal(Signal::SIGKILL as i32)? {
                SignalResult::Delivered | SignalResult::Gone => {}
            }
            child
                .wait()
                .map_err(|error| format!("launchctl reap after timeout failed: {error}"))?;
            return Err("timeout exceeded while waiting for launchctl".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn valid_launchd_label(label: &str) -> bool {
    label.starts_with("com.nous.pgokf.verifier.")
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn cleanup_broker_owned_jobs(
    owned: &Path,
    expected_domain: &str,
    deadline: Instant,
) -> Result<(), String> {
    let mut empty_since = None;
    loop {
        let mut records = Vec::new();
        for entry in std::fs::read_dir(owned)
            .map_err(|error| format!("launchd broker ownership read failed: {error}"))?
        {
            let entry = entry.map_err(|error| error.to_string())?;
            let metadata = entry
                .path()
                .symlink_metadata()
                .map_err(|error| error.to_string())?;
            let label = entry
                .file_name()
                .into_string()
                .map_err(|_| "launchd broker ownership label is not UTF-8".to_owned())?;
            if !metadata.file_type().is_file()
                || metadata.uid() != Uid::current().as_raw()
                || !valid_launchd_label(&label)
            {
                return Err("launchd broker ownership record is not authenticated".into());
            }
            let domain = std::fs::read_to_string(entry.path())
                .map_err(|error| error.to_string())?
                .trim()
                .to_owned();
            if domain != expected_domain {
                return Err("launchd broker ownership domain changed".into());
            }
            records.push((entry.path(), label));
        }
        if records.is_empty() {
            let since = *empty_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(100) {
                return Ok(());
            }
        } else {
            empty_since = None;
            for (record, label) in records {
                direct_bootout(expected_domain, &label, deadline)?;
                match std::fs::remove_file(record) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
        if Instant::now() >= deadline {
            return Err("timeout exceeded cleaning launchd broker-owned jobs".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn bootstrap_job(
    domain: &str,
    label: &str,
    plist: &Path,
    deadline: Instant,
) -> Result<BootstrapMethod, String> {
    let mut command = Command::new("/bin/launchctl");
    command.args(["bootstrap", domain]).arg(plist);
    let direct_error = match bounded_launchctl(command, deadline) {
        Ok((status, _)) if status.success() => return Ok(BootstrapMethod::Direct),
        Ok((_, diagnostics)) => format!("launchctl bootstrap failed: {diagnostics}"),
        Err(error) => format!("launchctl bootstrap failed: {error}"),
    };

    let broker = match BrokerClient::from_environment() {
        Ok(Some(broker)) => broker,
        Ok(None) => return Err(direct_error),
        Err(error) => return Err(format!("{direct_error}; parent broker rejected: {error}")),
    };
    if let Err(error) = broker.bootstrap(domain, label, plist, deadline) {
        let _ = broker.bootout(domain, label, Instant::now() + Duration::from_secs(2));
        return Err(format!("{direct_error}; {error}"));
    }
    Ok(BootstrapMethod::Broker(broker))
}

fn direct_bootout(domain: &str, label: &str, deadline: Instant) -> Result<(), String> {
    let service = format!("{domain}/{label}");
    let mut command = Command::new("/bin/launchctl");
    command.args(["bootout", &service]);
    match bounded_launchctl(command, deadline) {
        Ok((status, _)) if status.success() => Ok(()),
        Ok((status, diagnostics))
            if status.code() == Some(nix::libc::ESRCH)
                || diagnostics.contains("No such process") =>
        {
            Ok(())
        }
        Ok((_, diagnostics)) => Err(format!("launchctl bootout failed: {diagnostics}")),
        Err(error) => Err(format!("launchctl bootout failed: {error}")),
    }
}
