//! Run the B-rep kernel in a child process, so that OCCT taking the
//! process down takes the child down instead of the daemon.
//!
//! Not a hypothetical. Measured 2026-08-21 on `main` at 28ef7cc:
//!
//! ```json
//! {"op":"chamfer","distance":4.073,"edges":"all",
//!  "child":{"op":"union","children":[
//!    {"op":"cylinder","diameter":102.5,"height":64.28,"segments":48},
//!    {"op":"box","size":[152.42,119.66,77.54]}]}}
//! ```
//!
//! segfaults inside `ChFi3d_IsInFront`, reached from
//! `BRepFilletAPI_MakeChamfer::Build`. That is a null dereference, not a
//! `Standard_Failure`, so the `try`/`catch` the bridge wraps every OCCT
//! call in cannot see it — and `geometry.build` runs in the daemon
//! process, so one generated recipe ended every other client's session
//! too.
//!
//! The isolation is opt-in rather than automatic because it needs a
//! binary that can re-enter itself: [`use_worker`] is called by the
//! daemon with its own path. Anything else — the test binaries, an
//! embedder linking this crate — keeps building in process, which is
//! what it was doing before.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::Instant;

use slicer_geometry::CsgNode;

/// Exit code the worker uses for a part OCCT refused cleanly, with the reason in
/// an `{"error": …}` line on stdout, to tell it apart from a crash. Not 2: that
/// is clap's usage-error code, so a binary without `build-geometry` would read
/// as a refusal "without saying why" instead of as the misconfiguration it is,
/// which `EXIT_USAGE` tells.
pub const EXIT_REFUSED: i32 = 3;

/// Exit code of clap's usage error: what a binary that does not know
/// `build-geometry` answers the worker's command line with, because it is not
/// this daemon or is an older build of it.
const EXIT_USAGE: i32 = 2;

/// Exit code a Rust program leaves when a panic unwinds out of its main thread.
/// In the worker that is a panic in this daemon's own build code, and the same
/// part panics there the same way every time.
const EXIT_PANIC: i32 = 101;

/// Windows reports an unhandled exception as the process's exit code: an
/// NTSTATUS, whose two top bits are set when it is an error —
/// `STATUS_ACCESS_VIOLATION` is 0xC0000005, and `STATUS_STACK_BUFFER_OVERRUN`,
/// which is where Rust's `abort` ends, is 0xC0000409.
const NTSTATUS_ERROR_MIN: u32 = 0xC000_0000;

/// How a console ends the process it stops: Ctrl+C, or the console closing.
const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;
/// The loader found no DLL the worker needs, so it never started.
const STATUS_DLL_NOT_FOUND: u32 = 0xC000_0135;
/// The loader found a DLL without an entry point the worker needs.
const STATUS_ENTRYPOINT_NOT_FOUND: u32 = 0xC000_0139;
/// A DLL the worker needs failed to initialise.
const STATUS_DLL_INIT_FAILED: u32 = 0xC000_0142;
/// An image of the wrong format, a 32-bit DLL in a 64-bit process, say.
const STATUS_INVALID_IMAGE_FORMAT: u32 = 0xC000_007B;

/// The statuses in the error range that are still not an exception of the
/// build: somebody stopped the worker, or it could not start. Neither says
/// anything about the part, so neither is a crash.
const NOT_CRASH_STATUSES: [u32; 5] = [
    STATUS_CONTROL_C_EXIT,
    STATUS_DLL_NOT_FOUND,
    STATUS_ENTRYPOINT_NOT_FOUND,
    STATUS_DLL_INIT_FAILED,
    STATUS_INVALID_IMAGE_FORMAT,
];

// Unix signal numbers, named. `std` offers none and `libc` is not a dependency
// of this crate. They only mean something on unix, where a worker can be ended
// by one; elsewhere `signal_of` finds none and they are never compared with.

