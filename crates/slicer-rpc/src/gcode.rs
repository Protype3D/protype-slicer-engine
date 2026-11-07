//! `gcode.*` — post-processing as a pipeline step.
//!
//! Each arm reads a G-code file, applies one pass, and writes the result
//! next to it (or wherever the caller says). Writing goes through a
//! temporary and a rename, for the same reason `slicer-scad` does it: a
//! caller must never be handed a path holding half a transform, and a
//! failed pass must never leave the previous attempt's file behind.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use serde::Deserialize;
use serde_json::{json, Value};
use slicer_gcode::{fan_lead, layers, resume, slot, FanLeadParams};
use slicer_machine::hazard::{Part, DEFAULT_MARGIN_MM};

use crate::error::RpcError;

#[derive(Debug, Deserialize)]
pub struct AnnotateParams {
    pub gcode_path: String,
    #[serde(default)]
    pub output_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SelectToolParams {
    pub gcode_path: String,
    pub machine_id: String,
    pub tool: String,
    #[serde(default)]
    pub output_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResumeParams {
    pub gcode_path: String,
    pub machine_id: String,
    pub from_layer: usize,
    #[serde(default)]
    pub parts_on_bed: Vec<PartParams>,
    pub z_reference: resume::ZReference,
    #[serde(default)]
    pub safe_z_mm: Option<f64>,
    #[serde(default)]
    pub margin_mm: Option<f64>,
    #[serde(default)]
    pub output_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PartParams {
    pub name: String,
    pub min: [f64; 3],
    pub max: [f64; 3],
}

pub fn handle_annotate_layers(params: Value) -> Result<Value, RpcError> {
    let p: AnnotateParams = parse(params)?;
    let source = read(&p.gcode_path)?;
    let result = layers::annotate_layers(&source).map_err(project)?;
    let out = write_beside(
        &p.gcode_path,
        p.output_path.as_deref(),
        "layers",
        &result.gcode,
    )?;

    Ok(json!({
        "output_path": out,
        "layers": result.layers,
        "inserted": result.inserted,
        "already_present": result.already_present,
        // Empty is the expected answer. Anything here means our count and
        // the slicer's own `M73 L` disagree, and neither should be acted
        // on until that is settled.
        "disagreements": result.disagreements,
    }))
}

pub fn handle_select_tool(params: Value) -> Result<Value, RpcError> {
    let p: SelectToolParams = parse(params)?;
    let passport = slicer_machine::bundled(&p.machine_id)
        .map_err(|e| RpcError::Project(format!("machine passport: {e}")))?;
    let source = read(&p.gcode_path)?;
    let result = slot::select_tool(&source, &passport, &p.tool).map_err(project)?;
    let out = write_beside(
        &p.gcode_path,
        p.output_path.as_deref(),
        &p.tool,
        &result.gcode,
    )?;

    Ok(json!({
        "output_path": out,
        "tool": result.tool,
        "tool_index": result.tool_index,
        "slot_var": result.slot_var,
        "box_print_start_rewrites": result.box_print_start_rewrites,
        "tool_select_rewrites": result.tool_select_rewrites,
        "unload_filament_rewrites": result.unload_filament_rewrites,
        "missing_mandatory_lines": result.missing_mandatory_lines,
    }))
}

pub fn handle_plan_resume(params: Value) -> Result<Value, RpcError> {
    let p: ResumeParams = parse(params)?;
    let passport = slicer_machine::bundled(&p.machine_id)
        .map_err(|e| RpcError::Project(format!("machine passport: {e}")))?;
    let source = read(&p.gcode_path)?;
    let parts: Vec<Part> = p
        .parts_on_bed
        .iter()
        .map(|part| Part::from_bounds(part.name.clone(), part.min, part.max))
        .collect();

    let plan = resume::plan_resume(&resume::ResumeRequest {
        gcode: &source,
        passport: &passport,
        from_layer: p.from_layer,
        parts_on_bed: parts,
        z_reference: p.z_reference,
        safe_z_mm: p.safe_z_mm,
        margin_mm: p.margin_mm.unwrap_or(DEFAULT_MARGIN_MM),
    })
    .map_err(project)?;

    let out = write_beside(
        &p.gcode_path,
        p.output_path.as_deref(),
        &format!("resume-{}", p.from_layer),
        &plan.gcode,
    )?;

    Ok(json!({
        "output_path": out,
        "from_layer": plan.from_layer,
        "total_layers": plan.total_layers,
        "layer_z": plan.layer_z,
        "safe_z_mm": plan.safe_z_mm,
        "approach": plan.approach,
        // Which reading of the body's `E` numbers the generated block
        // declared — read out of the source file, never assumed.
        "extrusion_mode": plan.extrusion_mode,
        "dropped_wipe_lines": plan.dropped_wipe_lines,
        // A plan that comes back never contains a proven collision — those
        // are an error. These are what could not be settled either way,
        // and a caller is expected to read them before printing.
        "unresolved": plan.unresolved,
    }))
}

fn parse<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::Project(format!("invalid params: {e}")))
}

fn project(e: slicer_gcode::GcodeError) -> RpcError {
    // These are all things a caller acts on — a layer that does not
    // exist, a tool the machine does not have, a plan that would hit
    // something — so they keep their own words.
    RpcError::Project(e.to_string())
}

fn read(path: &str) -> Result<String, RpcError> {
    std::fs::read_to_string(path).map_err(|e| RpcError::Project(format!("cannot read {path}: {e}")))
}

/// Write through a temporary and rename, so the destination holds this
/// pass's output or nothing.
fn write_beside(
    source: &str,
    requested: Option<&str>,
    suffix: &str,
    content: &str,
) -> Result<String, RpcError> {
    let destination = match requested {
        Some(path) => std::path::PathBuf::from(path),
        None => {
            let source = std::path::Path::new(source);
            let stem = source.file_stem().unwrap_or_default().to_string_lossy();
            source.with_file_name(format!("{stem}.{suffix}.gcode"))
        }
    };
    let temporary = destination.with_extension(format!("partial-{}", std::process::id()));
    std::fs::write(&temporary, content)
        .map_err(|e| RpcError::Internal(format!("cannot write {}: {e}", temporary.display())))?;
    std::fs::rename(&temporary, &destination).map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        RpcError::Internal(format!("cannot write {}: {e}", destination.display()))
    })?;
    Ok(destination.to_string_lossy().into_owned())
}

