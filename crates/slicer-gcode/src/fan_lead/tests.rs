use super::*;

#[test]
fn defaults_match_the_printer_settings() {
    let params = FanLeadParams::default();
    assert_eq!(params.spinup_s, 1.0);
    assert_eq!(params.spindown_s, 2.0);
    assert!(params.validate().is_ok());
}

#[test]
fn params_outside_zero_to_ten_seconds_are_refused() {
    for (spinup_s, spindown_s) in [(-0.1, 2.0), (1.0, 10.5), (f64::NAN, 2.0)] {
        let err = FanLeadParams {
            spinup_s,
            spindown_s,
        }
        .validate()
        .expect_err("out of range");
        assert!(matches!(err, GcodeError::InvalidFanLead { .. }), "{err}");
    }
    assert!(FanLeadParams {
        spinup_s: 0.0,
        spindown_s: 10.0
    }
    .validate()
    .is_ok());
}

#[test]
fn missing_param_fields_take_their_defaults() {
    let params: FanLeadParams = serde_json::from_str(r#"{"spinup_s": 0.5}"#).unwrap();
    assert_eq!(
        params,
        FanLeadParams {
            spinup_s: 0.5,
            spindown_s: 2.0
        }
    );
}

#[test]
fn diagnostics_have_the_agreed_wording() {
    assert_eq!(
        applied_diagnostic(3, 1),
        "; fan_lead: applied shifted=3 merged=1"
    );
    assert_eq!(
        skipped_diagnostic("line 7: bad"),
        "; fan_lead: skipped (line 7: bad)"
    );
}

/// Lines joined with `\n`, each terminated.
fn gcode(lines: &[&str]) -> String {
    lines.iter().map(|line| format!("{line}\n")).collect()
}

/// Eight 2.5 mm moves at F600 from X0: each takes exactly 0.25 s, the last
/// ends at t = 2.0 s.
const QUARTER_SECOND_MOVES: [&str; 8] = [
    "G1 X2.5 E.1",
    "G1 X5 E.1",
    "G1 X7.5 E.1",
    "G1 X10 E.1",
    "G1 X12.5 E.1",
    "G1 X15 E.1",
    "G1 X17.5 E.1",
    "G1 X20 E.1",
];

/// A body that starts at X0 with the fan at `start_fan` and runs the eight
/// quarter-second moves, then `tail`.
fn quarter_second_body(start_fan: &str, tail: &[&str]) -> String {
    let mut lines = vec!["M83", start_fan, ";LAYER_CHANGE", ";Z:0.2", "G1 X0 Y0 F600"];
    lines.extend(QUARTER_SECOND_MOVES);
    lines.extend(tail);
    gcode(&lines)
}

fn applied(source: &str, params: FanLeadParams) -> (String, usize, usize) {
    match apply_fan_lead(source, params) {
        FanLeadOutcome::Applied {
            gcode,
            shifted,
            merged,
        } => (gcode, shifted, merged),
        other => panic!("expected the pass to apply, got {other:?}"),
    }
}

fn skip_reason(source: &str) -> String {
    skip_reason_with(source, FanLeadParams::default())
}

fn skip_reason_with(source: &str, params: FanLeadParams) -> String {
    match apply_fan_lead(source, params) {
        FanLeadOutcome::Skipped { gcode, reason } => {
            assert!(
                gcode.starts_with(source),
                "a skipped file keeps every original byte"
            );
            assert_eq!(
                &gcode[source.len()..],
                format!("; fan_lead: skipped ({reason})\n"),
                "and gains exactly one diagnostic line"
            );
            reason
        }
        other => panic!("expected the pass to skip, got {other:?}"),
    }
}

#[test]
fn a_file_without_markers_is_returned_byte_for_byte() {
    let source = "M83\r\n;LAYER_CHANGE\r\nM106 S255\r\nT1\r\n; PAINT_FANCY S80\r\nG1 X1O";
    assert_eq!(
        apply_fan_lead(source, FanLeadParams::default()),
        FanLeadOutcome::Unchanged
    );
}

#[test]
fn a_rise_is_led_by_the_spinup_time() {
    let source = gcode(&[
        "M83",
        ";LAYER_CHANGE",
        ";Z:0.2",
        "G1 X0 Y0 F600",
        "G1 X10 E1",
        "G1 X20 E1",
        "G1 X30 E1",
        "; PAINT_FAN S80",
        "M106 S204",
        "G1 X40 E1",
        "; PAINT_FAN END",
        "M106 S0",
        "G1 X50 E1",
    ]);
    // 0 → 80 %: 1.0 s × 204/255 = 0.8 s; the 1 s move before the zone covers it.
    let (out, shifted, merged) = applied(&source, FanLeadParams::default());
    assert_eq!(
        out,
        gcode(&[
            "M83",
            ";LAYER_CHANGE",
            ";Z:0.2",
            "G1 X0 Y0 F600",
            "G1 X10 E1",
            "G1 X20 E1",
            "M106 S204",
            "G1 X30 E1",
            "; PAINT_FAN S80",
            "G1 X40 E1",
            "; PAINT_FAN END",
            "M106 S0",
            "G1 X50 E1",
            "; fan_lead: applied shifted=1 merged=0",
        ])
    );
    assert_eq!((shifted, merged), (1, 0));
}

#[test]
fn a_rise_and_a_fall_each_use_their_own_time() {
    let params = FanLeadParams {
        spinup_s: 1.0,
        spindown_s: 2.0,
    };
    // 0 → 100 %: 1.0 s back from t = 2.0 is the start of X12.5.
    let rise = quarter_second_body(
        "M107",
        &[
            "; PAINT_FAN S100",
            "M106 S255",
            "G1 X30 E1",
            "; PAINT_FAN END",
        ],
    );
    let (out, _, _) = applied(&rise, params);
    assert!(
        out.contains("G1 X10 E.1\nM106 S255\nG1 X12.5 E.1\n"),
        "{out}"
    );

    // 100 → 0 %: 2.0 s back is the start of X2.5.
    let fall = quarter_second_body(
        "M106 S255",
        &["; PAINT_FAN S0", "M106 S0", "G1 X30 E1", "; PAINT_FAN END"],
    );
    let (out, _, _) = applied(&fall, params);
    assert!(
        out.contains("G1 X0 Y0 F600\nM106 S0\nG1 X2.5 E.1\n"),
        "{out}"
    );
}

#[test]
fn the_lead_is_proportional_to_the_speed_change() {
    // 0 → 50 % (S127): 1.0 s × 127/255 = 0.498 s → start of X17.5 (0.5 s back).
    let half = quarter_second_body(
        "M107",
        &[
            "; PAINT_FAN S50",
            "M106 S127",
            "G1 X30 E1",
            "; PAINT_FAN END",
        ],
    );
    let (out, _, _) = applied(&half, FanLeadParams::default());
    assert!(
        out.contains("G1 X15 E.1\nM106 S127\nG1 X17.5 E.1\n"),
        "{out}"
    );
}

#[test]
fn a_zero_time_disables_the_lead_in_that_direction() {
    let source = quarter_second_body(
        "M107",
        &[
            "; PAINT_FAN S100",
            "M106 S255",
            "G1 X30 E1",
            "; PAINT_FAN END",
        ],
    );
    let params = FanLeadParams {
        spinup_s: 0.0,
        spindown_s: 2.0,
    };
    let (out, shifted, _) = applied(&source, params);
    assert_eq!(
        out,
        format!("{source}; fan_lead: applied shifted=0 merged=0\n")
    );
    assert_eq!(shifted, 0);
}

#[test]
fn automatic_fan_commands_inside_the_lead_are_dropped() {
    // The profile raises the fan to 40 % at t = 1.75. Counted from there the
    // zone would need 0.6 s; but the lead has to start where the fan still
    // stands at 0, so it needs the full 1.0 s and the 40 % command in
    // between is overridden.
    let mut lines = vec!["M83", "M107", ";LAYER_CHANGE", ";Z:0.2", "G1 X0 Y0 F600"];
    lines.extend(&QUARTER_SECOND_MOVES[..7]);
    lines.extend([
        "M106 S102",
        "G1 X20 E.1",
        "; PAINT_FAN S100",
        "M106 S255",
        "G1 X30 E1",
        "; PAINT_FAN END",
    ]);
    let source = gcode(&lines);

    let (out, shifted, _) = applied(&source, FanLeadParams::default());

    let mut expected = vec!["M83", "M107", ";LAYER_CHANGE", ";Z:0.2", "G1 X0 Y0 F600"];
    expected.extend(&QUARTER_SECOND_MOVES[..4]);
    expected.extend([
        "M106 S255",
        "G1 X12.5 E.1",
        "G1 X15 E.1",
        "G1 X17.5 E.1",
        "G1 X20 E.1",
        "; PAINT_FAN S100",
        "G1 X30 E1",
        "; PAINT_FAN END",
        "; fan_lead: applied shifted=1 merged=0",
    ]);
    assert_eq!(out, gcode(&expected));
    assert_eq!(shifted, 1);
}

#[test]
fn a_long_move_is_not_split_and_the_lead_is_not_shortened() {
    let source = gcode(&[
        "M83",
        ";LAYER_CHANGE",
        ";Z:0.2",
        "G1 X0 Y0 F600",
        "G1 X50 E5",
        "G1 X52.5 E.1",
        "; PAINT_FAN S100",
        "M106 S255",
        "G1 X60 E1",
        "; PAINT_FAN END",
    ]);
    // Needs 1.0 s; the 0.25 s move is not enough, the 5 s move before it is
    // taken whole — 5.25 s of lead rather than a split move.
    let (out, _, _) = applied(&source, FanLeadParams::default());
    assert_eq!(
        out,
        gcode(&[
            "M83",
            ";LAYER_CHANGE",
            ";Z:0.2",
            "G1 X0 Y0 F600",
            "M106 S255",
            "G1 X50 E5",
            "G1 X52.5 E.1",
            "; PAINT_FAN S100",
            "G1 X60 E1",
            "; PAINT_FAN END",
            "; fan_lead: applied shifted=1 merged=0",
        ])
    );
}

#[test]
fn the_lead_crosses_a_layer_change() {
    let source = gcode(&[
        "M83",
        ";LAYER_CHANGE",
        ";Z:0.2",
        "G1 X0 Y0 F600",
        "G1 X10 E1",
        ";LAYER_CHANGE",
        ";Z:0.4",
        "G1 X15 E.5",
        "; PAINT_FAN S100",
        "M106 S255",
        "G1 X20 E.5",
        "; PAINT_FAN END",
    ]);
    let (out, shifted, _) = applied(&source, FanLeadParams::default());
    assert!(
        out.contains("G1 X0 Y0 F600\nM106 S255\nG1 X10 E1\n;LAYER_CHANGE\n"),
        "{out}"
    );
    assert_eq!(shifted, 1);
}

#[test]
fn the_lead_stops_at_the_first_layer_change() {
    let source = gcode(&[
        "M83",
        "G1 X0 Y0 F600",
        "G1 X10 E1",
        ";LAYER_CHANGE",
        ";Z:0.2",
        "G1 X15 E.5",
        "; PAINT_FAN S100",
        "M106 S255",
        "G1 X20 E.5",
        "; PAINT_FAN END",
    ]);
    // 1.0 s wanted, only 0.5 s of body before the zone: the command goes to
    // the first move of the body and no further.
    let (out, shifted, _) = applied(&source, FanLeadParams::default());
    assert!(
        out.contains(";LAYER_CHANGE\n;Z:0.2\nM106 S255\nG1 X15 E.5\n; PAINT_FAN S100\nG1 X20"),
        "{out}"
    );
    assert_eq!(shifted, 1);
}

#[test]
fn m107_is_a_part_fan_command_and_p3_is_not() {
    let source = gcode(&[
        "M83",
        "M106 S255",
        ";LAYER_CHANGE",
        ";Z:0.2",
        "G1 X0 Y0 F600",
        "G1 X10 E1",
        "G1 X20 E1",
        "M106 P3 S0",
        "G1 X30 E1",
        "; PAINT_FAN S0",
        "M107",
        "G1 X40 E1",
        "; PAINT_FAN END",
    ]);
    // 100 → 0 % with M107: 2.0 s. The exhaust fan command is neither a speed
    // change for the part fan nor something to drop.
    let (out, _, _) = applied(&source, FanLeadParams::default());
    assert_eq!(
        out,
        gcode(&[
            "M83",
            "M106 S255",
            ";LAYER_CHANGE",
            ";Z:0.2",
            "G1 X0 Y0 F600",
            "G1 X10 E1",
            "M107",
            "G1 X20 E1",
            "M106 P3 S0",
            "G1 X30 E1",
            "; PAINT_FAN S0",
            "G1 X40 E1",
            "; PAINT_FAN END",
            "; fan_lead: applied shifted=1 merged=0",
        ])
    );
}

#[test]
fn a_malformed_marker_skips_the_pass_and_says_why() {
    let source = gcode(&[
        "M83",
        ";LAYER_CHANGE",
        "G1 X0 Y0 F600",
        "; PAINT_FAN S150",
        "M106 S255",
    ]);
    assert_eq!(
        skip_reason(&source),
        "line 4: unreadable paint fan marker `; PAINT_FAN S150`"
    );
}

#[test]
fn markers_without_any_layer_change_skip_the_pass() {
    let source = gcode(&["G1 X0 Y0 F600", "; PAINT_FAN S100", "M106 S255", "G1 X10"]);
    let reason = skip_reason(&source);
    assert!(reason.contains("LAYER_CHANGE"), "{reason}");
}

#[test]
fn an_unreadable_word_skips_only_a_file_that_has_markers() {
    let with_marker = gcode(&[
        "M83",
        ";LAYER_CHANGE",
        "G1 X1O Y0 F600",
        "; PAINT_FAN S100",
        "M106 S255",
        "; PAINT_FAN END",
    ]);
    assert_eq!(
        skip_reason(&with_marker),
        "line 3: cannot read `X1O` in a motion or fan command"
    );

    let without = gcode(&["M83", ";LAYER_CHANGE", "G1 X1O Y0 F600"]);
    assert_eq!(
        apply_fan_lead(&without, FanLeadParams::default()),
        FanLeadOutcome::Unchanged
    );
}

#[test]
fn a_kickstart_command_right_before_the_marker_is_dropped_too() {
    // The engine can leave an automatic (kick-start) fan command on the line
    // just before `; PAINT_FAN`, with no move in between. It is automatic all
    // the same: the lead is measured from the speed the fan really stood at
    // before it, and the command it overrides goes with the rest of the
    // window.
    let mut lines = vec!["M83", "M107", ";LAYER_CHANGE", ";Z:0.2", "G1 X0 Y0 F600"];
    lines.extend(&QUARTER_SECOND_MOVES);
    lines.extend([
        "M106 S102",
        "; PAINT_FAN S100",
        "M106 S255",
        "G1 X30 E1",
        "; PAINT_FAN END",
    ]);
    let source = gcode(&lines);

    let (out, shifted, _) = applied(&source, FanLeadParams::default());

    let mut expected = vec!["M83", "M107", ";LAYER_CHANGE", ";Z:0.2", "G1 X0 Y0 F600"];
    expected.extend(&QUARTER_SECOND_MOVES[..4]);
    expected.push("M106 S255");
    expected.extend(&QUARTER_SECOND_MOVES[4..]);
    expected.extend([
        "; PAINT_FAN S100",
        "G1 X30 E1",
        "; PAINT_FAN END",
        "; fan_lead: applied shifted=1 merged=0",
    ]);
    assert_eq!(out, gcode(&expected));
    assert_eq!(shifted, 1);
}

#[test]
fn the_lead_stops_at_a_tool_change() {
    let source = gcode(&[
        "M83",
        ";LAYER_CHANGE",
        ";Z:0.2",
        "G1 X0 Y0 F600",
        "G1 X10 E1",
        "T1 ; change extruder",
        "G1 X15 E.5",
        "; PAINT_FAN S100",
        "M106 S255",
        "G1 X20 E.5",
        "; PAINT_FAN END",
    ]);
    // The command stops at the first move after `T1` — and is written once:
    // a copy left behind would blow the wrong nozzle's fan.
    let (out, shifted, merged) = applied(&source, FanLeadParams::default());
    assert_eq!(
        out,
        gcode(&[
            "M83",
            ";LAYER_CHANGE",
            ";Z:0.2",
            "G1 X0 Y0 F600",
            "G1 X10 E1",
            "T1 ; change extruder",
            "M106 S255",
            "G1 X15 E.5",
            "; PAINT_FAN S100",
            "G1 X20 E.5",
            "; PAINT_FAN END",
            "; fan_lead: applied shifted=1 merged=0",
        ])
    );
    assert_eq!((shifted, merged), (1, 0));
}

#[test]
fn a_lead_that_reaches_the_previous_exit_replaces_its_command() {
    let source = gcode(&[
        "M83",
        ";LAYER_CHANGE",
        ";Z:0.2",
        "G1 X0 Y0 F600",
        "; PAINT_FAN S100",
        "M106 S255",
        "G1 X10 E1",
        "; PAINT_FAN END",
        "M106 S0",
        "G1 X12.5 E.1",
        "; PAINT_FAN S80",
        "M106 S204",
        "G1 X20 E1",
        "; PAINT_FAN END",
        "M106 S0",
    ]);
    // The second zone needs 0.8 s and has 0.25 s after the first zone's exit:
    // the fan goes from 100 % straight to 80 % where the first zone ends.
    let (out, shifted, merged) = applied(&source, FanLeadParams::default());
    assert_eq!(
        out,
        gcode(&[
            "M83",
            ";LAYER_CHANGE",
            ";Z:0.2",
            "G1 X0 Y0 F600",
            "; PAINT_FAN S100",
            "M106 S255",
            "G1 X10 E1",
            "; PAINT_FAN END",
            "M106 S204",
            "G1 X12.5 E.1",
            "; PAINT_FAN S80",
            "G1 X20 E1",
            "; PAINT_FAN END",
            "M106 S0",
            "; fan_lead: applied shifted=1 merged=1",
        ])
    );
    assert_eq!((shifted, merged), (1, 1));
}

#[test]
fn a_long_enough_gap_leaves_the_previous_exit_alone() {
    let source = gcode(&[
        "M83",
        ";LAYER_CHANGE",
        ";Z:0.2",
        "G1 X0 Y0 F600",
        "; PAINT_FAN S100",
        "M106 S255",
        "G1 X10 E1",
        "; PAINT_FAN END",
        "M106 S0",
        "G1 X20 E1",
        "G1 X30 E1",
        "; PAINT_FAN S80",
        "M106 S204",
        "G1 X40 E1",
        "; PAINT_FAN END",
        "M106 S0",
    ]);
    let (out, shifted, merged) = applied(&source, FanLeadParams::default());
    assert!(
        out.contains(
            "; PAINT_FAN END\nM106 S0\nG1 X20 E1\nM106 S204\nG1 X30 E1\n; PAINT_FAN S80\nG1 X40"
        ),
        "{out}"
    );
    assert_eq!((shifted, merged), (1, 0));
}

#[test]
fn an_unbalanced_zone_skips_the_pass() {
    let exit_first = gcode(&[
        "M83",
        ";LAYER_CHANGE",
        "G1 X0 Y0 F600",
        "; PAINT_FAN END",
        "M106 S0",
    ]);
    assert_eq!(
        skip_reason(&exit_first),
        "line 4: paint fan zone is closed without being opened"
    );

    let nested = gcode(&[
        "M83",
        ";LAYER_CHANGE",
        "; PAINT_FAN S50",
        "M106 S127",
        "; PAINT_FAN S80",
        "M106 S204",
        "; PAINT_FAN END",
    ]);
    assert_eq!(
        skip_reason(&nested),
        "line 5: paint fan zone is opened inside another zone"
    );

    let never_closed = gcode(&["M83", ";LAYER_CHANGE", "; PAINT_FAN S50", "M106 S127"]);
    assert_eq!(
        skip_reason(&never_closed),
        "line 3: paint fan zone is never closed"
    );
}

#[test]
fn an_absurd_lead_time_skips_the_pass_however_long_the_file() {
    let moves: Vec<String> = (1..=20_000).map(|x| format!("G1 X{x} E.1")).collect();
    let mut lines = vec!["M83", ";LAYER_CHANGE", ";Z:0.2", "G1 X0 Y0 F600"];
    lines.extend(moves.iter().map(String::as_str));
    lines.extend([
        "; PAINT_FAN S100",
        "M106 S255",
        "G1 X30000 E1",
        "; PAINT_FAN END",
    ]);
    let params = FanLeadParams {
        spinup_s: 1e6,
        spindown_s: 2.0,
    };
    assert_eq!(
        skip_reason_with(&gcode(&lines), params),
        "fan lead spinup_s must be between 0 and 10 s, got 1000000"
    );
}

#[test]
fn a_file_that_ends_without_a_newline_gets_one_before_the_diagnostic() {
    let source = format!(
        "{}; PAINT_FAN END",
        gcode(&[
            "M83",
            ";LAYER_CHANGE",
            ";Z:0.2",
            "G1 X0 Y0 F600",
            "G1 X10 E1",
            "; PAINT_FAN S100",
            "M106 S255",
            "G1 X20 E1",
        ])
    );
    let (out, shifted, merged) = applied(&source, FanLeadParams::default());
    assert_eq!(
        out,
        gcode(&[
            "M83",
            ";LAYER_CHANGE",
            ";Z:0.2",
            "G1 X0 Y0 F600",
            "M106 S255",
            "G1 X10 E1",
            "; PAINT_FAN S100",
            "G1 X20 E1",
            "; PAINT_FAN END",
            "; fan_lead: applied shifted=1 merged=0",
        ])
    );
    assert_eq!((shifted, merged), (1, 0));
}

/// Lines joined with `\r\n`, each terminated — a file written on Windows.
fn crlf(lines: &[&str]) -> String {
    lines.iter().map(|line| format!("{line}\r\n")).collect()
}

#[test]
fn a_crlf_file_keeps_its_line_endings_diagnostic_included() {
    let source = crlf(&[
        "M83",
        ";LAYER_CHANGE",
        ";Z:0.2",
        "G1 X0 Y0 F600",
        "G1 X10 E1",
        "; PAINT_FAN S100",
        "M106 S255",
        "G1 X20 E1",
        "; PAINT_FAN END",
    ]);
    let (out, shifted, merged) = applied(&source, FanLeadParams::default());
    assert_eq!(
        out,
        crlf(&[
            "M83",
            ";LAYER_CHANGE",
            ";Z:0.2",
            "G1 X0 Y0 F600",
            "M106 S255",
            "G1 X10 E1",
            "; PAINT_FAN S100",
            "G1 X20 E1",
            "; PAINT_FAN END",
            "; fan_lead: applied shifted=1 merged=0",
        ])
    );
    assert_eq!((shifted, merged), (1, 0));

    // A skipped file keeps them too.
    let unbalanced = crlf(&["M83", ";LAYER_CHANGE", "; PAINT_FAN END"]);
    match apply_fan_lead(&unbalanced, FanLeadParams::default()) {
        FanLeadOutcome::Skipped { gcode, reason } => {
            assert_eq!(
                reason,
                "line 3: paint fan zone is closed without being opened"
            );
            assert_eq!(
                gcode,
                format!("{unbalanced}; fan_lead: skipped ({reason})\r\n")
            );
        }
        other => panic!("expected the pass to skip, got {other:?}"),
    }
}

#[test]
fn a_previous_exit_without_a_command_gives_up_the_line_after_its_marker() {
    // The first zone's exit changes nothing (the painted speed is what the
    // profile asks for outside it too), so the engine writes no command
    // there — there is nothing to replace, and the lead still stops at the
    // marker rather than reaching into the zone.
    let source = gcode(&[
        "M83",
        ";LAYER_CHANGE",
        ";Z:0.2",
        "G1 X0 Y0 F600",
        "; PAINT_FAN S100",
        "M106 S255",
        "G1 X10 E1",
        "; PAINT_FAN END",
        "G1 X12.5 E.1",
        "; PAINT_FAN S80",
        "M106 S204",
        "G1 X20 E1",
        "; PAINT_FAN END",
        "M106 S0",
    ]);
    // 100 → 80 % needs 2.0 s × 51/255 = 0.4 s; only 0.25 s stands between
    // the zones.
    let (out, shifted, merged) = applied(&source, FanLeadParams::default());
    assert_eq!(
        out,
        gcode(&[
            "M83",
            ";LAYER_CHANGE",
            ";Z:0.2",
            "G1 X0 Y0 F600",
            "; PAINT_FAN S100",
            "M106 S255",
            "G1 X10 E1",
            "; PAINT_FAN END",
            "M106 S204",
            "G1 X12.5 E.1",
            "; PAINT_FAN S80",
            "G1 X20 E1",
            "; PAINT_FAN END",
            "M106 S0",
            "; fan_lead: applied shifted=1 merged=0",
        ])
    );
    assert_eq!((shifted, merged), (1, 0));
}

#[test]
fn a_tool_change_after_an_exit_blocks_the_merge() {
    // The exit is a floor the lead may lean on, the tool change is one it may
    // not cross. The tool change comes later, so it wins: the command stops
    // at the first move after `T1` and the first zone's exit keeps its own.
    let source = gcode(&[
        "M83",
        ";LAYER_CHANGE",
        ";Z:0.2",
        "G1 X0 Y0 F600",
        "; PAINT_FAN S100",
        "M106 S255",
        "G1 X10 E1",
        "; PAINT_FAN END",
        "M106 S0",
        "T1 ; change extruder",
        "G1 X12.5 E.1",
        "; PAINT_FAN S80",
        "M106 S204",
        "G1 X20 E1",
        "; PAINT_FAN END",
        "M106 S0",
    ]);
    let (out, shifted, merged) = applied(&source, FanLeadParams::default());
    assert_eq!(
        out,
        gcode(&[
            "M83",
            ";LAYER_CHANGE",
            ";Z:0.2",
            "G1 X0 Y0 F600",
            "; PAINT_FAN S100",
            "M106 S255",
            "G1 X10 E1",
            "; PAINT_FAN END",
            "M106 S0",
            "T1 ; change extruder",
            "M106 S204",
            "G1 X12.5 E.1",
            "; PAINT_FAN S80",
            "G1 X20 E1",
            "; PAINT_FAN END",
            "M106 S0",
            "; fan_lead: applied shifted=1 merged=0",
        ])
    );
    assert_eq!((shifted, merged), (1, 0));
}

#[test]
fn an_automatic_command_between_two_zones_goes_with_the_merge() {
    // The profile sets its own 40 % right after the first zone. The merge
    // puts the second zone's speed where the exit command stood — before
    // that 40 %, which therefore has to go with it, or the fan would enter
    // the zone at 40 % anyway.
    let source = gcode(&[
        "M83",
        ";LAYER_CHANGE",
        ";Z:0.2",
        "G1 X0 Y0 F600",
        "; PAINT_FAN S100",
        "M106 S255",
        "G1 X10 E1",
        "; PAINT_FAN END",
        "M106 S0",
        "M106 S102",
        "G1 X12.5 E.1",
        "; PAINT_FAN S80",
        "M106 S204",
        "G1 X20 E1",
        "; PAINT_FAN END",
        "M106 S0",
    ]);
    let (out, shifted, merged) = applied(&source, FanLeadParams::default());
    assert_eq!(
        out,
        gcode(&[
            "M83",
            ";LAYER_CHANGE",
            ";Z:0.2",
            "G1 X0 Y0 F600",
            "; PAINT_FAN S100",
            "M106 S255",
            "G1 X10 E1",
            "; PAINT_FAN END",
            "M106 S204",
            "G1 X12.5 E.1",
            "; PAINT_FAN S80",
            "G1 X20 E1",
            "; PAINT_FAN END",
            "M106 S0",
            "; fan_lead: applied shifted=1 merged=1",
        ])
    );
    assert_eq!((shifted, merged), (1, 1));
}

#[test]
fn the_terminator_rule_is_the_file_s_own() {
    assert_eq!(LineTerminator::default().as_str(), "\n");
    assert_eq!(LineTerminator::of_file("M83\nM107\n").as_str(), "\n");
    assert_eq!(LineTerminator::of_file("M83\r\nM107\n").as_str(), "\r\n");

    let mut streamed = LineTerminator::default();
    streamed.saw_line(b"M83\n");
    assert_eq!(streamed.as_str(), "\n");
    // Once seen it sticks, as `of_file` reads the same file.
    streamed.saw_line(b"M107\r\n");
    streamed.saw_line(b"G1 X1");
    assert_eq!(streamed.as_str(), "\r\n");
    assert_eq!(streamed.as_bytes(), b"\r\n");
}