/// Illegal instruction.
const SIGILL: i32 = 4;
/// Trace or breakpoint trap.
const SIGTRAP: i32 = 5;
/// `abort()`: where an uncaught C++ exception, a failed assertion and a Rust
/// `abort` end.
const SIGABRT: i32 = 6;
/// Floating-point or integer arithmetic fault.
const SIGFPE: i32 = 8;
/// The signal that ends a process without letting it say anything, and that the
/// system sends to one it has no memory left for. OCCT does not die of it: it
/// dies of one of the crash signals.
const SIGKILL: i32 = 9;
/// Invalid memory access: a null dereference.
const SIGSEGV: i32 = 11;
/// Misaligned or unmapped memory access. The one number here that differs
/// between the unix families this daemon is built for: Linux makes it 7, macOS
/// and the BSDs 10.
#[cfg(any(target_os = "linux", target_os = "android"))]
const SIGBUS: i32 = 7;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const SIGBUS: i32 = 10;

/// The signals a process dies of when it did something the machine cannot go
/// on from, and the only ones that blame the part: OCCT crashes this way, and
/// the same part crashes it the same way again. Every other signal is somebody
/// sending it — Ctrl+C, a logout, "Quit" in Activity Monitor, the system short
/// of memory — and says nothing about the part.
const CRASH_SIGNALS: [i32; 6] = [SIGILL, SIGTRAP, SIGABRT, SIGFPE, SIGSEGV, SIGBUS];

/// How long one isolated build may take. Below the UI's 90 s RPC timeout for
/// `geometry.build`, so the daemon answers with a reason before the client
/// gives up and drops the socket.
pub const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(85);

/// How often the parent looks at whether the worker has exited.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

/// Why a B-rep build gave no part. The kinds are told apart by whether building
/// the same tree again could end any differently — see
/// [`is_transient`](BrepFailure::is_transient). Each carries the whole message,
/// written for whoever reads it, and `Display` prints it as it is.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrepFailure {
    /// The kernel looked at the tree and said no, and said why: OCCT's own
    /// refusal (exit code 3 and an `{"error": …}` line), a tree that cannot be
    /// put in the worker's format, a tree with `hull`, which OCCT has no
    /// primitive for, a daemon built without OCCT. The same tree gets the same
    /// answer.
    #[error("{0}")]
    Refused(String),
    /// The worker died while building, the way OCCT dies, and this tree makes it
    /// die that way every time. Told by an allow-list of ways to end and by
    /// nothing else: a crash signal (SIGSEGV, SIGBUS, SIGABRT, SIGILL, SIGFPE,
    /// SIGTRAP), a Windows exception code, exit code 101 (a panic in this
    /// daemon's own build code), or exit code 3 with no `{"error": …}` line,
    /// which is how MSVC's `abort()` ends a process. Every other end may be
    /// anyone's doing — another signal, an exit code nobody chose — and is
    /// [`Unavailable`](Self::Unavailable).
    #[error("{0}")]
    Crashed(String),
    /// The worker did not finish by the deadline and was killed. The machine's
    /// doing as much as the tree's: a quieter machine finishes what a busy one
    /// did not.
    #[error("{0}")]
    TimedOut(String),
    /// The build could not be run, or ended without telling anything about the
    /// tree: the tree could not be written for the worker, the worker could not
    /// be started or waited for, what it printed could not be read or holds no
    /// answer, a finished part could not be put in place. Or the worker ended in
    /// a way that is not a crash of the kernel: it was stopped from outside —
    /// SIGKILL, as the system sends when it is out of memory, SIGTERM, SIGINT,
    /// SIGHUP, SIGQUIT, Ctrl+C on Windows — or it is a binary without
    /// `build-geometry`, which answers with clap's usage code, or it exited with
    /// a code nobody chose. The cause is the machine's or the setup's.
    #[error("{0}")]
    Unavailable(String),
}

impl BrepFailure {
    /// Whether building the same tree again could succeed: when the failure is
    /// the machine's or the setup's — a missed deadline, a build that could not
    /// be run, a worker stopped from outside, an answer that never came — and
    /// not the tree's. A refusal or a crash comes back every time, so there is
    /// nothing to gain from asking again: and what a part's `csg` node builds
    /// after one is kept for good, where after a transient failure it is used
    /// once and built again at the next regeneration.
    pub fn is_transient(&self) -> bool {
        matches!(self, BrepFailure::TimedOut(_) | BrepFailure::Unavailable(_))
    }
}

