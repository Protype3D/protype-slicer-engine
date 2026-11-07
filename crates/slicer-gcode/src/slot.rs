//! Print from a different filament slot.
//!
//! Three lines name the slot of a QIDI print: `BOX_PRINT_START EXTRUDER=<n>`
//! and the `T<n>` tool select decide which slot it feeds from, and
//! `UNLOAD_FILAMENT T=<n>` in the end block must name that same slot, or the
//! machine stops on `QDE_004_025` after the last layer. The start block
//! loads the filament itself, so no manual loading is needed — which is also
//! why `BOX_PRINT_START` must survive untouched.
//!
//! This pass only ever *rewrites* values. It never removes a line: the
//! print that fed from slot 3 instead of slot 1 did so because
//! `INIT_MAPPING_VALUE` had been deleted as "a command that does not
//! exist", and the firmware swallows unknown commands silently, so
//! nothing said otherwise.

use crate::error::GcodeError;
use serde::{Deserialize, Serialize};
use slicer_machine::MachinePassport;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotRewrite {
    pub gcode: String,
    pub tool: String,
    pub tool_index: u32,
    /// Variable in the machine's `save_variables` this tool resolves
    /// through — carried so a caller can check the binding on the
    /// machine rather than assume it.
    pub slot_var: String,
    pub box_print_start_rewrites: usize,
    pub tool_select_rewrites: usize,
    pub unload_filament_rewrites: usize,
    /// Mandatory start-block lines the passport names and this file does
    /// not contain. Not an error here — the file may target a different
    /// machine — but a caller about to print it should look.
    pub missing_mandatory_lines: Vec<String>,
}

/// Point the print at `tool` (e.g. `"T1"`).
pub fn select_tool(
    gcode: &str,
    passport: &MachinePassport,
    tool: &str,
) -> Result<SlotRewrite, GcodeError> {
    let mapping = passport
        .tools
        .get(tool)
        .ok_or_else(|| GcodeError::UnknownTool {
            machine: passport.machine_id.clone(),
            tool: tool.to_string(),
            known: passport
                .tools
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        })?;
    let index: u32 =
        tool.trim_start_matches(['T', 't'])
            .parse()
            .map_err(|_| GcodeError::UnknownTool {
                machine: passport.machine_id.clone(),
                tool: tool.to_string(),
                known: passport
                    .tools
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
            })?;

    let mut out = String::with_capacity(gcode.len());
    let mut box_rewrites = 0;
    let mut tool_rewrites = 0;
    let mut unload_rewrites = 0;

    for line in gcode.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("BOX_PRINT_START") {
            out.push_str(&rewrite_argument(line, "EXTRUDER=", index));
            box_rewrites += 1;
        } else if trimmed.starts_with("UNLOAD_FILAMENT") {
            out.push_str(&rewrite_argument(line, "T=", index));
            unload_rewrites += 1;
        } else if is_tool_select(trimmed) {
            // Preserve the original indentation; only the number changes.
            let indent = &line[..line.len() - line.trim_start().len()];
            out.push_str(indent);
            out.push('T');
            out.push_str(&index.to_string());
            tool_rewrites += 1;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }

    let missing = passport
        .start_block
        .mandatory_lines
        .keys()
        .filter(|command| !gcode.contains(command.as_str()))
        .cloned()
        .collect();

    Ok(SlotRewrite {
        gcode: out,
        tool: tool.to_string(),
        tool_index: index,
        slot_var: mapping.slot_var.clone(),
        box_print_start_rewrites: box_rewrites,
        tool_select_rewrites: tool_rewrites,
        unload_filament_rewrites: unload_rewrites,
        missing_mandatory_lines: missing,
    })
}

/// Replace the value of the `key` argument and leave every other argument
/// — and the rest of the line — exactly as it was.
fn rewrite_argument(line: &str, key: &str, index: u32) -> String {
    let mut out = String::with_capacity(line.len() + 4);
    let mut first = true;
    for token in line.split_inclusive(char::is_whitespace) {
        if token.trim_start().starts_with(key) && !first {
            let trailing: String = token
                .chars()
                .rev()
                .take_while(|c| c.is_whitespace())
                .collect();
            out.push_str(&format!("{key}{index}{trailing}"));
        } else {
            out.push_str(token);
        }
        first = false;
    }
    out
}

