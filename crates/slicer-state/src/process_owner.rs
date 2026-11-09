//! Whether the process that wrote a pid down is still the one running
//! (spec §6.7). The project lock and the autosave manifest both name their
//! writer by pid, and both ask this module about it.
//!
//! A pid alone proves little: Windows hands a freed pid to the next process
//! within moments, and every OS does in time. So a record also carries when
//! its writer started, as the OS keeps it, and a process that holds the pid
//! but started at another time is somebody else — the writer is gone.
//!
//! The start is an opaque number per OS, only ever compared with a fresh
//! reading on the same host: `FILETIME` ticks from `GetProcessTimes` on
//! Windows, microseconds since the epoch from `proc_pidinfo` on macOS, clock
//! ticks since boot from `/proc/<pid>/stat` on Linux. Each stays the same for
//! the life of a process, so a reading never disowns a live writer.

use std::sync::OnceLock;

/// What became of the process a record names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// The pid runs, and it started when the record says.
    Running,
    /// Proven gone: no process holds the pid, the one holding it has exited,
    /// or the one holding it now started at another time.
    Gone,
    /// The pid runs, but nothing proves it is the writer; says why. What the
    /// record guards stays, and the reason goes to whoever asked.
    Unverified(String),
}

/// When this process started, as the OS keeps it: what a record written now
/// carries. `None` where the OS won't say — such a record is later judged
/// [`Owner::Unverified`] for as long as its pid runs.
pub fn this_process_start() -> Option<u64> {
    static START: OnceLock<Option<u64>> = OnceLock::new();
    *START.get_or_init(|| start_of(std::process::id()))
}

/// When the running process `pid` started, as the OS keeps it. `None` when
/// no running process holds the pid, or its start can't be read.
pub fn start_of(pid: u32) -> Option<u64> {
    match look(pid) {
        Seen::Running(Ok(started)) => Some(started),
        Seen::Running(Err(_)) | Seen::Absent => None,
    }
}

/// What became of the process `pid`, recorded as started at `started`
/// (`None`: a record from before starts were kept).
pub fn probe(pid: u32, started: Option<u64>) -> Owner {
    // Not a process anybody could have written down: on Unix `kill(0, …)`
    // means the caller's own process group, on Windows pid 0 is the idle
    // process.
    if pid == 0 {
        return Owner::Gone;
    }
    match look(pid) {
        Seen::Absent => Owner::Gone,
        Seen::Running(now) => judge(pid, started, now),
    }
}

/// What the OS says about a pid.
enum Seen {
    /// No process holds it, or the one that does has exited — a zombie not
    /// yet waited for, or on Windows a process object some handle keeps.
    Absent,
    /// A running process holds it: when that one started, or why that
    /// can't be read.
    Running(Result<u64, String>),
}

/// The verdict on a running `pid`, from the start the record carries and the
/// one the OS reports now.
fn judge(pid: u32, recorded: Option<u64>, now: Result<u64, String>) -> Owner {
    match (recorded, now) {
        (None, _) => Owner::Unverified(format!(
            "pid {pid} is running, but the record of it was written by an older version \
             that did not note when its writer started, so pid {pid} may be another program now"
        )),
        (Some(recorded), Ok(now)) if recorded == now => Owner::Running,
        (Some(_), Ok(_)) => Owner::Gone,
        (Some(_), Err(why)) => Owner::Unverified(format!(
            "pid {pid} is running, but when it started could not be read: {why}"
        )),
    }
}