static WORKER: OnceLock<PathBuf> = OnceLock::new();

/// Point B-rep builds at `exe`, which must accept `build-geometry
/// --tree <path> --output <path>` and answer as [`run_worker`] does.
///
/// Called once, at startup, by the daemon with its own executable. Later
/// calls are ignored rather than refused: the setting is a property of
/// the process, and a second caller asking for the same thing is not an
/// error worth failing a startup over.
pub fn use_worker(exe: PathBuf) {
    let _ = WORKER.set(exe);
}

/// Whether builds are isolated in this process, for callers that want to
/// say so in a log line.
pub fn is_isolated() -> bool {
    WORKER.get().is_some()
}

/// Build `tree` into `out` as STEP and answer its volume in mm³.
///
/// Isolated in a child process when one is configured, in process
/// otherwise. Both paths produce the same file and the same errors; only
/// a crash tells them apart, and only one of them survives it. In process a
/// build can only be refused: a crash takes the caller with it, and nothing
/// times the call out.
pub fn build_step(tree: &CsgNode, out: &Path) -> Result<f64, BrepFailure> {
    match WORKER.get() {
        Some(exe) => build_in_child_within(exe, tree, out, DEFAULT_TIMEOUT),
        None => build_in_process(tree, out).map_err(BrepFailure::Refused),
    }
}

#[cfg(feature = "with-libslic3r")]
fn build_in_process(tree: &CsgNode, out: &Path) -> Result<f64, String> {
    slicer_ffi::brep::build_step(tree, out)
        .map(|built| built.volume_mm3)
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "with-libslic3r"))]
fn build_in_process(_tree: &CsgNode, _out: &Path) -> Result<f64, String> {
    Err("this daemon was built without OCCT".to_string())
}

/// The child side: build the tree named by `tree_path` and report on
/// stdout. Returns the process exit code the caller should exit with.
///
/// Deliberately total — it prints and returns a code rather than
/// propagating, because its whole job is to be a process boundary and a
/// panic on this side would look to the parent exactly like the crash it
/// exists to contain.
pub fn run_worker(tree_path: &Path, out: &Path) -> i32 {
    let tree = match std::fs::read(tree_path)
        .map_err(|e| format!("cannot read the tree at {}: {e}", tree_path.display()))
        .and_then(|bytes| {
            serde_json::from_slice::<CsgNode>(&bytes).map_err(|e| format!("invalid tree: {e}"))
        }) {
        Ok(tree) => tree,
        Err(why) => {
            println!("{}", report(&Err(why)));
            return EXIT_REFUSED;
        }
    };
    let built = build_in_process(&tree, out);
    let refused = built.is_err();
    println!("{}", report(&built));
    if refused {
        EXIT_REFUSED
    } else {
        0
    }
}

fn report(built: &Result<f64, String>) -> String {
    match built {
        Ok(volume) => serde_json::json!({ "volume_mm3": volume }).to_string(),
        Err(why) => serde_json::json!({ "error": why }).to_string(),
    }
}

/// The parent side of one isolated build, with a deadline. Public for the
/// tests that drive it with fake workers; production goes through
/// [`build_step`].
///
/// The worker's stdout is read on a thread of its own while this side polls
/// for its exit; see [`drain`] for why that cannot wait until it has gone.
#[doc(hidden)]
pub fn build_in_child_within(
    exe: &Path,
    tree: &CsgNode,
    out: &Path,
    timeout: std::time::Duration,
) -> Result<f64, BrepFailure> {
    // Beside the output, which is already a per-call temporary path in the
    // object cache: two builds running at once must not share this file.
    let tree_path = out.with_extension("tree.json");
    let json = serde_json::to_vec(tree)
        .map_err(|e| BrepFailure::Refused(format!("cannot write the tree: {e}")))?;
    std::fs::write(&tree_path, json)
        .map_err(|e| BrepFailure::Unavailable(format!("cannot write the tree: {e}")))?;

    let ran = run_child(exe, &tree_path, out, timeout);
    let _ = std::fs::remove_file(&tree_path);
    let Finished { status, stdout } = ran?;
    classify(status, stdout)
}

