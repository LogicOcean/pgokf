//! Audited safe wrappers around macOS audit-token and coalition operations.

use std::{io, mem};

const TASK_AUDIT_TOKEN: u32 = 15;
const TASK_AUDIT_TOKEN_COUNT: u32 = 8;
const KERN_SUCCESS: i32 = 0;
const PROC_ALL_PIDS: u32 = 1;
const PROC_PIDT_SHORTBSDINFO: i32 = 13;
const PROC_PIDCOALITIONINFO: i32 = 20;
const PROC_PIDCOALITIONINFO_SIZE: usize = 40;
const PROC_PIDT_SHORTBSDINFO_SIZE: usize = 64;
const SZOMB: u32 = 5;
const ENOENT: i32 = 2;
const ESRCH: i32 = 3;

type AuditToken = [u32; 8];

type ProcPidCoalitionInfo = [u64; 5];
type ProcBsdShortInfo = [u32; 16];

const _: () = assert!(mem::size_of::<ProcPidCoalitionInfo>() == PROC_PIDCOALITIONINFO_SIZE);
const _: () = assert!(mem::size_of::<ProcBsdShortInfo>() == PROC_PIDT_SHORTBSDINFO_SIZE);

/// The exact resource and jetsam coalition identities reported by macOS.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CoalitionId {
    resource_id: u64,
    jetsam_id: u64,
}

/// An inspectable macOS coalition identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoalitionHandle {
    id: CoalitionId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessHandle {
    token: AuditToken,
    pid: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalResult {
    Delivered,
    Gone,
}

struct TaskNamePort(u32);

#[allow(unsafe_code)]
impl Drop for TaskNamePort {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a send right returned by `task_name_for_pid` and
        // this owner releases it exactly once against the current task port.
        unsafe {
            let _ = mach_port_deallocate(mach_task_self_, self.0);
        }
    }
}

impl CoalitionId {
    /// The resource coalition identity.
    #[must_use]
    pub const fn resource_id(self) -> u64 {
        self.resource_id
    }

    /// The jetsam coalition identity.
    #[must_use]
    pub const fn jetsam_id(self) -> u64 {
        self.jetsam_id
    }
}

impl CoalitionHandle {
    /// Capture the exact resource/jetsam coalition pair for `pid`.
    ///
    /// `Ok(None)` means the process vanished while its coalition was inspected.
    /// A short or oversized private-ABI response is rejected rather than used.
    ///
    /// # Errors
    ///
    /// Returns an error when the PID is invalid or libproc cannot return the
    /// exact coalition-info ABI payload.
    pub fn capture(pid: u32) -> Result<Option<Self>, String> {
        coalition_id_for_pid(pid).map(|id| id.map(|id| Self { id }))
    }

    /// Return the exact resource/jetsam coalition pair represented here.
    #[must_use]
    pub const fn id(self) -> CoalitionId {
        self.id
    }

    /// Enumerate live coalition members bound to kernel process identities.
    ///
    /// Each candidate is captured before its coalition is read and captured
    /// again afterward. Only equal before/after audit-token handles are
    /// returned, so PID reuse cannot substitute a different process generation.
    /// Vanished candidates are ignored. A live member whose identity cannot be
    /// bound causes the enumeration to fail closed.
    ///
    /// # Errors
    ///
    /// Returns an error for libproc failures, ABI-size mismatches, or failure to
    /// bind the identity of a process observed in this coalition.
    pub fn members(self) -> Result<Vec<ProcessHandle>, String> {
        let mut members = Vec::new();
        for pid in list_all_pids()? {
            let before = ProcessHandle::capture(pid);
            let Some(candidate_id) = coalition_id_for_pid(pid)? else {
                continue;
            };
            if candidate_id != self.id {
                continue;
            }
            if process_is_zombie(pid)? {
                continue;
            }
            let Some(before) = (match before {
                Ok(before) => before,
                Err(_) => capture_member_identity(pid, self.id, "before inspection")?,
            }) else {
                continue;
            };
            let Some(after) = capture_member_identity(pid, self.id, "after inspection")? else {
                continue;
            };
            if before == after {
                members.push(after);
            }
        }
        members.sort_unstable_by_key(|member| member.pid);
        members.dedup();
        Ok(members)
    }

    /// Signal identity-bound members except for one exact process identity.
    ///
    /// Delivery uses only `proc_signal_with_audittoken`; the stored diagnostic
    /// PID is never used for signaling. Processes that exit after enumeration
    /// are reported as [`SignalResult::Gone`].
    ///
    /// # Errors
    ///
    /// Returns an error if member enumeration fails closed or identity-bound
    /// signal delivery is refused for a reason other than process exit.
    pub fn signal_members_except(
        self,
        signal: i32,
        excluded: &ProcessHandle,
    ) -> Result<Vec<(ProcessHandle, SignalResult)>, String> {
        signal_members_except(self.members()?, signal, excluded)
    }
}

