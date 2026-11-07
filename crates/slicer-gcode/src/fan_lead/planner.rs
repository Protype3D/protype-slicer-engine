//! The first pass: a streaming state machine over the G-code lines.
//!
//! Memory is the lead window, not the file. Candidates are the moves since
//! the last boundary the lead may not cross — the first layer change, a tool
//! change, the previous zone's exit; once a move started `max_lead_s` before
//! the newest one, every older move is further back than any lead can ask for
//! and is dropped.

use std::collections::VecDeque;

use super::line::{classify, Line, Marker, FULL_FAN};
use super::timeline::Timeline;
use super::{FanLeadParams, FanLeadPlan, MAX_FAN_LEAD_S};
use crate::error::GcodeError;

/// Rounding slack when comparing accumulated seconds.
const EPSILON_S: f64 = 1e-9;

/// A place the entry command may be moved to: before this timed line.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    line: usize,
    start_s: f64,
    /// Part fan speed in effect just before this line.
    speed_before: u16,
}

/// How far back the current window reaches.
#[derive(Debug, Clone, Copy)]
enum Floor {
    /// Before the first `;LAYER_CHANGE`: no lead at all.
    None,
    /// The first `;LAYER_CHANGE` or a tool change: nothing earlier may be used.
    Plain,
    /// The previous zone's exit. `insert_at` is the line after its marker,
    /// `exit_command` the exit's own fan command if it has one.
    AfterExit {
        insert_at: usize,
        exit_command: Option<usize>,
    },
}

#[derive(Debug)]
struct PendingMarker {
    line: usize,
    marker: Marker,
    start_s: f64,
    speed_before: u16,
}

#[derive(Debug)]
struct OwnedCommand {
    line: usize,
    speed: u16,
    raw: Vec<u8>,
}

pub(super) struct Planner {
    params: FanLeadParams,
    max_lead_s: f64,
    timeline: Timeline,
    line: usize,
    now_s: f64,
    speed: u16,
    body_started: bool,
    open_zone: Option<usize>,
    floor: Floor,
    pending: Option<PendingMarker>,
    window: VecDeque<Candidate>,
    automatic: VecDeque<usize>,
    plan: FanLeadPlan,
    first_error: Option<GcodeError>,
}

impl Planner {
    pub(super) fn new(params: FanLeadParams) -> Self {
        Self {
            params,
            max_lead_s: window_span_s(params),
            timeline: Timeline::default(),
            line: 0,
            now_s: 0.0,
            speed: 0,
            body_started: false,
            open_zone: None,
            floor: Floor::None,
            pending: None,
            window: VecDeque::new(),
            automatic: VecDeque::new(),
            plan: FanLeadPlan::default(),
            first_error: None,
        }
    }

    pub(super) fn feed(&mut self, raw: &[u8]) {
        let text = String::from_utf8_lossy(raw);
        let line = match classify(&text) {
            Ok(line) => line,
            Err(word) => {
                self.record(GcodeError::UnreadableWord {
                    line: self.line + 1,
                    word,
                });
                Line::Other
            }
        };

        if let Some(pending) = self.pending.take() {
            if let Line::PartFan(speed) = line {
                let command = OwnedCommand {
                    line: self.line,
                    speed,
                    raw: with_newline(raw),
                };
                self.settle(pending, Some(&command));
                self.speed = speed;
                self.line += 1;
                return;
            }
            self.settle(pending, None);
        }

        match line {
            Line::Marker(marker) => {
                self.plan.markers += 1;
                self.pending = Some(PendingMarker {
                    line: self.line,
                    marker,
                    start_s: self.now_s,
                    speed_before: self.speed,
                });
            }
            Line::BadMarker => {
                self.plan.markers += 1;
                self.record(GcodeError::PaintFanMarkerMalformed {
                    line: self.line + 1,
                    text: text.trim().to_string(),
                });
            }
            Line::PartFan(speed) => {
                if self.collecting() {
                    self.automatic.push_back(self.line);
                    self.prune_automatic();
                }
                self.speed = speed;
            }
            // A tool change in the start block is before the body anyway.
            Line::ToolChange => {
                if self.body_started {
                    self.cut(Floor::Plain);
                }
            }
            Line::LayerChange => {
                if !self.body_started {
                    self.body_started = true;
                    self.cut(Floor::Plain);
                }
            }
            Line::Motion(command) => {
                let seconds = self.timeline.advance(&command);
                if seconds > 0.0 && self.collecting() {
                    self.window.push_back(Candidate {
                        line: self.line,
                        start_s: self.now_s,
                        speed_before: self.speed,
                    });
                    self.prune();
                }
                self.now_s += seconds;
            }
            Line::Other => {}
        }
        self.line += 1;
    }