/// What one finished run of the worker comes to, from how it ended and what it
/// printed. Pure, so that every way a worker can end is checked without running
/// one.
///
/// How it ended comes first, and only the ways OCCT is known to end blame the
/// part. That is an allow-list on purpose: a part that is blamed is not asked
/// again, and what a `csg` node built after it stays for good, so an end that
/// was somebody else's doing — Ctrl+C, a logout, "Quit" on a hung worker in
/// Activity Monitor — must never read as the part's.
///
/// - exit 0: it answered, in the last line that reads `{"volume_mm3": …}`.
///   With none, nothing is known: `Unavailable`.
/// - [`EXIT_REFUSED`]: OCCT said no, in the last line that reads
///   `{"error": …}`, and the part will get the same answer again. With none the
///   worker did not say it, and that is how MSVC's `abort()` ends a process: a
///   crash.
/// - a crash signal ([`CRASH_SIGNALS`]), a Windows exception code or
///   [`EXIT_PANIC`]: a crash. OCCT, or this daemon's own build code, ends the
///   same way on the same part every time.
/// - anything else is `Unavailable`: SIGKILL, as the system sends when it is
///   out of memory (this module's own deadline kill never gets here — `wait_for`
///   reports that as `TimedOut` before any run is judged), every other signal,
///   [`EXIT_USAGE`], any other exit code. Nothing was learned about the part.
fn classify(status: ExitStatus, stdout: Result<String, String>) -> Result<f64, BrepFailure> {
    if status.success() {
        return volume_printed(stdout);
    }
    if status.code() == Some(EXIT_REFUSED) {
        return Err(refusal_printed(stdout));
    }
    Err(ended_without_answer(&status))
}

/// The answer of a worker that exited cleanly. It exits cleanly only after
/// printing one, so a run that did not has told nothing about the part.
fn volume_printed(stdout: Result<String, String>) -> Result<f64, BrepFailure> {
    let printed = stdout.map_err(BrepFailure::Unavailable)?;
    last_answer(&printed, volume_of).ok_or_else(|| {
        BrepFailure::Unavailable(format!(
            "the B-rep worker exited cleanly but printed no volume (its last line: {}) — \
             nothing is known about this part",
            echoed(&printed)
        ))
    })
}

/// What a worker that exited with [`EXIT_REFUSED`] has to say: its own reason,
/// when it gave one, and when it did not, that something else ended it.
fn refusal_printed(stdout: Result<String, String>) -> BrepFailure {
    let printed = match stdout {
        Ok(printed) => printed,
        // Whether it refused or crashed, and why, was in what could not be read.
        Err(unread) => return BrepFailure::Unavailable(unread),
    };
    match last_answer(&printed, reason_of) {
        Some(reason) => BrepFailure::Refused(reason),
        None => BrepFailure::Crashed(format!(
            "the B-rep worker exited with code {EXIT_REFUSED} without giving a reason — a \
             refusal always says why, and this is what the Windows C runtime ends a process \
             with when it aborts, so the kernel most likely crashed"
        )),
    }
}

/// A worker that neither answered nor refused, judged by how it ended: a
/// crash, or something that says nothing about the part.
fn ended_without_answer(status: &ExitStatus) -> BrepFailure {
    if status.code() == Some(EXIT_PANIC) {
        return BrepFailure::Crashed(format!(
            "the B-rep worker panicked while building this part (exit code {EXIT_PANIC}) — \
             that is a defect in this daemon's own build code rather than in the part, and the \
             daemon's log has the panic; this part runs into it every time"
        ));
    }
    if is_kernel_crash(status) {
        // The part is named as the innocent party on purpose: a crash here is a
        // defect in OCCT, and the reader's next move is to change the part anyway.
        return BrepFailure::Crashed(format!(
            "the B-rep kernel crashed while building this part ({}) — that is a defect in OCCT \
             rather than in the part, and the usual way round it is to drop the fillet or \
             chamfer, or to apply it to one solid instead of to a union of several",
            died_how(status)
        ));
    }
    BrepFailure::Unavailable(unexplained_end(status))
}

