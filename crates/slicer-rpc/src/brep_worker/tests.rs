//! What `classify` makes of every way a worker can end, and of what it printed.
//!
//! The exits and signals are real `ExitStatus` values built with `from_raw`, so no
//! worker runs here; `tests/brep_worker_child.rs` runs fake workers that exit
//! the same ways.

use super::*;

const VOLUME: &str = r#"{"volume_mm3": 6000.0}"#;
const REFUSAL: &str = r#"{"error":"no such shape"}"#;

/// A worker that exited by itself with `code`.
#[cfg(unix)]
fn exited(code: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(code << 8)
}

#[cfg(windows)]
fn exited(code: i32) -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    ExitStatus::from_raw(code as u32)
}

/// What a worker printed: some chatter, then `last_line`.
fn printed(last_line: &str) -> Result<String, String> {
    Ok(format!("some chatter\n{last_line}\n"))
}

/// What a worker printed, one of `lines` after the other.
fn printed_lines(lines: &[&str]) -> Result<String, String> {
    Ok(format!("{}\n", lines.join("\n")))
}

#[test]
fn a_clean_exit_with_a_volume_is_the_volume() {
    assert_eq!(classify(exited(0), printed(VOLUME)), Ok(6000.0));
}

#[test]
fn a_volume_is_found_among_the_output_that_comes_before_and_after_it() {
    // OCCT and libslic3r write to the same pipe, and what one of them
    // flushes late lands behind the answer: the answer is the last line
    // that is one, not the last line.
    let late = printed_lines(&["chatter", VOLUME, "late chatter", "more of it"]);
    assert_eq!(classify(exited(0), late), Ok(6000.0));
    let only = printed_lines(&[VOLUME]);
    assert_eq!(classify(exited(0), only), Ok(6000.0));
}