    pub(super) fn finish(mut self) -> Result<FanLeadPlan, GcodeError> {
        if let Some(pending) = self.pending.take() {
            self.settle(pending, None);
        }
        if let Some(entry) = self.open_zone {
            self.record(GcodeError::PaintFanUnbalanced {
                line: entry + 1,
                problem: "is never closed",
            });
        }
        if self.plan.markers == 0 {
            return Ok(FanLeadPlan::default());
        }
        self.params.validate()?;
        if !self.body_started {
            return Err(GcodeError::NoLayerMarkers);
        }
        match self.first_error {
            Some(error) => Err(error),
            None => Ok(self.plan),
        }
    }

    fn collecting(&self) -> bool {
        self.open_zone.is_none() && !matches!(self.floor, Floor::None)
    }

    fn record(&mut self, error: GcodeError) {
        self.first_error.get_or_insert(error);
    }

    fn cut(&mut self, floor: Floor) {
        self.floor = floor;
        self.window.clear();
        self.automatic.clear();
    }

    /// Drop moves that no lead can reach: once the second-oldest candidate
    /// is `max_lead_s` behind the newest, it satisfies any request, so the
    /// oldest is never chosen.
    fn prune(&mut self) {
        while self.window.len() >= 2 && self.window[1].start_s <= self.now_s - self.max_lead_s {
            self.window.pop_front();
            // The new oldest candidate is a whole `max_lead_s` back, so every
            // lead from here on finds one and none can reach the boundary
            // again: the previous zone's exit stops being a place to merge
            // into, and holding it would keep its automatic commands alive
            // for the rest of the file.
            self.floor = Floor::Plain;
        }
        self.prune_automatic();
    }

    /// The earliest line a command could be put before, if there is one: the
    /// previous zone's exit when the window rests on it — a lead may be
    /// merged into it, ahead of every candidate — otherwise the oldest move
    /// still in the window.
    fn earliest_insert(&self) -> Option<usize> {
        match self.floor {
            Floor::AfterExit { insert_at, .. } => Some(insert_at),
            _ => self.window.front().map(|candidate| candidate.line),
        }
    }

    /// Drop automatic fan commands no lead can override any more: the ones
    /// before that earliest line. With no such line the whole queue is dead,
    /// which is what keeps a stretch of fan commands with no move between
    /// them from growing it without bound.
    fn prune_automatic(&mut self) {
        let Some(earliest) = self.earliest_insert() else {
            self.automatic.clear();
            return;
        };
        while self.automatic.front().is_some_and(|line| *line < earliest) {
            self.automatic.pop_front();
        }
    }

    fn settle(&mut self, pending: PendingMarker, command: Option<&OwnedCommand>) {
        match pending.marker {
            Marker::Exit => self.leave_zone(&pending, command),
            Marker::Entry => self.enter_zone(&pending, command),
        }
    }

    fn leave_zone(&mut self, pending: &PendingMarker, command: Option<&OwnedCommand>) {
        if self.open_zone.take().is_none() {
            self.record(GcodeError::PaintFanUnbalanced {
                line: pending.line + 1,
                problem: "is closed without being opened",
            });
        }
        if self.body_started {
            self.cut(Floor::AfterExit {
                insert_at: pending.line + 1,
                exit_command: command.map(|c| c.line),
            });
        }
    }

    fn enter_zone(&mut self, pending: &PendingMarker, command: Option<&OwnedCommand>) {
        if self.open_zone.replace(pending.line).is_some() {
            self.record(GcodeError::PaintFanUnbalanced {
                line: pending.line + 1,
                problem: "is opened inside another zone",
            });
        }
        if let Some(command) = command {
            self.lead(pending, command);
        }
        self.window.clear();
        self.automatic.clear();
    }

    /// Seconds the fan needs to go from `from` to `to`: the spin-up or
    /// spin-down time covers a full swing, 0 → `FULL_FAN` or back.
    fn need_s(&self, from: u16, to: u16) -> f64 {
        let per_full_swing = if to > from {
            self.params.spinup_s
        } else {
            self.params.spindown_s
        };
        per_full_swing * f64::from(from.abs_diff(to)) / f64::from(FULL_FAN)
    }

    fn lead(&mut self, pending: &PendingMarker, command: &OwnedCommand) {
        if matches!(self.floor, Floor::None)
            || self.need_s(pending.speed_before, command.speed) <= EPSILON_S
        {
            return;
        }
        let reached = self.window.iter().rev().copied().find(|candidate| {
            pending.start_s - candidate.start_s + EPSILON_S
                >= self.need_s(candidate.speed_before, command.speed)
        });
        match (reached, self.floor) {
            (Some(candidate), _) => self.move_command(candidate.line, command),
            // The previous zone is too close: its exit command gives way.
            (
                None,
                Floor::AfterExit {
                    insert_at,
                    exit_command,
                },
            ) => {
                self.move_command(insert_at, command);
                // `merged` counts commands that gave way, not entries: an
                // exit that changed nothing has none to give.
                if let Some(exit) = exit_command {
                    self.plan.edits.entry(exit).or_default().remove = true;
                    self.plan.merged += 1;
                }
            }
            // Not enough time since the boundary: as far back as it allows.
            (None, _) => {
                if let Some(first) = self.window.front().copied() {
                    self.move_command(first.line, command);
                }
            }
        }
    }

