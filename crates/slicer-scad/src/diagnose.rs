//! Read OpenSCAD's stderr and say what actually went wrong.
//!
//! Everything here is matched against output captured from OpenSCAD
//! 2021.01 rather than written from the manual, and the captured strings
//! are kept verbatim in the tests. If a later OpenSCAD rewords these,
//! the tests say so instead of the crate quietly degrading every assert
//! failure into a generic "exited with code 1".

use crate::echo::Echoes;
use crate::error::ScadError;

/// Longest tail of stderr to carry into a generic failure. Enough to
/// hold the real message, short enough not to paste a render log into an
/// RPC response.
const TAIL_LIMIT: usize = 2000;

/// Classify a failed run.
///
/// `echoes` are folded into an assertion error, because they are the
/// variable values and OpenSCAD does not put them in the error text.
pub fn classify(stderr: &str, exit_code: Option<i32>, echoes: &Echoes) -> ScadError {
    if let Some(error) = assertion(stderr, echoes) {
        return error;
    }
    if let Some(error) = parser_error(stderr) {
        return error;
    }
    // Checked after the two specific ones: an assert that fires also
    // leaves the top level empty, and reporting that would name the
    // symptom instead of the cause.
    if stderr.contains("Current top level object is empty") {
        return ScadError::EmptyResult;
    }
    if stderr.contains("not a 3D object") {
        return ScadError::NotThreeDimensional;
    }
    ScadError::Failed {
        exit_code,
        stderr_tail: tail(stderr),
    }
}

/// `ERROR: Assertion '<expr>' failed: "<message>" in file <f>, line <n>`
///
/// The message is optional — a bare `assert(cond)` has no `str(...)`, and
/// OpenSCAD then omits the `failed: "..."` clause entirely.
fn assertion(stderr: &str, echoes: &Echoes) -> Option<ScadError> {
    let line = stderr
        .lines()
        .find(|l| l.trim_start().starts_with("ERROR: Assertion"))?;
    let rest = line.trim_start().strip_prefix("ERROR: Assertion")?.trim();

    let (expression, rest) = between(rest, '\'', '\'')?;
    let message = between(rest, '"', '"')
        .map(|(m, _)| m.to_string())
        .unwrap_or_default();
    let (file, line_no) = file_and_line(rest)?;

    Some(ScadError::Assertion {
        expression: expression.to_string(),
        message,
        file,
        line: line_no,
        values: echoes.values.clone(),
        trace: stderr
            .lines()
            .filter(|l| l.trim_start().starts_with("TRACE:"))
            .map(|l| l.trim().to_string())
            .collect(),
    })
}

/// `ERROR: Parser error: syntax error in file <f>, line <n>`
fn parser_error(stderr: &str) -> Option<ScadError> {
    let line = stderr
        .lines()
        .find(|l| l.trim_start().starts_with("ERROR: Parser error"))?;
    let detail = line
        .trim_start()
        .strip_prefix("ERROR: ")
        .unwrap_or(line)
        .split(" in file ")
        .next()
        .unwrap_or("parser error")
        .to_string();
    let (file, line_no) = file_and_line(line)?;
    Some(ScadError::Parse {
        file,
        line: line_no,
        detail,
    })
}