/// What [`apply_fan_lead_to_file`] did to the file.
#[derive(Debug, Clone, PartialEq)]
pub enum FanLeadReport {
    /// No `; PAINT_FAN` line: not a byte was written.
    Unchanged,
    Applied {
        shifted: usize,
        merged: usize,
    },
    /// The pass could not read the file; only the diagnostic line was added.
    Skipped {
        reason: String,
    },
}

/// Run the paint fan lead pass over a finished G-code file, in place.
///
/// Both passes stream the file, so a 280 MB slice costs the lead window and
/// the edit map, not the file. The rewrite goes through a temporary and a
/// rename like every other pass here. `Err` means the file could not be
/// opened or rewritten; the original is then left as it was.
pub fn apply_fan_lead_to_file(
    path: &Path,
    params: FanLeadParams,
) -> Result<FanLeadReport, RpcError> {
    let plan = match fan_lead::plan(open_buffered(path)?, params) {
        Ok(plan) => plan,
        Err(error) => {
            let reason = error.to_string();
            append_line(path, &fan_lead::skipped_diagnostic(&reason))?;
            return Ok(FanLeadReport::Skipped { reason });
        }
    };
    if plan.markers == 0 {
        return Ok(FanLeadReport::Unchanged);
    }

    let temporary = path.with_extension(format!("fan-lead-{}", std::process::id()));
    let written = File::create(&temporary).and_then(|file| {
        let mut writer = BufWriter::new(file);
        plan.write(BufReader::new(File::open(path)?), &mut writer)
    });
    if let Err(e) = written.and_then(|()| std::fs::rename(&temporary, path)) {
        let _ = std::fs::remove_file(&temporary);
        return Err(RpcError::Internal(format!(
            "cannot rewrite {} with the fan lead: {e}",
            path.display()
        )));
    }
    Ok(FanLeadReport::Applied {
        shifted: plan.shifted,
        merged: plan.merged,
    })
}

fn open_buffered(path: &Path) -> Result<BufReader<File>, RpcError> {
    File::open(path)
        .map(BufReader::new)
        .map_err(|e| RpcError::Project(format!("cannot read {}: {e}", path.display())))
}

/// Append `line` on a line of its own, in the file's own line-ending style.
///
/// The style is `\r\n` if any of the file's lines already uses it, `\n`
/// otherwise — the same sticky rule [`fan_lead::LineTerminator`] applies to
/// the in-memory pass and to a successful [`FanLeadPlan::write`], so a CRLF
/// file does not end up with one bare `\n` here either; this reuses that
/// one type instead of guessing again from the last byte.
fn append_line(path: &Path, line: &str) -> Result<(), RpcError> {
    let fail =
        |e: std::io::Error| RpcError::Internal(format!("cannot append to {}: {e}", path.display()));
    let (terminator, ends_with_newline) = file_terminator(path).map_err(fail)?;
    let mut file = OpenOptions::new().append(true).open(path).map_err(fail)?;
    let mut text = String::with_capacity(line.len() + 4);
    if !ends_with_newline {
        text.push_str(terminator.as_str());
    }
    text.push_str(line);
    text.push_str(terminator.as_str());
    file.write_all(text.as_bytes()).map_err(fail)
}

/// Stream the file once to learn its line-ending style and whether it
/// already ends with one. An empty file counts as already ended, so
/// nothing is inserted ahead of the diagnostic.
fn file_terminator(path: &Path) -> std::io::Result<(fan_lead::LineTerminator, bool)> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut terminator = fan_lead::LineTerminator::default();
    let mut ends_with_newline = true;
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer)? == 0 {
            break;
        }
        terminator.saw_line(&buffer);
        ends_with_newline = buffer.ends_with(b"\n");
    }
    Ok((terminator, ends_with_newline))
}