/// Why a worker that ended some way other than answering, refusing or crashing
/// tells nothing about the part.
fn unexplained_end(status: &ExitStatus) -> String {
    if signal_of(status) == Some(SIGKILL) {
        return "the B-rep worker was killed from outside (SIGKILL), most likely because the \
                system ran out of memory — that says nothing about this part"
            .to_string();
    }
    if status.code() == Some(EXIT_USAGE) {
        return format!(
            "the B-rep worker did not understand `build-geometry` (exit code {EXIT_USAGE}): the \
             binary it was started from does not provide that command, which says nothing about \
             this part"
        );
    }
    format!(
        "the B-rep worker ended without answering ({}), and not in any way OCCT crashes: it \
         was stopped from outside — Ctrl+C, a logout or shutdown, \"Quit\" in Activity Monitor \
         — or it is not this daemon's worker. That says nothing about this part",
        died_how(status)
    )
}

/// One run of the worker, over: how it ended and what it printed.
struct Finished {
    status: ExitStatus,
    /// The worker's stdout as text, or why it could not be read.
    stdout: Result<String, String>,
}

/// Run the worker until it exits or `timeout` runs out, whichever comes
/// first. Either way it is dead and reaped when this returns.
fn run_child(
    exe: &Path,
    tree_path: &Path,
    out: &Path,
    timeout: std::time::Duration,
) -> Result<Finished, BrepFailure> {
    let mut child = Command::new(exe)
        .arg("build-geometry")
        .arg("--tree")
        .arg(tree_path)
        .arg("--output")
        .arg(out)
        // `spawn` hands the worker the daemon's stdin unless told otherwise,
        // and a worker that read it would wait on whoever started the daemon.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // The daemon's own stderr, so that a panic in the worker, or what
        // OCCT says before it falls over, lands in the daemon's log.
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            BrepFailure::Unavailable(format!(
                "cannot run the B-rep worker {}: {e}",
                exe.display()
            ))
        })?;

    let reader = match drain(&mut child) {
        Ok(reader) => reader,
        Err(why) => {
            kill_and_reap(&mut child);
            return Err(BrepFailure::Unavailable(why));
        }
    };
    let waited = wait_for(&mut child, timeout);
    // The worker has exited or been killed, so its end of the pipe is closed
    // and the reader is at the end of the stream: this does not wait. (Unless
    // the worker left a child of its own holding the pipe open, which the
    // real one, an OCCT call in a single process, does not.)
    let stdout = join_output(reader);
    Ok(Finished {
        status: waited?,
        stdout,
    })
}

/// Start reading the worker's stdout on a thread of its own.
///
/// From the moment the worker starts, not once it has exited: OCCT's default
/// messenger writes to `std::cout` and a pipe holds 64 KiB at most. A worker
/// that nobody reads from blocks in `write`, and the parent would then wait
/// out the whole deadline for a part that had in fact built.
fn drain(child: &mut Child) -> Result<JoinHandle<io::Result<Vec<u8>>>, String> {
    let mut pipe = child
        .stdout
        .take()
        .ok_or_else(|| "cannot read the B-rep worker's output: it has no stdout".to_string())?;
    std::thread::Builder::new()
        .name("brep-worker-out".to_string())
        .spawn(move || {
            let mut printed = Vec::new();
            pipe.read_to_end(&mut printed).map(|_| printed)
        })
        .map_err(|e| format!("cannot read the B-rep worker's output: {e}"))
}