/// Pull `... in file <path>, line <n>` off the end of a diagnostic.
///
/// Scanned from the last occurrence: a path may itself contain the
/// words, and the location clause is always last.
fn file_and_line(text: &str) -> Option<(String, u32)> {
    let at = text.rfind(" in file ")?;
    let rest = &text[at + " in file ".len()..];
    let comma = rest.rfind(", line ")?;
    let file = rest[..comma].to_string();
    let number: String = rest[comma + ", line ".len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    Some((file, number.parse().ok()?))
}

/// Text between the first `open` and the next `close` after it, plus
/// whatever follows — so callers can keep scanning the remainder.
fn between(text: &str, open: char, close: char) -> Option<(&str, &str)> {
    let start = text.find(open)? + open.len_utf8();
    let end = start + text[start..].find(close)?;
    Some((&text[start..end], &text[end + close.len_utf8()..]))
}

fn tail(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.len() <= TAIL_LIMIT {
        return trimmed.to_string();
    }
    let start = trimmed.len() - TAIL_LIMIT;
    let boundary = trimmed
        .char_indices()
        .map(|(i, _)| i)
        .find(|i| *i >= start)
        .unwrap_or(trimmed.len());
    format!("…{}", &trimmed[boundary..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Verbatim from OpenSCAD 2021.01.
    const ASSERT_STDERR: &str = "\
ECHO: sock_od = 60, cup_base = 63.67
ERROR: Assertion '((SOCK_OD + (2 * RIM)) <= CUP_BASE)' failed: \"socket D60 wider than base 63.67\" in file ok.scad, line 4
TRACE: called by 'assert' in file ok.scad, line 4
Geometries in cache: 1
Current top level object is empty.
";

    const PARSER_STDERR: &str = "\
ERROR: Parser error: syntax error in file syn.scad, line 3
Can't parse file 'syn.scad'!
";

    const EMPTY_STDERR: &str = "\
Geometries in cache: 1
Total rendering time: 0:00:00.000
Current top level object is empty.
";

    #[test]
    fn an_assert_failure_keeps_its_expression_message_and_location() {
        let echoes = crate::echo::parse(ASSERT_STDERR);
        match classify(ASSERT_STDERR, Some(1), &echoes) {
            ScadError::Assertion {
                expression,
                message,
                file,
                line,
                ..
            } => {
                assert_eq!(expression, "((SOCK_OD + (2 * RIM)) <= CUP_BASE)");
                assert_eq!(message, "socket D60 wider than base 63.67");
                assert_eq!(file, "ok.scad");
                assert_eq!(line, 4);
            }
            other => panic!("expected an assertion, got {other:?}"),
        }
    }

    /// The values are the point. OpenSCAD's error text does not contain
    /// them, and reconstructing them from the source by hand is the
    /// guesswork this crate removes.
    #[test]
    fn an_assert_failure_carries_the_echoed_variable_values() {
        let echoes = crate::echo::parse(ASSERT_STDERR);
        match classify(ASSERT_STDERR, Some(1), &echoes) {
            ScadError::Assertion { values, .. } => {
                assert_eq!(values["sock_od"], json!(60.0));
                assert_eq!(values["cup_base"], json!(63.67));
            }
            other => panic!("expected an assertion, got {other:?}"),
        }
    }

    #[test]
    fn an_assert_failure_keeps_the_trace_that_names_the_calling_module() {
        let echoes = crate::echo::parse(ASSERT_STDERR);
        match classify(ASSERT_STDERR, Some(1), &echoes) {
            ScadError::Assertion { trace, .. } => {
                assert_eq!(trace.len(), 1);
                assert!(trace[0].contains("called by 'assert'"));
            }
            other => panic!("expected an assertion, got {other:?}"),
        }
    }

    /// An assert that fires also empties the top level. Reporting that
    /// would name the symptom and lose the cause.
    #[test]
    fn an_assert_failure_is_not_reported_as_an_empty_result() {
        let echoes = crate::echo::parse(ASSERT_STDERR);
        assert!(matches!(
            classify(ASSERT_STDERR, Some(1), &echoes),
            ScadError::Assertion { .. }
        ));
    }

    #[test]
    fn a_bare_assert_with_no_message_still_classifies() {
        let stderr = "ERROR: Assertion '(a > b)' failed in file m.scad, line 12\n";
        match classify(stderr, Some(1), &Echoes::default()) {
            ScadError::Assertion {
                expression,
                message,
                line,
                ..
            } => {
                assert_eq!(expression, "(a > b)");
                assert_eq!(message, "");
                assert_eq!(line, 12);
            }
            other => panic!("expected an assertion, got {other:?}"),
        }
    }

    #[test]
    fn a_syntax_error_is_a_parse_error_with_a_location() {
        match classify(PARSER_STDERR, Some(1), &Echoes::default()) {
            ScadError::Parse { file, line, detail } => {
                assert_eq!(file, "syn.scad");
                assert_eq!(line, 3);
                assert!(detail.contains("syntax error"));
            }
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    #[test]
    fn everything_subtracted_away_is_its_own_error() {
        assert!(matches!(
            classify(EMPTY_STDERR, Some(1), &Echoes::default()),
            ScadError::EmptyResult
        ));
    }

    /// Verbatim from `square(10)` with `-o out.stl`. Worth its own
    /// variant: "exited with code 1" would send a reader looking for a
    /// bug when the fix is one `linear_extrude`.
    #[test]
    fn a_two_dimensional_result_says_so() {
        let stderr = "   Top level object is a 2D object:\n   Contours:        1\n\
                      Current top level object is not a 3D object.\n";
        assert!(matches!(
            classify(stderr, Some(1), &Echoes::default()),
            ScadError::NotThreeDimensional
        ));
    }

    #[test]
    fn an_unrecognised_failure_keeps_the_output_rather_than_swallowing_it() {
        match classify(
            "something we have never seen\n",
            Some(3),
            &Echoes::default(),
        ) {
            ScadError::Failed {
                exit_code,
                stderr_tail,
            } => {
                assert_eq!(exit_code, Some(3));
                assert!(stderr_tail.contains("never seen"));
            }
            other => panic!("expected a generic failure, got {other:?}"),
        }
    }

    #[test]
    fn a_path_containing_spaces_still_parses() {
        let stderr =
            "ERROR: Assertion '(x)' failed: \"no\" in file /tmp/my parts/a b.scad, line 7\n";
        match classify(stderr, Some(1), &Echoes::default()) {
            ScadError::Assertion { file, line, .. } => {
                assert_eq!(file, "/tmp/my parts/a b.scad");
                assert_eq!(line, 7);
            }
            other => panic!("expected an assertion, got {other:?}"),
        }
    }
}
