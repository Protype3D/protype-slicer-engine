//! A calibration test the slice runs: libslic3r's per-layer `CalibMode`
//! (retraction length and speed, pressure advance, outer-wall speed by print
//! height). Values are in libslic3r's own units — mm, s, mm/s.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationTestMode {
    /// `retraction_length = start + floor(max(0, z - 0.4)) * step`, mm.
    RetractionTower,
    /// `SET_PRESSURE_ADVANCE start + floor(z) * step`, s.
    PaTower,
    /// `outer_wall_speed = start + z * step`, mm/s.
    VolSpeedTower,
    /// Retraction length (`start` → `end`, mm) and retraction speed
    /// (`speed_start` → `speed_end`, mm/s), stepped evenly across the bands
    /// of [`RetractionSteps`]; a series whose ends are equal holds its value.
    RetractionSteps,
}

/// Band layout and speed series of a `retraction_steps` test.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetractionSteps {
    /// Bands in the test, the first starting at `base_height`.
    pub bands: u32,
    /// Height of one band, mm.
    pub band_height: f64,
    /// Print height where the first band starts, mm.
    pub base_height: f64,
    /// Retraction speed of the first and of the last band, mm/s.
    pub speed_start: f64,
    pub speed_end: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationTest {
    pub mode: CalibrationTestMode,
    pub start: f64,
    pub end: f64,
    pub step: f64,
    /// `retraction_steps` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retraction: Option<RetractionSteps>,
}

impl CalibrationTest {
    pub fn validate(&self) -> Result<(), String> {
        if self.mode == CalibrationTestMode::RetractionSteps {
            return self.validate_retraction_steps();
        }
        if self.retraction.is_some() {
            return Err("the retraction layout is only for retraction_steps".into());
        }
        for (name, value) in [
            ("start", self.start),
            ("end", self.end),
            ("step", self.step),
        ] {
            if !value.is_finite() {
                return Err(format!("{name} must be finite, got {value}"));
            }
        }
        if self.step == 0.0 {
            return Err("step must not be zero".into());
        }
        if (self.end - self.start) * self.step < 0.0 {
            return Err("step must point from start to end".into());
        }
        let lowest = self.start.min(self.end);
        match self.mode {
            CalibrationTestMode::VolSpeedTower if lowest <= 0.0 => {
                Err(format!("speed must stay positive, lowest is {lowest}"))
            }
            CalibrationTestMode::RetractionTower | CalibrationTestMode::PaTower if lowest < 0.0 => {
                Err(format!("value must not go negative, lowest is {lowest}"))
            }
            _ => Ok(()),
        }
    }

    /// Per-band increments of the length and the speed (mm, mm/s) of a
    /// `retraction_steps` test; `None` for any other mode.
    pub fn retraction_step_sizes(&self) -> Option<(f64, f64)> {
        let r = self.retraction.as_ref()?;
        let intervals = f64::from(r.bands.saturating_sub(1));
        if intervals == 0.0 {
            return Some((0.0, 0.0));
        }
        Some((
            (self.end - self.start) / intervals,
            (r.speed_end - r.speed_start) / intervals,
        ))
    }