/// A standalone `T<digits>` line. `T0` inside a comment, or
/// `TOOL_CHANGE_START`, must not match.
fn is_tool_select(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix('T') else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passport() -> MachinePassport {
        slicer_machine::bundled("qidi-xmax4").expect("bundled passport")
    }

    /// Shaped like the real vendor start block's relevant lines.
    const START: &str = "\
INIT_MAPPING_VALUE
BOX_PRINT_START EXTRUDER=0 HOTENDTEMP=250
M400
T0
G1 X10 Y10
UNLOAD_FILAMENT T=0
";

    #[test]
    fn every_line_that_names_a_slot_is_rewritten() {
        let result = select_tool(START, &passport(), "T1").expect("rewrites");
        assert!(result
            .gcode
            .contains("BOX_PRINT_START EXTRUDER=1 HOTENDTEMP=250"));
        assert!(result.gcode.lines().any(|l| l == "T1"));
        assert!(result.gcode.lines().any(|l| l == "UNLOAD_FILAMENT T=1"));
        assert_eq!(result.box_print_start_rewrites, 1);
        assert_eq!(result.tool_select_rewrites, 1);
        assert_eq!(result.unload_filament_rewrites, 1);
    }

    /// Left at `T=0`, the machine stops on `QDE_004_025` after the last layer:
    /// three prints paused at 99.9% before this line was rewritten.
    #[test]
    fn the_end_block_unloads_the_slot_the_print_fed_from() {
        let result = select_tool(START, &passport(), "T2").unwrap();
        assert!(!result.gcode.contains("UNLOAD_FILAMENT T=0"));
    }

    /// The config trailer quotes the macro as text; the machine never runs it.
    #[test]
    fn the_end_macro_quoted_in_the_trailer_stays_as_it_was() {
        let quoted = "; machine_end_gcode = UNLOAD_FILAMENT T=[current_extruder]\n";
        let result = select_tool(quoted, &passport(), "T2").unwrap();
        assert_eq!(result.gcode, quoted);
        assert_eq!(result.unload_filament_rewrites, 0);
    }

    #[test]
    fn the_rest_of_the_box_print_start_line_survives_untouched() {
        let result = select_tool(START, &passport(), "T2").unwrap();
        let line = result
            .gcode
            .lines()
            .find(|l| l.starts_with("BOX_PRINT_START"))
            .unwrap();
        assert_eq!(line, "BOX_PRINT_START EXTRUDER=2 HOTENDTEMP=250");
    }

    /// The line whose deletion fed a print from the wrong slot.
    #[test]
    fn no_line_is_ever_removed() {
        let result = select_tool(START, &passport(), "T3").unwrap();
        assert_eq!(
            result.gcode.lines().count(),
            START.lines().count(),
            "the pass rewrites values; it does not delete"
        );
        assert!(result.gcode.contains("INIT_MAPPING_VALUE"));
    }

    #[test]
    fn the_slot_variable_comes_back_so_the_binding_can_be_checked() {
        let result = select_tool(START, &passport(), "T1").unwrap();
        assert_eq!(result.slot_var, "value_t1");
    }

    #[test]
    fn a_tool_the_machine_does_not_have_is_refused_and_lists_what_it_does() {
        let err = select_tool(START, &passport(), "T9").expect_err("no such tool");
        let text = err.to_string();
        assert!(text.contains("T9"), "{text}");
        assert!(
            text.contains("T0"),
            "the error must list the real tools: {text}"
        );
    }

    #[test]
    fn a_mandatory_line_absent_from_the_file_is_surfaced() {
        let without = START.replace("INIT_MAPPING_VALUE\n", "");
        let result = select_tool(&without, &passport(), "T1").unwrap();
        assert!(
            result
                .missing_mandatory_lines
                .contains(&"INIT_MAPPING_VALUE".to_string()),
            "got {:?}",
            result.missing_mandatory_lines
        );
    }

    #[test]
    fn a_complete_file_reports_nothing_missing() {
        let result = select_tool(START, &passport(), "T1").unwrap();
        assert!(result.missing_mandatory_lines.is_empty());
    }

    #[test]
    fn tool_change_macros_and_comments_are_not_mistaken_for_tool_selects() {
        let tricky = "TOOL_CHANGE_START F=0 T=1\n; T0 in a comment\nT0\n";
        let result = select_tool(tricky, &passport(), "T1").unwrap();
        assert_eq!(result.tool_select_rewrites, 1);
        assert!(result.gcode.contains("TOOL_CHANGE_START F=0 T=1"));
        assert!(result.gcode.contains("; T0 in a comment"));
    }
}