#[cfg(unix)]
fn look(pid: u32) -> Seen {
    // A pid past `pid_t` names no process; cast as it is, it would turn
    // negative and `kill` would read it as a process group.
    let Ok(raw) = libc::pid_t::try_from(pid) else {
        return Seen::Absent;
    };
    // SAFETY: signal 0 delivers nothing; `kill` only reports whether the pid
    // exists (EPERM: it does, under another user).
    let found = unsafe { libc::kill(raw, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !found {
        return Seen::Absent;
    }
    look_up_start(raw)
}

/// `/proc/<pid>/stat` reads `pid (comm) state … starttime …`: the state is
/// field 3, the start field 22, in clock ticks since boot. `comm` may hold
/// spaces and parentheses, so the fields are counted from its last `)`.
#[cfg(target_os = "linux")]
fn look_up_start(pid: libc::pid_t) -> Seen {
    let path = format!("/proc/{pid}/stat");
    let stat = match std::fs::read_to_string(&path) {
        Ok(stat) => stat,
        // Exited and waited for since `kill` saw it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Seen::Absent,
        Err(e) => return Seen::Running(Err(format!("read {path}: {e}"))),
    };
    let fields: Vec<&str> = stat
        .rsplit_once(')')
        .map(|(_, rest)| rest.split_whitespace().collect())
        .unwrap_or_default();
    // Zombie, or dead and about to vanish.
    if matches!(fields.first(), Some(&"Z") | Some(&"X")) {
        return Seen::Absent;
    }
    const START_AFTER_COMM: usize = 22 - 3;
    let started = fields
        .get(START_AFTER_COMM)
        .and_then(|field| field.parse::<u64>().ok())
        .ok_or_else(|| format!("{path} has no start time: {stat:?}"));
    Seen::Running(started)
}

#[cfg(target_os = "macos")]
fn look_up_start(pid: libc::pid_t) -> Seen {
    // SAFETY: `proc_bsdinfo` is plain integers; all-zero is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` outlives the call and `size` is its exact size;
    // `proc_pidinfo` writes at most that many bytes into it.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    if written != size {
        let error = std::io::Error::last_os_error();
        // Exited and waited for since `kill` saw it.
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Seen::Absent;
        }
        // Another user's process answers EPERM: it runs, but whose it is
        // stays unknown.
        return Seen::Running(Err(format!("proc_pidinfo: {error}")));
    }
    if info.pbi_status == libc::SZOMB {
        return Seen::Absent;
    }
    Seen::Running(Ok(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec))
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn look_up_start(_pid: libc::pid_t) -> Seen {
    Seen::Running(Err("process start times are not read on this OS".into()))
}

#[cfg(windows)]
fn look(pid: u32) -> Seen {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_INVALID_PARAMETER};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // SAFETY: plain call; a null handle is checked below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        // SAFETY: reads this thread's last-error value, set by OpenProcess.
        let code = unsafe { GetLastError() };
        // No process object holds the pid — not even an exited one that a
        // handle keeps.
        if code == ERROR_INVALID_PARAMETER {
            return Seen::Absent;
        }
        // ERROR_ACCESS_DENIED and the like: a process holds the pid, but
        // whose it is stays unknown.
        let error = std::io::Error::from_raw_os_error(code as i32);
        return Seen::Running(Err(format!("OpenProcess: {error}")));
    }
    let seen = look_through(handle);
    // SAFETY: `handle` came from OpenProcess above and is closed once.
    unsafe { CloseHandle(handle) };
    seen
}

/// What the open process `handle` says: whether it has exited, and if not,
/// when it started.
#[cfg(windows)]
fn look_through(handle: windows_sys::Win32::Foundation::HANDLE) -> Seen {
    use windows_sys::Win32::Foundation::{FILETIME, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, GetProcessTimes};

    let mut exit_code = 0u32;
    // SAFETY: `handle` is open with PROCESS_QUERY_LIMITED_INFORMATION, which
    // both calls need; every out-pointer is a live local.
    if unsafe { GetExitCodeProcess(handle, &mut exit_code) } == 0 {
        let error = std::io::Error::last_os_error();
        return Seen::Running(Err(format!("GetExitCodeProcess: {error}")));
    }
    // A process that exited with 259 itself would read as running; the
    // start time is what tells such a pid's later holder apart.
    if exit_code != STILL_ACTIVE as u32 {
        return Seen::Absent;
    }
    let zero = || FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero(), zero(), zero(), zero());
    // SAFETY: as above.
    let read =
        unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    if read == 0 {
        let error = std::io::Error::last_os_error();
        return Seen::Running(Err(format!("GetProcessTimes: {error}")));
    }
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    Seen::Running(Ok(ticks))
}

/// Child processes for the tests of this crate.
#[cfg(test)]
pub(crate) mod test_support {
    use std::process::{Child, Command, Stdio};

    /// A child that runs until it is stopped or dropped: a process other
    /// than this one, alive for the length of a test.
    pub(crate) struct Sleeper(Child);

    impl Sleeper {
        pub(crate) fn spawn() -> Self {
            #[cfg(unix)]
            let mut command = Command::new("sleep");
            #[cfg(unix)]
            command.arg("120");
            #[cfg(windows)]
            let mut command = Command::new("ping");
            #[cfg(windows)]
            command.args(["-n", "120", "127.0.0.1"]);
            let child = command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn a sleeping child");
            Self(child)
        }

