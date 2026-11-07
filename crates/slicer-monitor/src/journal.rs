//! Two files, written as it goes.
//!
//! `.jsonl` — one record per poll, for a machine to read back afterwards.
//! `.log` — only the events, for a person reading now.
//!
//! Both are flushed on every write. The whole point is to survive the
//! moment the print dies, and a buffer that has not reached the disk when
//! the process is killed records nothing.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::console::ConsoleLine;
use crate::snapshot::Snapshot;
use crate::watch::Event;

pub struct Journal {
    base: PathBuf,
    records: std::fs::File,
    readable: std::fs::File,
}

impl Journal {
    /// `<dir>/<job>-<stamp>.{jsonl,log}`. `stamp` is supplied rather than
    /// taken from the clock so a caller can make the name reproducible.
    pub fn create(dir: &Path, job: &str, stamp: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let base = dir.join(format!("{}-{stamp}", sanitize(job)));
        Ok(Self {
            records: open(&base.with_extension("jsonl"))?,
            readable: open(&base.with_extension("log"))?,
            base,
        })
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    pub fn jsonl_path(&self) -> PathBuf {
        self.base.with_extension("jsonl")
    }

    pub fn log_path(&self) -> PathBuf {
        self.base.with_extension("log")
    }

    /// One poll: the full snapshot, any new console lines, and whatever
    /// the watch made of them.
    pub fn record(
        &mut self,
        at: f64,
        snapshot: &Snapshot,
        console: &[ConsoleLine],
        events: &[Event],
    ) -> std::io::Result<()> {
        let mut entry = serde_json::json!({
            "at": at,
            "snapshot": snapshot,
        });
        if !console.is_empty() {
            entry["console"] = serde_json::to_value(console).unwrap_or_default();
        }
        if !events.is_empty() {
            entry["events"] = serde_json::to_value(events).unwrap_or_default();
        }
        writeln!(self.records, "{entry}")?;
        self.records.flush()?;

        for line in events.iter().map(describe) {
            writeln!(self.readable, "{line}")?;
        }
        self.readable.flush()
    }

    /// A line that is not tied to a poll.
    pub fn say(&mut self, text: &str) -> std::io::Result<()> {
        writeln!(self.readable, "{text}")?;
        self.readable.flush()
    }
}

fn open(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// Render an event for a person.
pub fn describe(event: &Event) -> String {
    match event {
        Event::StateChanged { from, to, message } => {
            let from = from.as_deref().unwrap_or("(start)");
            if message.is_empty() {
                format!("state: {from} -> {to}")
            } else {
                format!("state: {from} -> {to}   message: {message}")
            }
        }
        Event::LayerChanged {
            layer,
            total,
            progress_pct,
        } => match total {
            Some(total) => format!("layer {layer}/{total}   {progress_pct:.1}%"),
            None => format!("layer {layer}   {progress_pct:.1}%"),
        },
        Event::Console { line, vendor_code } => {
            let mark = if line.is_error() { "ERROR" } else { "console" };
            let mut text = format!("[{mark}] {}", line.message);
            if let Some(code) = vendor_code {
                text.push_str(&format!("\n         └─ {code}"));
            }
            text
        }
        Event::Note { text } => format!("note: {text}"),
        Event::Finished { outcome } => format!("FINISHED: {}", describe_outcome(outcome)),
    }
}

fn describe_outcome(outcome: &crate::watch::Outcome) -> String {
    use crate::watch::Outcome::*;
    match outcome {
        Completed => "the print finished".into(),
        Failed { message } if message.is_empty() => {
            "the print failed; the machine gave no message, so the reason is in the console \
             lines above"
                .into()
        }
        Failed { message } => format!("the print failed: {message}"),
        Cancelled => "the print was cancelled".into(),
        Paused => "the print paused".into(),
        Stalled { idle_s, at_layer } => format!(
            "the layer counter has not moved for {:.0} s, stuck at layer {at_layer}",
            idle_s
        ),
        LayerCounterNeverMoved { waited_s } => format!(
            "the layer counter never left 0 after {:.0} s, though the start block announced a \
             total — this G-code almost certainly lacks SET_PRINT_STATS_INFO CURRENT_LAYER and \
             cannot be watched. Run the layer pass over it",
            waited_s
        ),
        ReachedLayer { layer } => format!("reached layer {layer}"),
        Unreachable { detail } => format!("the machine stopped answering: {detail}"),
    }
}

/// Keep a job name usable as a filename.
fn sanitize(job: &str) -> String {
    let stem = job.rsplit('/').next().unwrap_or(job);
    let stem = stem.strip_suffix(".gcode").unwrap_or(stem);
    let cleaned: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "job".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::Outcome;

    #[test]
    fn a_job_path_becomes_a_filename() {
        assert_eq!(
            sanitize("/home/pi/CUP_stand holder.gcode"),
            "CUP_stand_holder"
        );
        assert_eq!(sanitize(""), "job");
    }

    #[test]
    fn a_vendor_error_is_rendered_with_its_meaning_underneath() {
        let event = Event::Console {
            line: ConsoleLine {
                time: 1.0,
                kind: "response".into(),
                message: "!! QDE_004_006 Extruder loading failure".into(),
            },
            vendor_code: crate::console::find_vendor_code("!! QDE_004_006"),
        };
        let text = describe(&event);
        assert!(text.contains("[ERROR]"));
        assert!(text.contains("could not load filament"));
    }

    /// The two failures need different words because they need different
    /// actions: one sends you to the machine, the other to the file.
    #[test]
    fn a_stall_and_a_missing_counter_read_differently() {
        let stalled = describe_outcome(&Outcome::Stalled {
            idle_s: 1000.0,
            at_layer: 51,
        });
        let never = describe_outcome(&Outcome::LayerCounterNeverMoved { waited_s: 1600.0 });
        assert!(stalled.contains("stuck at layer 51"));
        assert!(never.contains("CURRENT_LAYER"));
        assert!(never.contains("Run the layer pass"));
    }

    #[test]
    fn a_failure_with_no_message_points_at_the_console() {
        let text = describe_outcome(&Outcome::Failed {
            message: String::new(),
        });
        assert!(text.contains("console"));
    }

    #[test]
    fn both_files_are_written_and_readable() {
        let dir = std::env::temp_dir().join(format!("journal-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut journal = Journal::create(&dir, "part.gcode", "stamp").expect("creates");

        let snapshot = Snapshot {
            state: "printing".into(),
            layer: Some(3),
            ..Default::default()
        };
        let events = vec![Event::LayerChanged {
            layer: 3,
            total: Some(10),
            progress_pct: 30.0,
        }];
        journal
            .record(1.0, &snapshot, &[], &events)
            .expect("records");

        let jsonl = std::fs::read_to_string(journal.jsonl_path()).unwrap();
        let log = std::fs::read_to_string(journal.log_path()).unwrap();
        assert!(jsonl.contains("\"at\":1.0"));
        assert!(jsonl.contains("\"layer\":3"));
        assert!(log.contains("layer 3/10"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