/// What the reader thread collected, as text. Decoded leniently: one byte
/// that is not UTF-8 among OCCT's chatter must not cost the answer, which is
/// the last line.
fn join_output(reader: JoinHandle<io::Result<Vec<u8>>>) -> Result<String, String> {
    let unread = match reader.join() {
        Ok(Ok(bytes)) => return Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Ok(Err(e)) => format!("cannot read what the B-rep worker printed: {e}"),
        Err(_) => "the thread reading the B-rep worker's output panicked".to_string(),
    };
    // Logged here as well as returned: the caller only looks at the output
    // when the worker exited cleanly or refused, and a crash must not hide
    // that the pipe went wrong too.
    tracing::warn!(%unread, "the B-rep worker's output was lost");
    Err(unread)
}

/// Poll for the worker's exit, killing it when `timeout` passes first.
fn wait_for(child: &mut Child, timeout: std::time::Duration) -> Result<ExitStatus, BrepFailure> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                kill_and_reap(child);
                return Err(BrepFailure::TimedOut(format!(
                    "the B-rep kernel did not finish this part within {:.1} s — OCCT is stuck \
                     on it; simplify the fillets or chamfers and try again",
                    timeout.as_secs_f32()
                )));
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(e) => {
                kill_and_reap(child);
                return Err(BrepFailure::Unavailable(format!(
                    "cannot wait for the B-rep worker: {e}"
                )));
            }
        }
    }
}

/// Stop a worker nobody wants any more and collect it, so that it does not
/// outlive the call as a zombie.
fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The answer in what a worker printed: the last line `read` takes one from.
///
/// From the end, and skipping every line that is not one. OCCT and libslic3r
/// write to the same pipe, and what they flush late lands after the answer as
/// well as before it.
fn last_answer<T>(printed: &str, read: impl Fn(&serde_json::Value) -> Option<T>) -> Option<T> {
    printed.lines().rev().find_map(|line| {
        let parsed = serde_json::from_str::<serde_json::Value>(line).ok()?;
        read(&parsed)
    })
}

/// The volume of a line that is `{"volume_mm3": <number>}`.
fn volume_of(answer: &serde_json::Value) -> Option<f64> {
    answer.get("volume_mm3")?.as_f64()
}

/// The reason of a line that is `{"error": "<reason>"}`.
fn reason_of(answer: &serde_json::Value) -> Option<String> {
    answer.get("error")?.as_str().map(str::to_owned)
}

/// How much of a worker's last line a message repeats.
const ECHOED_CHARS: usize = 200;

/// The last line a worker printed, cut short: what a message has to show when
/// there was no answer among it.
fn echoed(printed: &str) -> String {
    let last = printed
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("nothing");
    let mut shown: String = last.chars().take(ECHOED_CHARS).collect();
    if last.chars().count() > ECHOED_CHARS {
        shown.push('…');
    }
    shown
}

/// Whether the worker died of a crash of the kernel: a crash signal, or on
/// Windows an exception. A panic is told apart before this, for its message.
fn is_kernel_crash(status: &ExitStatus) -> bool {
    signal_of(status).is_some_and(|signal| CRASH_SIGNALS.contains(&signal))
        || status.code().is_some_and(is_exception_code)
}

/// Whether a Windows exit code is an exception that ended the worker: an
/// NTSTATUS in the error range, bar the few a console or the loader ends a
/// process with. No exit code is one anywhere else.
fn is_exception_code(code: i32) -> bool {
    // The exit code is the status's 32 bits, read as signed.
    let status = code as u32;
    cfg!(windows) && status >= NTSTATUS_ERROR_MIN && !NOT_CRASH_STATUSES.contains(&status)
}

/// The signal that ended the worker.
#[cfg(unix)]
fn signal_of(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

/// Off unix there are no signals, so nothing is taken for one.
#[cfg(not(unix))]
fn signal_of(_status: &ExitStatus) -> Option<i32> {
    None
}

/// How the worker's end reads in a message.
fn died_how(status: &ExitStatus) -> String {
    match (signal_of(status), status.code()) {
        (Some(signal), _) => format!("killed by signal {signal}"),
        (None, Some(code)) if is_exception_code(code) => format!("exception {:#010X}", code as u32),
        (None, Some(code)) => format!("exit code {code}"),
        (None, None) => "no exit code".to_string(),
    }
}

#[cfg(test)]
mod tests;