fn capture_member_identity(
    pid: u32,
    expected: CoalitionId,
    phase: &str,
) -> Result<Option<ProcessHandle>, String> {
    let mut first_error = None;
    let mut last_error = None;
    for _ in 0..10 {
        match ProcessHandle::capture(pid) {
            Ok(handle) => return Ok(handle),
            Err(error) => {
                first_error.get_or_insert_with(|| error.clone());
                last_error = Some(error);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    if coalition_id_for_pid(pid)? != Some(expected) || process_is_zombie(pid)? {
        return Ok(None);
    }
    Err(format!(
        "failed to bind coalition member process {pid} {phase}: {}; bounded recapture remained live: {}",
        first_error.unwrap_or_else(|| "unknown capture failure".into()),
        last_error.unwrap_or_else(|| "unknown capture failure".into())
    ))
}

fn signal_members_except(
    members: Vec<ProcessHandle>,
    signal: i32,
    excluded: &ProcessHandle,
) -> Result<Vec<(ProcessHandle, SignalResult)>, String> {
    members
        .into_iter()
        .filter(|member| member != excluded)
        .map(|member| member.signal(signal).map(|result| (member, result)))
        .collect()
}

impl ProcessHandle {
    /// Capture an opaque kernel process-generation token for `pid`.
    ///
    /// `Ok(None)` means the process disappeared before a handle could be bound.
    /// Any permission or kernel-interface failure is returned rather than
    /// falling back to numeric-PID signaling.
    ///
    /// # Errors
    ///
    /// Returns an error when the PID is invalid, permission is denied, or the
    /// kernel audit-token interface cannot bind the process identity.
    pub fn capture(pid: u32) -> Result<Option<Self>, String> {
        let raw_pid = i32::try_from(pid).map_err(|error| error.to_string())?;
        capture(raw_pid, pid)
    }

    /// Return the numeric PID observed when this identity was captured.
    ///
    /// This value is diagnostic metadata only. It can be stale or reused and
    /// must never be used for signaling, authorization, or identity checks.
    #[must_use]
    pub const fn pid(self) -> u32 {
        self.pid
    }

    /// Signal only the process generation represented by this handle.
    ///
    /// # Errors
    ///
    /// Returns an error when the kernel refuses identity-bound delivery for a
    /// reason other than the represented process having exited.
    pub fn signal(self, signal: i32) -> Result<SignalResult, String> {
        signal_bound(self.token, signal)
    }
}

#[allow(unsafe_code)]
fn capture(raw_pid: i32, pid: u32) -> Result<Option<ProcessHandle>, String> {
    let mut port = 0_u32;
    // SAFETY: `port` is a valid out-pointer and `mach_task_self_` is supplied
    // by libSystem for the current process.
    let result = unsafe { task_name_for_pid(mach_task_self_, raw_pid, &raw mut port) };
    if result != KERN_SUCCESS {
        return classify_capture_failure(
            pid,
            format!("task_name_for_pid failed for process {pid}: Mach error {result}"),
        );
    }
    let port = TaskNamePort(port);
    let mut token: AuditToken = [0; 8];
    let mut count = TASK_AUDIT_TOKEN_COUNT;
    // SAFETY: `token` is exactly eight 32-bit words as required by
    // TASK_AUDIT_TOKEN, and `count` describes that writable buffer.
    let result = unsafe {
        task_info(
            port.0,
            TASK_AUDIT_TOKEN,
            token.as_mut_ptr().cast(),
            &raw mut count,
        )
    };
    if result != KERN_SUCCESS || count != TASK_AUDIT_TOKEN_COUNT {
        return classify_capture_failure(
            pid,
            format!(
                "task_info(TASK_AUDIT_TOKEN) failed for process {pid}: Mach error {result}, count {count}"
            ),
        );
    }
    Ok(Some(ProcessHandle { token, pid }))
}

fn classify_capture_failure(
    pid: u32,
    capture_error: String,
) -> Result<Option<ProcessHandle>, String> {
    if process_is_zombie(pid)? {
        return Ok(None);
    }
    match coalition_id_for_pid(pid) {
        Ok(None) => Ok(None),
        Ok(Some(_)) => Err(capture_error),
        Err(probe_error) => Err(format!(
            "{capture_error}; process-presence inspection also failed: {probe_error}"
        )),
    }
}

#[allow(unsafe_code)]
fn clear_errno() {
    // SAFETY: `__error` returns this thread's writable errno slot on macOS.
    unsafe {
        *__error() = 0;
    }
}

fn process_absent(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == ENOENT || code == ESRCH
    )
}

#[allow(unsafe_code)]
fn process_is_zombie(pid: u32) -> Result<bool, String> {
    let raw_pid = i32::try_from(pid).map_err(|error| error.to_string())?;
    let mut info: ProcBsdShortInfo = [0; 16];
    let expected =
        i32::try_from(mem::size_of::<ProcBsdShortInfo>()).map_err(|error| error.to_string())?;
    clear_errno();
    // SAFETY: `info` is the exact public `proc_bsdshortinfo` layout and its
    // writable byte size is passed verbatim to libproc.
    let returned = unsafe {
        proc_pidinfo(
            raw_pid,
            PROC_PIDT_SHORTBSDINFO,
            0,
            (&raw mut info).cast(),
            expected,
        )
    };
    if returned == expected {
        return Ok(info[3] == SZOMB);
    }
    let error = io::Error::last_os_error();
    if returned == 0 && process_absent(&error) {
        Ok(true)
    } else {
        Err(format!(
            "proc_pidinfo(PROC_PIDT_SHORTBSDINFO) returned {returned} bytes for process {pid}, expected {expected}: {error}"
        ))
    }
}

#[allow(unsafe_code)]
fn coalition_id_for_pid(pid: u32) -> Result<Option<CoalitionId>, String> {
    let raw_pid = i32::try_from(pid).map_err(|error| error.to_string())?;
    let mut info: ProcPidCoalitionInfo = [0; 5];
    let expected =
        i32::try_from(mem::size_of::<ProcPidCoalitionInfo>()).map_err(|error| error.to_string())?;
    // SAFETY: `info` is a writable five-word array whose exact 40-byte size is
    // passed to libproc. Any different returned size is rejected below.
    clear_errno();
    let returned = unsafe {
        proc_pidinfo(
            raw_pid,
            PROC_PIDCOALITIONINFO,
            0,
            (&raw mut info).cast(),
            expected,
        )
    };
    if returned == expected {
        return Ok(Some(CoalitionId {
            resource_id: info[0],
            jetsam_id: info[1],
        }));
    }

    let error = io::Error::last_os_error();
    if returned == 0 && process_absent(&error) {
        Ok(None)
    } else {
        Err(format!(
            "proc_pidinfo(PROC_PIDCOALITIONINFO) returned {returned} bytes for process {pid}, expected {expected}: {error}"
        ))
    }
}

#[allow(unsafe_code)]
fn list_all_pids() -> Result<Vec<u32>, String> {
    // SAFETY: a null buffer with size zero asks libproc for its current byte
    // requirement and does not permit a write.
    clear_errno();
    let needed = unsafe { proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Err(format!(
            "proc_listpids(PROC_ALL_PIDS) sizing failed: {}",
            io::Error::last_os_error()
        ));
    }

    let pid_size = mem::size_of::<i32>();
    let needed = usize::try_from(needed).map_err(|error| error.to_string())?;
    if needed % pid_size != 0 {
        return Err(format!(
            "proc_listpids(PROC_ALL_PIDS) sizing returned {needed} bytes, not a multiple of PID size {pid_size}"
        ));
    }
    let mut slots = needed
        .checked_div(pid_size)
        .and_then(|count| count.checked_add(1024))
        .ok_or_else(|| "proc_listpids buffer size overflow".to_owned())?;

    loop {
        let mut pids = vec![0_i32; slots];
        let byte_capacity = pids
            .len()
            .checked_mul(pid_size)
            .ok_or_else(|| "proc_listpids buffer size overflow".to_owned())?;
        let raw_capacity = i32::try_from(byte_capacity)
            .map_err(|_| "proc_listpids buffer exceeds its i32 ABI size".to_owned())?;
        // SAFETY: `pids` owns `byte_capacity` writable bytes, and that exact
        // capacity is passed to libproc.
        clear_errno();
        let returned =
            unsafe { proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr().cast(), raw_capacity) };
        if returned <= 0 {
            return Err(format!(
                "proc_listpids(PROC_ALL_PIDS) failed: {}",
                io::Error::last_os_error()
            ));
        }
        let returned = usize::try_from(returned).map_err(|error| error.to_string())?;
        if returned > byte_capacity || returned % pid_size != 0 {
            return Err(format!(
                "proc_listpids(PROC_ALL_PIDS) returned invalid byte count {returned} for buffer size {byte_capacity}"
            ));
        }
        if returned == byte_capacity {
            slots = slots
                .checked_mul(2)
                .ok_or_else(|| "proc_listpids buffer size overflow".to_owned())?;
            continue;
        }

        pids.truncate(returned / pid_size);
        let mut result = Vec::with_capacity(pids.len());
        for pid in pids {
            if pid == 0 {
                continue;
            }
            result.push(u32::try_from(pid).map_err(|_| {
                format!("proc_listpids(PROC_ALL_PIDS) returned invalid negative PID {pid}")
            })?);
        }
        result.sort_unstable();
        result.dedup();
        return Ok(result);
    }
}

#[allow(unsafe_code)]
fn signal_bound(mut token: AuditToken, signal: i32) -> Result<SignalResult, String> {
    // SAFETY: `token` came from TASK_AUDIT_TOKEN and points to a live local
    // value for the duration of the call. The kernel validates its PID version
    // atomically with signal delivery.
    if unsafe { proc_signal_with_audittoken(&raw mut token, signal) } == 0 {
        return Ok(SignalResult::Delivered);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(3) {
        Ok(SignalResult::Gone)
    } else {
        Err(format!("audit-token signal failed: {error}"))
    }
}

#[allow(unsafe_code)]
unsafe extern "C" {
    static mach_task_self_: u32;
    fn __error() -> *mut i32;
    fn task_name_for_pid(target_tport: u32, pid: i32, task_name: *mut u32) -> i32;
    fn task_info(
        target_task: u32,
        flavor: u32,
        task_info_out: *mut i32,
        task_info_count: *mut u32,
    ) -> i32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
    fn proc_listpids(
        pid_type: u32,
        type_info: u32,
        buffer: *mut core::ffi::c_void,
        buffer_size: i32,
    ) -> i32;
    fn proc_pidinfo(
        pid: i32,
        flavor: i32,
        arg: u64,
        buffer: *mut core::ffi::c_void,
        buffer_size: i32,
    ) -> i32;
    fn proc_signal_with_audittoken(token: *mut AuditToken, signal: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{process::Command, thread, time::Duration};

    #[test]
    fn stale_handle_cannot_signal_another_process() {
        let mut original = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let handle = ProcessHandle::capture(original.id()).unwrap().unwrap();
        original.kill().unwrap();
        original.wait().unwrap();

        let mut unrelated = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        assert_eq!(handle.signal(9).unwrap(), SignalResult::Gone);
        thread::sleep(Duration::from_millis(20));
        assert!(unrelated.try_wait().unwrap().is_none());
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
    }

    #[test]
    fn handle_signals_only_its_bound_process() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let handle = ProcessHandle::capture(child.id()).unwrap().unwrap();
        assert_eq!(handle.signal(9).unwrap(), SignalResult::Delivered);
        assert!(!child.wait().unwrap().success());
    }

    #[test]
    fn coalition_info_layout_matches_private_libproc_abi() {
        let words: ProcPidCoalitionInfo = [0; 5];
        assert_eq!(std::mem::size_of_val(&words), 40);
        assert_eq!(std::mem::align_of::<ProcPidCoalitionInfo>(), 8);
        assert_eq!(
            words.as_ptr().wrapping_add(1).addr() - words.as_ptr().addr(),
            8
        );
    }

    #[test]
    fn captures_current_process_coalition_and_member_identity() {
        let pid = std::process::id();
        let coalition = CoalitionHandle::capture(pid).unwrap().unwrap();
        assert_ne!(coalition.id().resource_id(), 0);
        assert_ne!(coalition.id().jetsam_id(), 0);

        let current = ProcessHandle::capture(pid).unwrap().unwrap();
        assert_eq!(current.pid(), pid);
        let members = coalition.members().unwrap();
        assert!(members.contains(&current));
        assert!(members.iter().any(|member| member.pid() == pid));
    }

    #[test]
    fn signal_members_except_omits_the_exact_identity() {
        let pid = std::process::id();
        let excluded = ProcessHandle::capture(pid).unwrap().unwrap();

        let outcomes = signal_members_except(vec![excluded], 9, &excluded).unwrap();
        assert!(outcomes.is_empty());
    }

    #[test]
    fn members_exclude_a_live_pid_from_another_coalition_when_available() {
        let coalition = CoalitionHandle::capture(std::process::id())
            .unwrap()
            .unwrap();
        let Some(nonmember_pid) = list_all_pids().unwrap().into_iter().find(|&pid| {
            matches!(
                coalition_id_for_pid(pid),
                Ok(Some(id)) if id != coalition.id()
            )
        }) else {
            return;
        };

        assert!(
            coalition
                .members()
                .unwrap()
                .iter()
                .all(|member| member.pid() != nonmember_pid)
        );
    }
}