        pub(crate) fn pid(&self) -> u32 {
            self.0.id()
        }

        pub(crate) fn child(&mut self) -> &mut Child {
            &mut self.0
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A child that has run to completion and been waited for. The `Child`
    /// is handed back: on Windows it still holds the process handle, which
    /// keeps the exited process object — and its pid — around.
    pub(crate) fn finished_child() -> Child {
        #[cfg(unix)]
        let mut command = Command::new("true");
        #[cfg(windows)]
        let mut command = Command::new("cmd");
        #[cfg(windows)]
        command.args(["/C", "exit 0"]);
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a child that exits at once");
        child.wait().expect("wait for the child");
        child
    }

    /// The pid of a child that has exited, been waited for and let go of:
    /// no process holds it, unless the OS has handed it on since.
    pub(crate) fn finished_pid() -> u32 {
        finished_child().id()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{finished_child, finished_pid, Sleeper};
    use super::*;

    #[test]
    fn this_process_is_running() {
        let started = this_process_start();

        assert!(
            started.is_some(),
            "the OS must say when this process started"
        );
        assert_eq!(probe(std::process::id(), started), Owner::Running);
    }

    #[test]
    fn another_running_process_is_running() {
        let sleeper = Sleeper::spawn();
        let started = start_of(sleeper.pid());

        assert!(started.is_some(), "the start of a child must be readable");
        assert_ne!(started, this_process_start());
        assert_eq!(probe(sleeper.pid(), started), Owner::Running);
    }

    #[test]
    fn the_pid_of_a_finished_child_is_gone() {
        assert_eq!(probe(finished_pid(), None), Owner::Gone);
    }

    // On Windows the waited-for child's handle keeps its process object, so
    // `OpenProcess` still succeeds on the pid: only the exit code tells.
    #[test]
    fn a_finished_child_is_gone_while_its_handle_is_open() {
        let mut child = finished_child();

        assert_eq!(probe(child.id(), None), Owner::Gone);
        // Waited for already; this only keeps the handle open up to here.
        child.wait().unwrap();
    }

    #[test]
    fn a_stopped_child_is_gone_though_its_start_matches() {
        let mut sleeper = Sleeper::spawn();
        let started = start_of(sleeper.pid());
        assert_eq!(probe(sleeper.pid(), started), Owner::Running);

        sleeper.child().kill().unwrap();
        sleeper.child().wait().unwrap();

        assert_eq!(probe(sleeper.pid(), started), Owner::Gone);
    }

    // A killed child its parent has not waited for yet is a zombie: `kill`
    // still finds its pid, though the process has exited.
    #[cfg(unix)]
    #[test]
    fn an_exited_child_not_yet_waited_for_is_gone() {
        let mut sleeper = Sleeper::spawn();
        let started = start_of(sleeper.pid());
        sleeper.child().kill().unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut owner = probe(sleeper.pid(), started);
        while owner != Owner::Gone && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            owner = probe(sleeper.pid(), started);
        }

        assert_eq!(owner, Owner::Gone);
    }

    #[test]
    fn this_pid_recorded_with_another_start_is_gone() {
        let started = this_process_start().expect("this process's start");

        assert_eq!(probe(std::process::id(), Some(started + 1)), Owner::Gone);
        assert_eq!(probe(std::process::id(), Some(started - 1)), Owner::Gone);
    }

    #[test]
    fn a_running_pid_recorded_without_a_start_is_unverified() {
        let sleeper = Sleeper::spawn();

        match probe(sleeper.pid(), None) {
            Owner::Unverified(why) => {
                assert!(why.contains(&sleeper.pid().to_string()), "{why}");
                assert!(why.contains("older version"), "{why}");
            }
            other => panic!("expected Unverified, got {other:?}"),
        }
    }

    // Another user's process on macOS, a protected one on Windows: the pid
    // runs, its start stays unread, and the reason travels with the verdict.
    #[test]
    fn a_running_pid_whose_start_cannot_be_read_is_unverified() {
        match judge(7, Some(1), Err("access denied".into())) {
            Owner::Unverified(why) => {
                assert!(why.contains("pid 7"), "{why}");
                assert!(why.contains("access denied"), "{why}");
            }
            other => panic!("expected Unverified, got {other:?}"),
        }
    }

    #[test]
    fn pid_zero_is_gone() {
        assert_eq!(probe(0, None), Owner::Gone);
    }
}
