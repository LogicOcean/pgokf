//! Bounded argv execution for public verification commands.
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, process::Output, time::Duration};

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
}

impl CommandSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            env_remove: Vec::new(),
            stdin: Vec::new(),
            timeout: Duration::from_mins(2),
            output_cap: 16 * 1024 * 1024,
        }
    }

    /// Execute without a shell, retaining at most `output_cap` bytes per stream.
    /// # Errors
    /// Spawn, I/O, timeout, unsupported platform, and output overflow fail closed.
    pub fn run(&self) -> Result<Output, String> {
        #[cfg(unix)]
        {
            unix::run(self).map_err(|error| format!("{}: {error}", self.program.display()))
        }
        #[cfg(not(unix))]
        {
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

#[cfg(unix)]
mod unix {
    use super::{CommandSpec, Duration, Output};
    use nix::{
        fcntl::{FcntlArg, OFlag, fcntl},
        sys::signal::{Signal, killpg},
        unistd::Pid,
    };
    use std::{
        io::{Read, Seek, Write},
        os::{fd::AsFd, unix::process::CommandExt},
        process::{Child, Command, Stdio},
        time::Instant,
    };

    struct OwnedChild {
        child: Child,
        group: Pid,
    }
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = killpg(self.group, Signal::SIGKILL);
            let _ = self.child.kill();
            let _ = self.child.wait();
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

    fn drain(reader: &mut impl Read, output: &mut Vec<u8>, cap: usize) -> Result<(), String> {
        // One bounded read per iteration ensures a noisy stream cannot starve
        // its sibling or bypass the deadline.
        let mut bytes = [0_u8; 8192];
        match reader.read(&mut bytes) {
            Ok(count) => {
                if count > cap.saturating_sub(output.len()) {
                    return Err("output cap exceeded".into());
                }
                output.extend_from_slice(&bytes[..count]);
                Ok(())
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    pub(super) fn run(spec: &CommandSpec) -> Result<Output, String> {
        if spec.timeout.is_zero() || spec.output_cap == 0 {
            return Err("positive timeout and output cap required".into());
        }
        let mut input = tempfile::tempfile().map_err(|e| e.to_string())?;
        input.write_all(&spec.stdin).map_err(|e| e.to_string())?;
        input.rewind().map_err(|e| e.to_string())?;
        let mut command = Command::new(&spec.program);
        for name in &spec.env_remove {
            command.env_remove(name);
        }
        command
            .args(&spec.args)
            .envs(&spec.env)
            .stdin(input)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        let child = command.spawn().map_err(|e| e.to_string())?;
        let group = Pid::from_raw(i32::try_from(child.id()).map_err(|e| e.to_string())?);
        let mut owned = OwnedChild { child, group };
        let mut stdout = owned.child.stdout.take().ok_or("missing stdout pipe")?;
        let mut stderr = owned.child.stderr.take().ok_or("missing stderr pipe")?;
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        let start = Instant::now();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let status = loop {
            drain(&mut stdout, &mut out, spec.output_cap)?;
            drain(&mut stderr, &mut err, spec.output_cap)?;
            if let Some(status) = owned.child.try_wait().map_err(|e| e.to_string())? {
                break status;
            }
            if start.elapsed() >= spec.timeout {
                return Err("timeout exceeded".into());
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        // End descendants even if the direct process left inherited pipes open.
        let _ = killpg(group, Signal::SIGKILL);
        loop {
            let before = (out.len(), err.len());
            drain(&mut stdout, &mut out, spec.output_cap)?;
            drain(&mut stderr, &mut err, spec.output_cap)?;
            if before == (out.len(), err.len()) {
                break;
            }
            if start.elapsed() >= spec.timeout {
                return Err("timeout exceeded while draining".into());
            }
        }
        Ok(Output {
            status,
            stdout: out,
            stderr: err,
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

    fn shell(source: &str) -> CommandSpec {
        let mut spec = CommandSpec::new("/bin/sh");
        spec.args = ["-c", source].map(OsString::from).to_vec();
        spec.timeout = Duration::from_secs(2);
        spec
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
        let mut spec = shell("sleep 30");
        spec.timeout = Duration::from_millis(50);
        let start = Instant::now();
        assert!(spec.run().unwrap_err().contains("timeout"));
        assert!(start.elapsed() < Duration::from_secs(2));
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
        assert!(shell("sleep 30 & printf done").checked().is_ok());
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
