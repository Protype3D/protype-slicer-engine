//! The `watch` subcommand: poll, journal, decide.
//!
//! The loop is all that lives here. Everything it decides comes from
//! `slicer_monitor::Watch`, which has no I/O and is tested without a
//! printer — including the cases that need a machine to have been stuck
//! for twenty minutes.

use anyhow::Result;
use slicer_monitor::moonraker::Moonraker;
use slicer_monitor::{journal::describe, Journal, Outcome, Watch, WatchConfig};

pub struct Options {
    pub host: String,
    pub logdir: String,
    pub interval_s: f64,
    pub stall_after_s: f64,
    pub start_grace_s: f64,
    pub until_layer: Option<u32>,
    pub once: bool,
}

/// How many console lines to ask for per poll. The ring buffer is the
/// only place a vendor error appears and it overwrites within minutes, so
/// this is deliberately larger than one interval's worth of chatter.
const CONSOLE_LINES: u32 = 200;

pub async fn run(options: Options) -> Result<i32> {
    // Bound every poll. A machine that accepts the connection and then
    // goes quiet — wedged host, blackholed Wi-Fi — would otherwise hold
    // this loop open forever: no further turn, a journal that stops
    // mid-print, and `Outcome::Unreachable` never reached.
    let machine = Moonraker::new(&options.host)
        .with_timeout(slicer_monitor::moonraker::poll_timeout(options.interval_s));

    // One snapshot before anything else, so an unreachable machine fails
    // now rather than after a journal has been opened for it.
    let first = match machine.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(e) => {
            eprintln!("cannot reach {}: {e}", machine.api_base());
            return Ok(Outcome::Unreachable {
                detail: e.to_string(),
            }
            .exit_code());
        }
    };

    let job = if first.filename.is_empty() {
        "idle".to_string()
    } else {
        first.filename.clone()
    };
    let stamp = stamp();
    let mut journal = Journal::create(std::path::Path::new(&options.logdir), &job, &stamp)?;
    let log_path = journal.log_path().display().to_string();
    say(&mut journal, &format!("journal: {log_path}"));
    say(&mut journal, &format!("machine: {}", machine.api_base()));
    say(
        &mut journal,
        &format!(
            "state on arrival: {} ({})",
            first.state,
            if first.filename.is_empty() {
                "no file".into()
            } else {
                first.filename.clone()
            }
        ),
    );

    let mut watch = Watch::new(WatchConfig {
        stall_after_s: options.stall_after_s,
        start_grace_s: options.start_grace_s,
        until_layer: options.until_layer,
    });

    // Zero, not "a minute ago": the first poll should take everything the
    // ring buffer still holds. That buffer is the only place a vendor
    // error ever appears and it overwrites within minutes, so whatever is
    // in it when the watch starts is exactly what would otherwise be lost.
    let mut seen_after = 0.0;
    let began = std::time::Instant::now();

    loop {
        let at = began.elapsed().as_secs_f64();
        let snapshot = match machine.snapshot().await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                let events = watch.unreachable(e.to_string());
                journal.record(at, &Default::default(), &[], &events)?;
                for event in &events {
                    println!("{}", describe(event));
                }
                break;
            }
        };
        let (console, newest) = machine
            .console_since(seen_after, CONSOLE_LINES)
            .await
            .unwrap_or_else(|_| (Vec::new(), seen_after));
        seen_after = newest;

        let events = watch.observe(at, &snapshot, &console);
        journal.record(at, &snapshot, &console, &events)?;
        for event in &events {
            println!("{}", describe(event));
        }

        if watch.is_finished() || options.once {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs_f64(options.interval_s)).await;
    }

    let outcome = watch.outcome().cloned();
    say(&mut journal, &format!("journal closed: {log_path}"));
    println!("journal: {}", journal.jsonl_path().display());
    Ok(outcome.map(|o| o.exit_code()).unwrap_or(0))
}

fn say(journal: &mut Journal, line: &str) {
    println!("{line}");
    let _ = journal.say(line);
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// A sortable stamp for the journal filename. Seconds since the epoch
/// rather than a formatted date, to avoid a time-formatting dependency
/// for something only a filename ever reads.
fn stamp() -> String {
    format!("{:.0}", unix_now())
}