    /// Put `command` before line `before`, drop it from its own place and
    /// drop every automatic fan command it now overrides.
    fn move_command(&mut self, before: usize, command: &OwnedCommand) {
        self.plan.edits.entry(before).or_default().insert_before = Some(command.raw.clone());
        self.plan.edits.entry(command.line).or_default().remove = true;
        for line in self.automatic.iter().filter(|line| **line >= before) {
            self.plan.edits.entry(*line).or_default().remove = true;
        }
        self.plan.shifted += 1;
    }
}

/// How far back the window must reach: the longer of the two times, bounded
/// to what `finish` accepts. `finish` refuses an out-of-range time only at
/// the end of the file, and an absurd one would keep every move until then.
/// NaN (both times NaN) asks for nothing — `f64::clamp` would keep the NaN,
/// and a NaN bound silently stops the pruning.
fn window_span_s(params: FanLeadParams) -> f64 {
    let asked = params.spinup_s.max(params.spindown_s);
    if asked.is_nan() {
        return 0.0;
    }
    asked.clamp(0.0, MAX_FAN_LEAD_S)
}

fn with_newline(raw: &[u8]) -> Vec<u8> {
    let mut owned = raw.to_vec();
    if !owned.ends_with(b"\n") {
        owned.push(b'\n');
    }
    owned
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A time outside 0..=`MAX_FAN_LEAD_S` is refused by `finish`, and
    /// `finish` only runs at the end of the file. Until then the window has
    /// to stay a window: an absurd time must not turn it into the file.
    #[test]
    fn an_absurd_lead_time_does_not_keep_the_whole_file_in_the_window() {
        let mut planner = Planner::new(FanLeadParams {
            spinup_s: 1e6,
            spindown_s: 1e6,
        });
        assert_eq!(planner.max_lead_s, MAX_FAN_LEAD_S);

        planner.feed(b";LAYER_CHANGE\n");
        planner.feed(b"G1 X0 Y0 F600\n");
        for x in 1..=20_000 {
            planner.feed(format!("G1 X{x} E.1\n").as_bytes());
        }
        // 1 mm at 10 mm/s is 0.1 s, so 10 s of lead is 100 moves; one older
        // candidate is kept as the floor of the window.
        assert!(planner.window.len() <= 102, "{}", planner.window.len());
    }

    /// `f64::clamp` keeps a NaN, and a NaN bound compares false against every
    /// candidate — the window would never be pruned again.
    #[test]
    fn two_nan_times_ask_for_no_window_at_all() {
        let planner = Planner::new(FanLeadParams {
            spinup_s: f64::NAN,
            spindown_s: f64::NAN,
        });
        assert_eq!(planner.max_lead_s, 0.0);
    }

    /// The queue of automatic fan commands is pruned when a move arrives.
    /// A stretch of fan commands with no move between them must not pile up
    /// in it: with no move in the window there is no place to put a lead at,
    /// so not one of them could ever be overridden.
    #[test]
    fn fan_commands_without_any_move_do_not_pile_up() {
        let mut planner = Planner::new(FanLeadParams::default());
        planner.feed(b";LAYER_CHANGE\n");
        for speed in 0..10_000u16 {
            planner.feed(format!("M106 S{}\n", speed % 256).as_bytes());
        }
        assert!(planner.automatic.is_empty(), "{}", planner.automatic.len());
    }

    /// Once the window has been pruned the boundary is out of reach: every
    /// lead finds a candidate, so nothing will be merged into that exit any
    /// more and the automatic commands after it are dead too. They must not
    /// pile up for the rest of the file.
    #[test]
    fn automatic_commands_after_a_distant_exit_do_not_pile_up() {
        let mut planner = Planner::new(FanLeadParams::default());
        for line in [
            ";LAYER_CHANGE\n",
            "G1 X0 Y0 F600\n",
            "; PAINT_FAN S100\n",
            "M106 S255\n",
            "G1 X1 E.1\n",
            "; PAINT_FAN END\n",
            "M106 S0\n",
        ] {
            planner.feed(line.as_bytes());
        }
        // 1 mm at 10 mm/s, so the 2 s window is ~20 moves and the same
        // number of fan commands between them.
        for x in 2..=2_000 {
            planner.feed(format!("G1 X{x} E.1\n").as_bytes());
            planner.feed(b"M106 S102\n");
        }
        assert!(planner.automatic.len() <= 32, "{}", planner.automatic.len());
    }
}
