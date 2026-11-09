//! The preview PNG that `project.export_gcode` embeds as a thumbnail block.
//!
//! The block goes right after the `; HEADER_BLOCK_END` line of the file and
//! is never added when the slicer already wrote its own thumbnails.

use base64::Engine as _;
use slicer_ffi::{ScenePreview, MAX_PREVIEW_DIMENSION};

pub(crate) const GCODE_THUMBNAIL_MAX_BASE64_BYTES: usize = 5 * 1024 * 1024;
/// How far into the file `embed_thumbnail_block` looks for the header end.
pub(crate) const HEADER_SCAN_BYTES: usize = 64 * 1024;
const GCODE_THUMBNAIL_LINE_WIDTH: usize = 78;

const HEADER_END_LINE: &[u8] = b"; HEADER_BLOCK_END";
const THUMBNAIL_MARKERS: [&[u8]; 2] = [b"; THUMBNAIL_BLOCK_START", b";gimage:"];

pub(crate) struct PreviewPayload {
    /// Normalised base64: data-URL prefix stripped, whitespace removed.
    pub base64: String,
    pub width: u32,
    pub height: u32,
}

/// Validates the preview parameters of `project.export_gcode`. An absent,
/// empty or empty-data-URL preview is `Ok(None)`.
pub(crate) fn preview_payload(
    base64: Option<&str>,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<Option<PreviewPayload>, String> {
    let Some(raw_base64) = base64 else {
        return Ok(None);
    };
    let trimmed = raw_base64.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let payload = trimmed
        .strip_prefix("data:image/png;base64,")
        .unwrap_or(trimmed);
    let normalized: String = payload.chars().filter(|ch| !ch.is_whitespace()).collect();
    if normalized.is_empty() {
        return Ok(None);
    }
    if normalized.len() > GCODE_THUMBNAIL_MAX_BASE64_BYTES {
        return Err(format!(
            "preview_png_base64 is too large: {} bytes",
            normalized.len()
        ));
    }
    if !normalized
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
    {
        return Err("preview_png_base64 contains non-base64 characters".to_string());
    }
    let width = width
        .ok_or_else(|| "preview_width is required when preview_png_base64 is set".to_string())?;
    let height = height
        .ok_or_else(|| "preview_height is required when preview_png_base64 is set".to_string())?;
    if width == 0 || height == 0 || width > MAX_PREVIEW_DIMENSION || height > MAX_PREVIEW_DIMENSION
    {
        return Err(format!(
            "preview dimensions must be between 1 and {MAX_PREVIEW_DIMENSION}: {width}x{height}"
        ));
    }
    Ok(Some(PreviewPayload {
        base64: normalized,
        width,
        height,
    }))
}

/// Decodes a validated payload into the snapshot the bridge makes thumbnails
/// from. The declared size must be the PNG's own.
pub(crate) fn decode_scene_preview(payload: &PreviewPayload) -> Result<ScenePreview, String> {
    let png = base64::engine::general_purpose::STANDARD
        .decode(&payload.base64)
        .map_err(|e| format!("preview_png_base64 is not valid base64: {e}"))?;
    let preview = ScenePreview::from_png(&png).map_err(|e| e.to_string())?;
    if preview.width() != payload.width || preview.height() != payload.height {
        return Err(format!(
            "preview_width/preview_height {}x{} disagree with the PNG, which is {}x{}",
            payload.width,
            payload.height,
            preview.width(),
            preview.height()
        ));
    }
    Ok(preview)
}

/// The slicer-compatible thumbnail comment block for a validated preview.
pub(crate) fn thumbnail_block(payload: &PreviewPayload) -> String {
    let mut block = String::new();
    block.push_str("; THUMBNAIL_BLOCK_START\n");
    block.push_str(";\n");
    block.push_str(&format!(
        "; thumbnail begin {}x{} {}\n",
        payload.width,
        payload.height,
        payload.base64.len()
    ));
    // The payload is ASCII (checked in `preview_payload`), so chunking by
    // bytes never splits a character.
    for chunk in payload.base64.as_bytes().chunks(GCODE_THUMBNAIL_LINE_WIDTH) {
        block.push_str("; ");
        block.push_str(&String::from_utf8_lossy(chunk));
        block.push('\n');
    }
    block.push_str("; thumbnail end\n");
    block.push_str("; THUMBNAIL_BLOCK_END\n\n");
    block
}

/// Puts `block` right after the header block of `gcode`. `None` means the
/// file already carries thumbnails and must be copied as it is.
pub(crate) fn embed_thumbnail_block(gcode: &[u8], block: &str) -> Option<Vec<u8>> {
    let insert_at = match header_end(gcode) {
        Some(at) => at,
        None => return Some([block.as_bytes(), gcode].concat()),
    };
    let rest = &gcode[insert_at..];
    let skipped = rest
        .iter()
        .take_while(|byte| byte.is_ascii_whitespace())
        .count();
    let after = &rest[skipped..];
    if THUMBNAIL_MARKERS.iter().any(|m| after.starts_with(m)) {
        return None;
    }
    let mut out = Vec::with_capacity(gcode.len() + block.len());
    out.extend_from_slice(&gcode[..insert_at]);
    out.extend_from_slice(block.as_bytes());
    out.extend_from_slice(rest);
    Some(out)
}

/// Offset just past the `; HEADER_BLOCK_END` line (and the blank line Orca
/// writes after it), looking only at the first `HEADER_SCAN_BYTES`.
fn header_end(gcode: &[u8]) -> Option<usize> {
    let scan = &gcode[..gcode.len().min(HEADER_SCAN_BYTES)];
    let mut start = 0;
    while start < scan.len() {
        let line_end = scan[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(scan.len(), |p| start + p);
        let line = scan[start..line_end]
            .strip_suffix(b"\r")
            .unwrap_or(&scan[start..line_end]);
        if line == HEADER_END_LINE {
            let mut at = (line_end + 1).min(gcode.len());
            if gcode.get(at) == Some(&b'\n') {
                at += 1;
            }
            return Some(at);
        }
        start = line_end + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "; HEADER_BLOCK_START\n; generated by OrcaSlicer 2.3.1 on 2026-11-07 at 10:00:00\n; Protype Slicer 0.1.38\n; HEADER_BLOCK_END\n\n";
    const BODY: &str = "; external perimeters extrusion width = 0.45mm\nG28\n";
    const BLOCK: &str = "; THUMBNAIL_BLOCK_START\n;\n; thumbnail begin 1x1 4\n; QUJD\n; thumbnail end\n; THUMBNAIL_BLOCK_END\n\n";
    fn embed(gcode: &str) -> Option<String> {
        embed_thumbnail_block(gcode.as_bytes(), BLOCK).map(|b| String::from_utf8(b).unwrap())
    }

    #[test]
    fn the_thumbnail_goes_right_after_the_header_block() {
        assert_eq!(
            embed(&format!("{HEADER}{BODY}")).unwrap(),
            format!("{HEADER}{BLOCK}{BODY}")
        );
    }
    #[test]
    fn a_file_with_orca_thumbnails_is_left_as_it_is() {
        for existing in [BLOCK, "\n\n;gimage:AAAA\n\n"] {
            assert_eq!(embed(&format!("{HEADER}{existing}{BODY}")), None);
        }
    }
    #[test]
    fn the_passport_line_stays_above_the_header() {
        let passport = "; PROTYPE_PRINT_RUN:01234567-0123-4123-8123-0123456789ab\n";
        assert_eq!(
            embed(&format!("{passport}{HEADER}{BODY}")).unwrap(),
            format!("{passport}{HEADER}{BLOCK}{BODY}")
        );
    }
    #[test]
    fn a_file_without_a_header_block_gets_the_thumbnail_first() {
        assert_eq!(
            embed("; Protype Slicer FFI G-code\nG28\n").unwrap(),
            format!("{BLOCK}; Protype Slicer FFI G-code\nG28\n")
        );
    }
    #[test]
    fn the_block_is_what_export_wrote_before() {
        let p = PreviewPayload {
            base64: "QUJDREVGRw==".into(),
            width: 300,
            height: 300,
        };
        assert_eq!(thumbnail_block(&p), "; THUMBNAIL_BLOCK_START\n;\n; thumbnail begin 300x300 12\n; QUJDREVGRw==\n; thumbnail end\n; THUMBNAIL_BLOCK_END\n\n");
    }
}
