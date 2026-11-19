//! Validation common to the paint RPCs and saved project files.
use base64::{engine::general_purpose::STANDARD, Engine as _};

const MAX_FACETS_RLE_BYTES: usize = 8 * 1024 * 1024;

fn decode(rle: &str) -> Result<Vec<u8>, String> {
    if rle.len() > MAX_FACETS_RLE_BYTES {
        return Err(format!("facets_rle exceeds {MAX_FACETS_RLE_BYTES} bytes"));
    }
    STANDARD
        .decode(rle)
        .map_err(|_| "facets_rle is not valid base64".into())
}

pub fn validate_facets_rle(rle: Option<&str>) -> Result<(), String> {
    rle.map(decode).transpose().map(|_| ())
}

pub fn validate_paint_runs(rle: Option<&str>, kind: &str, max_value: u8) -> Result<(), String> {
    let Some(rle) = rle else {
        return Ok(());
    };
    let data = decode(rle)?;
    let (runs, remainder) = data.as_chunks::<5>();
    if !remainder.is_empty() {
        return Err(format!("{kind} paint requires 5-byte RLE runs"));
    }
    let mut total = 0u64;
    for &[value, a, b, c, d] in runs {
        let count = u32::from_le_bytes([a, b, c, d]);
        total += u64::from(count);
        if value > max_value || count == 0 || total > 20_000_000 {
            return Err(format!("invalid {kind} paint value, run or facet count"));
        }
    }
    Ok(())
}
