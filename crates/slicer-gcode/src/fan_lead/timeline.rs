//! How long a G-code line takes, `FanMover`'s way: length over feed rate for
//! moves, the stated time for dwells, nothing for everything else.

use super::line::Command;

/// Starts at the origin, absolute, with no feed rate known.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Timeline {
    position: [f64; 3],
    relative: bool,
    feed_mm_s: f64,
}

impl Timeline {
    /// Seconds `command` takes; updates position, mode and feed rate.
    pub(super) fn advance(&mut self, command: &Command) -> f64 {
        match *command {
            Command::Move {
                x,
                y,
                z,
                feed_mm_min,
            } => {
                if let Some(feed) = feed_mm_min.filter(|f| *f > 0.0) {
                    self.feed_mm_s = feed / 60.0;
                }
                let target = self.target([x, y, z]);
                let length = target
                    .iter()
                    .zip(self.position)
                    .map(|(to, from)| (to - from).powi(2))
                    .sum::<f64>()
                    .sqrt();
                self.position = target;
                // No feed rate yet (start block): count nothing rather than
                // divide by zero — an undercount only lengthens the lead.
                if self.feed_mm_s > 0.0 {
                    length / self.feed_mm_s
                } else {
                    0.0
                }
            }
            Command::Dwell { seconds } => seconds,
            Command::Absolute => {
                self.relative = false;
                0.0
            }
            Command::Relative => {
                self.relative = true;
                0.0
            }
            Command::SetPosition { x, y, z } => {
                for (axis, value) in [x, y, z].into_iter().enumerate() {
                    if let Some(value) = value {
                        self.position[axis] = value;
                    }
                }
                0.0
            }
        }
    }

    fn target(&self, words: [Option<f64>; 3]) -> [f64; 3] {
        let mut target = self.position;
        for (axis, value) in words.into_iter().enumerate() {
            if let Some(value) = value {
                target[axis] = if self.relative {
                    self.position[axis] + value
                } else {
                    value
                };
            }
        }
        target
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn go(x: Option<f64>, y: Option<f64>, z: Option<f64>, f: Option<f64>) -> Command {
        Command::Move {
            x,
            y,
            z,
            feed_mm_min: f,
        }
    }

    #[test]
    fn a_move_takes_its_length_over_the_feed_rate() {
        let mut timeline = Timeline::default();
        assert_eq!(
            timeline.advance(&go(Some(30.0), Some(40.0), None, Some(600.0))),
            5.0
        );
    }

    #[test]
    fn the_feed_rate_carries_over_to_later_moves() {
        let mut timeline = Timeline::default();
        timeline.advance(&go(None, None, None, Some(1200.0)));
        assert_eq!(timeline.advance(&go(Some(10.0), None, None, None)), 0.5);
    }

    #[test]
    fn z_counts_toward_the_length() {
        let mut timeline = Timeline::default();
        assert_eq!(
            timeline.advance(&go(None, None, Some(10.0), Some(600.0))),
            1.0
        );
    }

    #[test]
    fn nothing_is_counted_before_a_feed_rate_is_known() {
        let mut timeline = Timeline::default();
        assert_eq!(timeline.advance(&go(Some(100.0), None, None, None)), 0.0);
    }

    #[test]
    fn relative_moves_add_to_the_position() {
        let mut timeline = Timeline::default();
        timeline.advance(&go(Some(10.0), None, None, Some(600.0)));
        timeline.advance(&Command::Relative);
        assert_eq!(timeline.advance(&go(Some(10.0), None, None, None)), 1.0);
        timeline.advance(&Command::Absolute);
        assert_eq!(timeline.advance(&go(Some(20.0), None, None, None)), 0.0);
    }

    #[test]
    fn set_position_moves_nothing_and_takes_no_time() {
        let mut timeline = Timeline::default();
        timeline.advance(&go(None, None, None, Some(600.0)));
        assert_eq!(
            timeline.advance(&Command::SetPosition {
                x: Some(50.0),
                y: None,
                z: None,
            }),
            0.0
        );
        assert_eq!(timeline.advance(&go(Some(60.0), None, None, None)), 1.0);
    }

    #[test]
    fn a_move_with_no_axis_words_takes_no_time() {
        // A retract or a bare feed-rate change (`G1 E1 F600`, `G1 F3000`)
        // carries no XYZ, so it moves nothing and takes nothing — the pass
        // never offers it as a place to put the fan command.
        let mut timeline = Timeline::default();
        timeline.advance(&go(Some(10.0), None, None, Some(600.0)));
        assert_eq!(timeline.advance(&go(None, None, None, Some(1200.0))), 0.0);
        assert_eq!(timeline.advance(&go(None, None, None, None)), 0.0);
        // The position is untouched, so the next real move still costs its
        // whole length at the new feed rate.
        assert_eq!(timeline.advance(&go(Some(30.0), None, None, None)), 1.0);
    }

    #[test]
    fn a_dwell_takes_its_own_time() {
        let mut timeline = Timeline::default();
        assert_eq!(timeline.advance(&Command::Dwell { seconds: 1.5 }), 1.5);
    }
}
