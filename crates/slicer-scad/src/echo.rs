//! Turn OpenSCAD's `ECHO:` lines into data.
//!
//! A parametric model computes things the caller needs — a clearance, a
//! resolved wall thickness, a chosen count — and the only way it can
//! hand them out is `echo()`. Leaving those in the log means every
//! caller writes the same fragile regex, so they are parsed once, here.
//!
//! Formats, taken from OpenSCAD 2021.01 output rather than from the
//! manual:
//!
//! ```text
//! ECHO: sock_od = 55, clearance = 4.67
//! ECHO: "plain string echo"
//! ECHO: vec = [1, 2, 3], flag = true
//! ECHO: x = inf
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Echoes {
    /// `echo(name = value)` pairs. A name echoed twice keeps the last
    /// value, matching the order the model ran in.
    pub values: BTreeMap<String, Value>,
    /// `echo("text")` and any other positional argument, in order.
    pub messages: Vec<String>,
}

impl Echoes {
    pub fn is_empty(&self) -> bool {
        self.values.is_empty() && self.messages.is_empty()
    }
}

/// Collect every `ECHO:` line out of a captured stderr.
pub fn parse(stderr: &str) -> Echoes {
    let mut out = Echoes::default();
    for line in stderr.lines() {
        let Some(body) = line.trim().strip_prefix("ECHO:") else {
            continue;
        };
        for item in split_top_level(body.trim(), ',') {
            match split_once_top_level(&item, '=') {
                Some((name, raw)) => {
                    out.values
                        .insert(name.trim().to_string(), parse_value(raw.trim()));
                }
                None => out.messages.push(unquote(item.trim()).to_string()),
            }
        }
    }
    out
}

fn parse_value(raw: &str) -> Value {
    if raw == "true" {
        return Value::Bool(true);
    }
    if raw == "false" {
        return Value::Bool(false);
    }
    if raw == "undef" {
        return Value::Null;
    }
    if let Some(inner) = raw.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        return Value::Array(
            split_top_level(inner, ',')
                .iter()
                .map(|item| parse_value(item.trim()))
                .collect(),
        );
    }
    if raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2 {
        return Value::String(unquote(raw).to_string());
    }
    // `inf` and `nan` are values OpenSCAD prints and JSON cannot hold.
    // Keeping them as strings makes them visibly not-a-number to a
    // caller, which is better than the alternatives — dropping them, or
    // coercing them to something arithmetic would silently accept.
    if let Ok(number) = raw.parse::<f64>() {
        if number.is_finite() {
            if let Some(n) = serde_json::Number::from_f64(number) {
                return Value::Number(n);
            }
        }
        return Value::String(raw.to_string());
    }
    Value::String(raw.to_string())
}

fn unquote(text: &str) -> &str {
    text.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(text)
}

/// Split on `separator`, ignoring separators inside `[]` and `""`.
/// A naive split tears `[1, 2, 3]` into three useless fragments.
fn split_top_level(text: &str, separator: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut in_string = false;
    for ch in text.chars() {
        match ch {
            '"' => {
                in_string = !in_string;
                current.push(ch);
            }
            '[' | '(' if !in_string => {
                depth += 1;
                current.push(ch);
            }
            ']' | ')' if !in_string => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            c if c == separator && depth == 0 && !in_string => {
                out.push(std::mem::take(&mut current));
            }
            c => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

fn split_once_top_level(text: &str, separator: char) -> Option<(String, String)> {
    let mut depth = 0usize;
    let mut in_string = false;
    for (index, ch) in text.char_indices() {
        match ch {
            '"' => in_string = !in_string,
            '[' | '(' if !in_string => depth += 1,
            ']' | ')' if !in_string => depth = depth.saturating_sub(1),
            c if c == separator && depth == 0 && !in_string => {
                return Some((text[..index].to_string(), text[index + 1..].to_string()));
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Verbatim stderr from OpenSCAD 2021.01, so these tests are about
    /// the format the tool actually emits rather than the one the manual
    /// describes.
    const REAL_STDERR: &str = "\
ECHO: sock_od = 55, clearance = 4.67
ECHO: \"plain string echo\"
ECHO: vec = [1, 2, 3], flag = true
Geometries in cache: 1
Geometry cache size in bytes: 728
Total rendering time: 0:00:00.000
";

    #[test]
    fn named_values_come_back_as_data() {
        let echoes = parse(REAL_STDERR);
        assert_eq!(echoes.values["sock_od"], json!(55.0));
        assert_eq!(echoes.values["clearance"], json!(4.67));
        assert_eq!(echoes.values["flag"], json!(true));
    }

    #[test]
    fn a_vector_survives_as_a_vector_rather_than_three_fragments() {
        let echoes = parse(REAL_STDERR);
        assert_eq!(echoes.values["vec"], json!([1.0, 2.0, 3.0]));
    }

    #[test]
    fn a_bare_string_echo_is_a_message_not_a_value() {
        let echoes = parse(REAL_STDERR);
        assert_eq!(echoes.messages, vec!["plain string echo"]);
    }

    #[test]
    fn the_noise_lines_are_not_mistaken_for_echoes() {
        let echoes = parse(REAL_STDERR);
        assert_eq!(echoes.values.len(), 4);
        assert!(!echoes.values.contains_key("Geometries in cache"));
    }

    #[test]
    fn a_non_finite_value_is_kept_visibly_non_numeric() {
        // OpenSCAD prints `inf` for 1/0. JSON has no such number, and
        // turning it into one a caller can do arithmetic on would be the
        // worst of the options.
        let echoes = parse("ECHO: x = inf\nECHO: y = nan\n");
        assert_eq!(echoes.values["x"], json!("inf"));
        assert_eq!(echoes.values["y"], json!("nan"));
    }

    #[test]
    fn undef_is_null() {
        assert_eq!(parse("ECHO: a = undef").values["a"], json!(null));
    }

    #[test]
    fn a_string_value_containing_a_comma_is_not_split() {
        let echoes = parse("ECHO: note = \"wide, by 0.33mm\", n = 2");
        assert_eq!(echoes.values["note"], json!("wide, by 0.33mm"));
        assert_eq!(echoes.values["n"], json!(2.0));
    }

    #[test]
    fn nested_vectors_survive() {
        let echoes = parse("ECHO: pts = [[0, 0], [1, 2]]");
        assert_eq!(echoes.values["pts"], json!([[0.0, 0.0], [1.0, 2.0]]));
    }

    #[test]
    fn the_last_value_of_a_repeated_name_wins() {
        let echoes = parse("ECHO: w = 1\nECHO: w = 9");
        assert_eq!(echoes.values["w"], json!(9.0));
    }

    #[test]
    fn stderr_with_no_echoes_yields_nothing_rather_than_noise() {
        assert!(parse("Total rendering time: 0:00:00.000\n").is_empty());
    }
}