    fn validate_retraction_steps(&self) -> Result<(), String> {
        let Some(r) = self.retraction.as_ref() else {
            return Err("retraction_steps needs its retraction layout".into());
        };
        for (name, value) in [
            ("start", self.start),
            ("end", self.end),
            ("band_height", r.band_height),
            ("base_height", r.base_height),
            ("speed_start", r.speed_start),
            ("speed_end", r.speed_end),
        ] {
            if !value.is_finite() {
                return Err(format!("{name} must be finite, got {value}"));
            }
        }
        if r.bands == 0 {
            return Err("retraction_steps needs at least one band".into());
        }
        if r.band_height <= 0.0 {
            return Err(format!(
                "band_height must be positive, got {}",
                r.band_height
            ));
        }
        if r.base_height < 0.0 {
            return Err(format!(
                "base_height must not be negative, got {}",
                r.base_height
            ));
        }
        let lowest_length = self.start.min(self.end);
        if lowest_length < 0.0 {
            return Err(format!(
                "value must not go negative, lowest is {lowest_length}"
            ));
        }
        let lowest_speed = r.speed_start.min(r.speed_end);
        if lowest_speed <= 0.0 {
            return Err(format!(
                "speed must stay positive, lowest is {lowest_speed}"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test(mode: CalibrationTestMode, start: f64, end: f64, step: f64) -> CalibrationTest {
        CalibrationTest {
            mode,
            start,
            end,
            step,
            retraction: None,
        }
    }

    fn steps(start: f64, end: f64, retraction: RetractionSteps) -> CalibrationTest {
        CalibrationTest {
            mode: CalibrationTestMode::RetractionSteps,
            start,
            end,
            step: 0.0,
            retraction: Some(retraction),
        }
    }

    fn speed_bands(speed_start: f64, speed_end: f64) -> RetractionSteps {
        RetractionSteps {
            bands: 9,
            band_height: 3.0,
            base_height: 0.4,
            speed_start,
            speed_end,
        }
    }

    #[test]
    fn accepts_retraction_steps_with_a_constant_length() {
        assert_eq!(steps(0.8, 0.8, speed_bands(20.0, 60.0)).validate(), Ok(()));
    }

    #[test]
    fn steps_length_and_speed_evenly_across_the_bands() {
        let t = steps(0.0, 1.6, speed_bands(20.0, 60.0));
        assert_eq!(t.retraction_step_sizes(), Some((0.2, 5.0)));
        let single = steps(
            0.8,
            0.8,
            RetractionSteps {
                bands: 1,
                ..speed_bands(30.0, 30.0)
            },
        );
        assert_eq!(single.retraction_step_sizes(), Some((0.0, 0.0)));
    }

    #[test]
    fn refuses_retraction_steps_without_their_layout() {
        let err = test(CalibrationTestMode::RetractionSteps, 0.0, 1.0, 0.1)
            .validate()
            .unwrap_err();
        assert!(err.contains("retraction"), "{err}");
    }

    #[test]
    fn refuses_a_non_positive_retraction_speed() {
        let err = steps(0.8, 0.8, speed_bands(0.0, 40.0))
            .validate()
            .unwrap_err();
        assert!(err.contains("speed must stay positive"), "{err}");
    }

    #[test]
    fn refuses_a_flat_band() {
        let flat = RetractionSteps {
            band_height: 0.0,
            ..speed_bands(20.0, 40.0)
        };
        let err = steps(0.8, 0.8, flat).validate().unwrap_err();
        assert!(err.contains("band_height"), "{err}");
    }

    #[test]
    fn refuses_a_retraction_layout_on_another_mode() {
        let mut t = test(CalibrationTestMode::PaTower, 0.0, 0.08, 0.002);
        t.retraction = Some(speed_bands(20.0, 40.0));
        assert!(t
            .validate()
            .unwrap_err()
            .contains("only for retraction_steps"));
    }

    #[test]
    fn parses_the_retraction_steps_wire_form() {
        let parsed: CalibrationTest = serde_json::from_value(serde_json::json!({
            "mode": "retraction_steps", "start": 0.8, "end": 0.8, "step": 0.0,
            "retraction": { "bands": 9, "band_height": 3.0, "base_height": 0.4,
                            "speed_start": 20.0, "speed_end": 60.0 }
        }))
        .unwrap();
        assert_eq!(parsed.mode, CalibrationTestMode::RetractionSteps);
        assert_eq!(parsed.retraction.unwrap().bands, 9);
    }

    #[test]
    fn accepts_an_ascending_retraction_ladder() {
        assert_eq!(
            test(CalibrationTestMode::RetractionTower, 0.0, 1.5, 0.1).validate(),
            Ok(())
        );
    }

    #[test]
    fn refuses_a_zero_step() {
        let err = test(CalibrationTestMode::PaTower, 0.0, 0.08, 0.0)
            .validate()
            .unwrap_err();
        assert!(err.contains("step must not be zero"), "{err}");
    }

    #[test]
    fn refuses_a_step_pointing_away_from_end() {
        let err = test(CalibrationTestMode::RetractionTower, 1.0, 0.0, 0.1)
            .validate()
            .unwrap_err();
        assert!(err.contains("step must point"), "{err}");
    }

    #[test]
    fn refuses_non_finite_values() {
        let err = test(CalibrationTestMode::PaTower, f64::NAN, 0.08, 0.002)
            .validate()
            .unwrap_err();
        assert!(err.contains("start must be finite"), "{err}");
    }

    #[test]
    fn refuses_a_negative_retraction() {
        let err = test(CalibrationTestMode::RetractionTower, -0.2, 1.0, 0.1)
            .validate()
            .unwrap_err();
        assert!(err.contains("must not go negative"), "{err}");
    }

    #[test]
    fn refuses_a_non_positive_speed() {
        let err = test(CalibrationTestMode::VolSpeedTower, 0.0, 40.0, 1.0)
            .validate()
            .unwrap_err();
        assert!(err.contains("speed must stay positive"), "{err}");
    }

    #[test]
    fn parses_the_wire_form() {
        let parsed: CalibrationTest = serde_json::from_value(serde_json::json!({
            "mode": "pa_tower", "start": 0.0, "end": 0.08, "step": 0.002
        }))
        .unwrap();
        assert_eq!(parsed.mode, CalibrationTestMode::PaTower);
    }

    #[test]
    fn refuses_an_unknown_mode() {
        let parsed = serde_json::from_value::<CalibrationTest>(serde_json::json!({
            "mode": "temp_tower", "start": 1.0, "end": 2.0, "step": 1.0
        }));
        assert!(parsed.is_err());
    }
}