#[test]
fn a_clean_exit_without_a_volume_line_is_unavailable_not_a_refusal() {
    // Nothing was learned about the part: it may well have built, and the
    // next run may hear it say so.
    let without = [
        printed("not json"),
        printed(r#"{"volume_mm3": null}"#),
        printed(r#"{"volume": 1.0}"#),
        printed(REFUSAL),
        Ok(String::new()),
    ];
    for stdout in without {
        let why = classify(exited(0), stdout.clone()).unwrap_err();
        assert!(
            matches!(why, BrepFailure::Unavailable(_)),
            "{stdout:?}: {why:?}"
        );
        assert!(why.to_string().contains("volume"), "got: {why}");
        assert!(why.is_transient(), "{stdout:?}");
    }
}

#[test]
fn a_clean_exit_whose_output_was_lost_is_unavailable() {
    let why = classify(exited(0), Err("pipe".to_string())).unwrap_err();
    assert_eq!(why, BrepFailure::Unavailable("pipe".to_string()));
    assert!(why.is_transient(), "the part may well have built");
}

#[test]
fn a_refusal_keeps_the_workers_reason() {
    let why = classify(exited(EXIT_REFUSED), printed(REFUSAL)).unwrap_err();
    assert_eq!(why, BrepFailure::Refused("no such shape".to_string()));
    assert!(!why.is_transient());
}

#[test]
fn a_refusal_is_found_among_the_output_that_comes_before_and_after_it() {
    let late = printed_lines(&["chatter", REFUSAL, "late chatter", "more of it"]);
    let why = classify(exited(EXIT_REFUSED), late).unwrap_err();
    assert_eq!(why, BrepFailure::Refused("no such shape".to_string()));
}

#[test]
fn exit_code_3_without_a_reason_is_a_crash_not_a_refusal() {
    // The worker's own refusal always says why. MSVC's `abort()` ends a
    // process with this code and says nothing.
    let without = [
        printed("not json"),
        printed(VOLUME),
        printed(r#"{"error": 3}"#),
        Ok(String::new()),
    ];
    for stdout in without {
        let why = classify(exited(EXIT_REFUSED), stdout.clone()).unwrap_err();
        assert!(
            matches!(why, BrepFailure::Crashed(_)),
            "{stdout:?}: {why:?}"
        );
        assert!(
            why.to_string().contains("without giving a reason"),
            "got: {why}"
        );
        assert!(!why.is_transient(), "{stdout:?}");
    }
}

#[test]
fn a_refusal_whose_reason_could_not_be_read_is_unavailable() {
    // Whether the worker refused or crashed, and why, was in what could
    // not be read.
    let why = classify(exited(EXIT_REFUSED), Err("pipe".to_string())).unwrap_err();
    assert_eq!(why, BrepFailure::Unavailable("pipe".to_string()));
    assert!(why.is_transient());
}

#[test]
fn a_rust_panic_is_a_crash() {
    let why = classify(exited(EXIT_PANIC), printed("")).unwrap_err();
    assert!(matches!(why, BrepFailure::Crashed(_)), "got: {why:?}");
    assert!(why.to_string().contains("panicked"), "got: {why}");
    assert!(
        why.to_string().contains(&format!("exit code {EXIT_PANIC}")),
        "got: {why}"
    );
    assert!(
        !why.is_transient(),
        "this part panics the same way every time"
    );
}

/// The highest exit code a process can report: an exit status is one byte.
const LAST_EXIT_CODE: i32 = 255;

#[test]
fn any_other_exit_code_is_unavailable_not_a_crash() {
    // Nobody chose these: a binary that is not the worker, a wrapper that
    // gave up, something that stopped it from outside. None of it is the
    // part's doing.
    let codes = (1..=LAST_EXIT_CODE).filter(|code| *code != EXIT_REFUSED && *code != EXIT_PANIC);
    for code in codes {
        let why = classify(exited(code), printed("")).unwrap_err();
        assert!(
            matches!(why, BrepFailure::Unavailable(_)),
            "exit code {code}: {why:?}"
        );
        assert!(why.is_transient(), "exit code {code}");
    }
    let one = classify(exited(1), printed("")).unwrap_err();
    assert!(one.to_string().contains("exit code 1"), "got: {one}");
    assert!(
        !one.to_string().contains("defect in OCCT"),
        "an exit nobody chose is not OCCT's defect: {one}"
    );
}

#[test]
fn a_worker_that_answers_with_clap_s_usage_code_is_unavailable() {
    let why = classify(exited(EXIT_USAGE), printed("")).unwrap_err();
    assert!(matches!(why, BrepFailure::Unavailable(_)), "got: {why:?}");
    assert!(why.to_string().contains("build-geometry"), "got: {why}");
    assert!(
        why.is_transient(),
        "a binary without `build-geometry` is a setup problem, not a verdict on the part"
    );
}

#[cfg(unix)]
mod signals {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    // The same number on every unix, and not a crash.
    const SIGHUP: i32 = 1;
    const SIGINT: i32 = 2;
    const SIGQUIT: i32 = 3;
    const SIGTERM: i32 = 15;

    /// The signal numbers `kill -l` knows, which is all of them and more.
    const SIGNALS_TRIED: i32 = 64;

    /// The core-dump flag a status carries when the system wrote a core file.
    const CORE_DUMPED: i32 = 0x80;

    /// A worker that `signal` ended.
    fn killed_by(signal: i32) -> ExitStatus {
        ExitStatus::from_raw(signal)
    }

    #[test]
    fn a_worker_the_system_killed_is_unavailable_not_a_crash() {
        let why = classify(killed_by(SIGKILL), printed("")).unwrap_err();
        assert!(matches!(why, BrepFailure::Unavailable(_)), "got: {why:?}");
        assert!(
            why.to_string().contains("killed from outside"),
            "got: {why}"
        );
        assert!(
            why.is_transient(),
            "nothing was learned about the part: the system took the worker, not the part"
        );
    }

    #[test]
    fn a_worker_that_dies_of_a_signal_of_its_own_is_a_crash() {
        // What OCCT dies of: a null dereference, an abort, a bad
        // instruction, a bus error, an arithmetic fault, a trap. Never
        // SIGKILL. With a core file or without one.
        for signal in CRASH_SIGNALS {
            for status in [killed_by(signal), killed_by(signal | CORE_DUMPED)] {
                let why = classify(status, printed("")).unwrap_err();
                assert!(matches!(why, BrepFailure::Crashed(_)), "got: {why:?}");
                assert!(
                    why.to_string()
                        .contains(&format!("killed by signal {signal}")),
                    "got: {why}"
                );
                assert!(
                    !why.is_transient(),
                    "this part dies the same way every time"
                );
            }
        }
    }

    #[test]
    fn a_worker_stopped_from_outside_is_unavailable_not_a_crash() {
        // Ctrl+C on the daemon reaches the whole process group; logging
        // out or shutting down, and "Quit" on a hung worker in Activity
        // Monitor, send the others. The worker may be mid-build: whatever
        // it was doing says nothing about the part.
        for signal in [SIGHUP, SIGINT, SIGQUIT, SIGTERM] {
            let why = classify(killed_by(signal), printed("")).unwrap_err();
            assert!(matches!(why, BrepFailure::Unavailable(_)), "got: {why:?}");
            assert!(
                why.to_string()
                    .contains(&format!("killed by signal {signal}")),
                "got: {why}"
            );
            assert!(
                !why.to_string().contains("defect in OCCT"),
                "an outside stop is not OCCT's defect: {why}"
            );
            assert!(why.is_transient(), "signal {signal}");
        }
    }

    #[test]
    fn no_signal_but_the_six_crash_signals_is_a_crash() {
        let crash = [SIGILL, SIGTRAP, SIGABRT, SIGBUS, SIGFPE, SIGSEGV];
        for signal in (1..=SIGNALS_TRIED).filter(|signal| !crash.contains(signal)) {
            let why = classify(killed_by(signal), printed("")).unwrap_err();
            assert!(
                matches!(why, BrepFailure::Unavailable(_)),
                "signal {signal}: {why:?}"
            );
        }
    }

    #[test]
    fn a_signal_does_not_hide_behind_an_answer_in_the_output() {
        // A worker that printed an answer and was killed afterwards has
        // still not exited cleanly, and is judged by how it ended.
        let why = classify(killed_by(SIGTERM), printed(VOLUME)).unwrap_err();
        assert!(matches!(why, BrepFailure::Unavailable(_)), "got: {why:?}");
        let why = classify(killed_by(SIGSEGV), printed(VOLUME)).unwrap_err();
        assert!(matches!(why, BrepFailure::Crashed(_)), "got: {why:?}");
    }

    #[test]
    fn the_signal_numbers_are_the_ones_this_platform_gives_those_names() {
        // `kill -l <number>` is the shell's own table of this platform's
        // signals. It names a signal without sending one, which for these
        // would leave a core file or a crash report of `sh` behind. The
        // numbers differ between unix families (SIGBUS is 7 on Linux and
        // 10 on macOS), and a wrong one would silently turn a crash into
        // an outside stop or the other way round.
        let named = [
            ("ILL", SIGILL),
            ("TRAP", SIGTRAP),
            ("ABRT", SIGABRT),
            ("BUS", SIGBUS),
            ("FPE", SIGFPE),
            ("KILL", SIGKILL),
            ("SEGV", SIGSEGV),
        ];
        for (name, number) in named {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("kill -l {number}"))
                .output()
                .expect("sh must run");
            assert_eq!(
                String::from_utf8_lossy(&out.stdout).trim(),
                name,
                "signal {number} on this platform"
            );
        }
    }
}

#[cfg(windows)]
mod exceptions {
    use std::os::windows::process::ExitStatusExt;

    use super::*;

    const STATUS_ACCESS_VIOLATION: u32 = 0xC000_0005;
    const STATUS_STACK_OVERFLOW: u32 = 0xC000_00FD;
    const STATUS_STACK_BUFFER_OVERRUN: u32 = 0xC000_0409;

    fn raw(status: u32) -> ExitStatus {
        ExitStatus::from_raw(status)
    }

    #[test]
    fn a_windows_exception_is_a_crash() {
        for status in [
            STATUS_ACCESS_VIOLATION,
            STATUS_STACK_OVERFLOW,
            STATUS_STACK_BUFFER_OVERRUN,
            NTSTATUS_ERROR_MIN,
            u32::MAX,
        ] {
            let why = classify(raw(status), printed("")).unwrap_err();
            assert!(
                matches!(why, BrepFailure::Crashed(_)),
                "{status:#X}: {why:?}"
            );
            assert!(
                why.to_string()
                    .contains(&format!("exception {status:#010X}")),
                "got: {why}"
            );
            assert!(!why.is_transient(), "{status:#X}");
        }
    }

    #[test]
    fn what_the_console_and_the_loader_end_a_process_with_is_not_a_crash() {
        // In the error range, and still not the part's doing: Ctrl+C or a
        // closed console, and a worker that could not even start.
        for status in NOT_CRASH_STATUSES {
            let why = classify(raw(status), printed("")).unwrap_err();
            assert!(
                matches!(why, BrepFailure::Unavailable(_)),
                "{status:#X}: {why:?}"
            );
            assert!(why.is_transient(), "{status:#X}");
        }
    }

    #[test]
    fn a_status_below_the_error_range_is_not_a_crash() {
        // Warnings and informational statuses are not what a crash leaves.
        for status in [NTSTATUS_ERROR_MIN - 1, 0x8000_0003, 0x4001_0005] {
            let why = classify(raw(status), printed("")).unwrap_err();
            assert!(
                matches!(why, BrepFailure::Unavailable(_)),
                "{status:#X}: {why:?}"
            );
        }
    }
}
