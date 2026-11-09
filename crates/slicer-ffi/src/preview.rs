//! Scene snapshot sent by the window, and its scaling to G-code thumbnails.
//!
//! The decoder is the pure-Rust `png` crate: libpng is only linked on MSVC
//! builds, and libslic3r's own reader handles greyscale SLA masks only. The
//! scaler lives here too so it can be tested without a libslic3r build; the
//! C++ bridge calls it through `extern "Rust"`, so nothing below may panic.

use std::fmt;

/// Largest snapshot edge accepted, in pixels.
pub const MAX_PREVIEW_DIMENSION: u32 = 2048;

const CHANNELS: usize = 4;

/// A decoded snapshot: RGBA8, first row on top.
#[derive(Clone, PartialEq, Eq)]
pub struct ScenePreview {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl fmt::Debug for ScenePreview {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScenePreview")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewError {
    NotPng(String),
    Empty,
    TooLarge { width: u32, height: u32 },
    WrongLength { expected: usize, got: usize },
}

impl fmt::Display for PreviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPng(why) => write!(f, "preview is not a readable PNG: {why}"),
            Self::Empty => write!(f, "preview has a zero width or height"),
            Self::TooLarge { width, height } => write!(
                f,
                "preview is {width}x{height}, larger than {MAX_PREVIEW_DIMENSION}x{MAX_PREVIEW_DIMENSION}"
            ),
            Self::WrongLength { expected, got } => {
                write!(f, "preview holds {got} bytes, expected {expected}")
            }
        }
    }
}

impl std::error::Error for PreviewError {}

fn check_size(width: u32, height: u32) -> Result<(), PreviewError> {
    if width == 0 || height == 0 {
        return Err(PreviewError::Empty);
    }
    if width > MAX_PREVIEW_DIMENSION || height > MAX_PREVIEW_DIMENSION {
        return Err(PreviewError::TooLarge { width, height });
    }
    Ok(())
}

impl ScenePreview {
    pub fn from_png(png: &[u8]) -> Result<Self, PreviewError> {
        let mut decoder = png::Decoder::new(png);
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder
            .read_info()
            .map_err(|e| PreviewError::NotPng(e.to_string()))?;
        let (width, height) = {
            let info = reader.info();
            (info.width, info.height)
        };
        check_size(width, height)?;

        let mut frame = vec![0u8; reader.output_buffer_size()];
        let info = reader
            .next_frame(&mut frame)
            .map_err(|e| PreviewError::NotPng(e.to_string()))?;
        let data = &frame[..info.buffer_size()];

        let rgba = match info.color_type {
            png::ColorType::Rgba => data.to_vec(),
            png::ColorType::Rgb => data
                .chunks(3)
                .flat_map(|p| [p[0], p[1], p[2], 255])
                .collect(),
            png::ColorType::Grayscale => data.iter().flat_map(|&g| [g, g, g, 255]).collect(),
            png::ColorType::GrayscaleAlpha => data
                .chunks(2)
                .flat_map(|p| [p[0], p[0], p[0], p[1]])
                .collect(),
            // `normalize_to_color8` expands palettes; a decoder that kept one
            // would be a bug worth a refusal, not a panic.
            png::ColorType::Indexed => {
                return Err(PreviewError::NotPng("unexpanded palette".into()))
            }
        };
        Self::from_rgba(width, height, rgba)
    }

    pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self, PreviewError> {
        check_size(width, height)?;
        let expected = width as usize * height as usize * CHANNELS;
        if rgba.len() != expected {
            return Err(PreviewError::WrongLength {
                expected,
                got: rgba.len(),
            });
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// The snapshot scaled to `width` x `height` (centre-cropped to the
    /// target's proportions), rows bottom-up. Empty on a zero size.
    pub fn thumbnail_bottom_up(&self, width: u32, height: u32) -> Vec<u8> {
        cover_scaled_bottom_up(&self.rgba, self.width, self.height, width, height)
    }
}

/// The snapshot as the bridge takes it; `None` is width 0, no snapshot.
pub(crate) fn bridge_preview(
    preview: Option<&ScenePreview>,
) -> crate::ffi_bridge::BridgeScenePreview {
    match preview {
        Some(p) => crate::ffi_bridge::BridgeScenePreview {
            width: p.width,
            height: p.height,
            rgba: p.rgba.clone(),
        },
        None => crate::ffi_bridge::BridgeScenePreview {
            width: 0,
            height: 0,
            rgba: Vec::new(),
        },
    }
}

/// What the bridge's thumbnail callback asks for: the snapshot cover-scaled
/// to `width` x `height`, rows bottom-up. Empty when the snapshot or the size
/// is unusable; never panics.
pub(crate) fn preview_thumbnail_bottom_up(
    preview: &crate::ffi_bridge::BridgeScenePreview,
    width: u32,
    height: u32,
) -> Vec<u8> {
    cover_scaled_bottom_up(&preview.rgba, preview.width, preview.height, width, height)
}

/// Centre-crop `rgba` (`src_width` x `src_height`, top row first) to the
/// proportions of `width` x `height`, area-average it to that size and
/// return the rows bottom-up, as Orca's `ThumbnailData` expects (the GUI
/// fills it from OpenGL and the PNG encoder flips it back).
///
/// Integer maths with exact area fractions, so the result is identical on
/// every platform. Returns an empty vector on a zero size or a buffer that is
/// not `src_width * src_height * 4`; never panics.
pub(crate) fn cover_scaled_bottom_up(
    rgba: &[u8],
    src_width: u32,
    src_height: u32,
    width: u32,
    height: u32,
) -> Vec<u8> {
    if src_width == 0 || src_height == 0 || width == 0 || height == 0 {
        return Vec::new();
    }
    let (sw, sh) = (src_width as usize, src_height as usize);
    let (w, h) = (width as usize, height as usize);
    let src_ok = sw
        .checked_mul(sh)
        .and_then(|n| n.checked_mul(CHANNELS))
        .is_some_and(|n| n == rgba.len());
    let Some(out_len) = w.checked_mul(h).and_then(|n| n.checked_mul(CHANNELS)) else {
        return Vec::new();
    };
    if !src_ok {
        return Vec::new();
    }
    let mut out = Vec::new();
    if out.try_reserve_exact(out_len).is_err() {
        return Vec::new();
    }
    out.resize(out_len, 0u8);

    // Crop window (u64 so the cross products cannot overflow).
    let (sw64, sh64, w64, h64) = (sw as u64, sh as u64, w as u64, h as u64);
    let (cw, ch, x0, y0) = if sw64 * h64 > w64 * sh64 {
        let cw = (sh64 * w64 / h64).max(1);
        (cw, sh64, (sw64 - cw) / 2, 0)
    } else if sw64 * h64 < w64 * sh64 {
        let ch = (sw64 * h64 / w64).max(1);
        (sw64, ch, 0, (sh64 - ch) / 2)
    } else {
        (sw64, sh64, 0, 0)
    };
    let norm = cw * ch;

    // Target cell t covers [t*c, (t+1)*c) in units of 1/target of a source
    // pixel; source pixel s covers [s*target, (s+1)*target).
    let overlap = |lo: u64, hi: u64, s: u64, target: u64| -> u64 {
        let a = lo.max(s * target);
        let b = hi.min((s + 1) * target);
        b.saturating_sub(a)
    };

    for ty in 0..h64 {
        let (ylo, yhi) = (ty * ch, (ty + 1) * ch);
        let (sy_first, sy_last) = (ylo / h64, (yhi - 1) / h64);
        let out_row = (h64 - 1 - ty) as usize;
        for tx in 0..w64 {
            let (xlo, xhi) = (tx * cw, (tx + 1) * cw);
            let (sx_first, sx_last) = (xlo / w64, (xhi - 1) / w64);
            let mut sum = [0u64; CHANNELS];
            for sy in sy_first..=sy_last {
                let wy = overlap(ylo, yhi, sy, h64);
                let row = (y0 + sy) as usize * sw;
                for sx in sx_first..=sx_last {
                    let weight = overlap(xlo, xhi, sx, w64) * wy;
                    let at = (row + (x0 + sx) as usize) * CHANNELS;
                    for (acc, &v) in sum.iter_mut().zip(&rgba[at..at + CHANNELS]) {
                        *acc += u64::from(v) * weight;
                    }
                }
            }
            let at = (out_row * w + tx as usize) * CHANNELS;
            for (dst, acc) in out[at..at + CHANNELS].iter_mut().zip(sum) {
                *dst = ((acc + norm / 2) / norm) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: [u8; 4] = [255, 0, 0, 255];
    const B: [u8; 4] = [0, 255, 0, 255];
    const C: [u8; 4] = [0, 0, 255, 255];
    const D: [u8; 4] = [255, 255, 255, 255];

    fn encode(
        w: u32,
        h: u32,
        color: png::ColorType,
        depth: png::BitDepth,
        palette: Option<&[u8]>,
        data: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(color);
            enc.set_depth(depth);
            if let Some(p) = palette {
                enc.set_palette(p.to_vec());
            }
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(data).unwrap();
        }
        out
    }

    fn rgba8(w: u32, h: u32, data: &[u8]) -> Vec<u8> {
        encode(w, h, png::ColorType::Rgba, png::BitDepth::Eight, None, data)
    }

    #[test]
    fn an_rgba_png_decodes_top_row_first() {
        let png = rgba8(2, 2, &[A, B, C, D].concat());
        let p = ScenePreview::from_png(&png).unwrap();
        assert_eq!((p.width(), p.height()), (2, 2));
        assert_eq!(p.rgba(), [A, B, C, D].concat());
    }

    #[test]
    fn rgb_grey_palette_and_16_bit_pngs_decode_to_rgba() {
        let rgb = encode(
            1,
            1,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            None,
            &[10, 20, 30],
        );
        assert_eq!(
            ScenePreview::from_png(&rgb).unwrap().rgba(),
            [10, 20, 30, 255]
        );

        let grey = encode(
            1,
            1,
            png::ColorType::Grayscale,
            png::BitDepth::Eight,
            None,
            &[77],
        );
        assert_eq!(
            ScenePreview::from_png(&grey).unwrap().rgba(),
            [77, 77, 77, 255]
        );

        let ga = encode(
            1,
            1,
            png::ColorType::GrayscaleAlpha,
            png::BitDepth::Eight,
            None,
            &[77, 128],
        );
        assert_eq!(
            ScenePreview::from_png(&ga).unwrap().rgba(),
            [77, 77, 77, 128]
        );

        let idx = encode(
            1,
            1,
            png::ColorType::Indexed,
            png::BitDepth::Eight,
            Some(&[1, 2, 3]),
            &[0],
        );
        assert_eq!(ScenePreview::from_png(&idx).unwrap().rgba(), [1, 2, 3, 255]);

        let rgb16 = encode(
            1,
            1,
            png::ColorType::Rgb,
            png::BitDepth::Sixteen,
            None,
            &[0xFF, 0xFF, 0x00, 0x00, 0x80, 0x80],
        );
        assert_eq!(
            ScenePreview::from_png(&rgb16).unwrap().rgba(),
            [255, 0, 128, 255]
        );
    }

    #[test]
    fn bytes_that_are_not_a_png_are_refused() {
        assert!(matches!(
            ScenePreview::from_png(b"ABCDEFG"),
            Err(PreviewError::NotPng(_))
        ));
    }

    #[test]
    fn a_png_wider_than_2048_is_refused() {
        let png = rgba8(2049, 1, &vec![0; 2049 * 4]);
        assert_eq!(
            ScenePreview::from_png(&png),
            Err(PreviewError::TooLarge {
                width: 2049,
                height: 1
            })
        );
    }

    #[test]
    fn from_rgba_checks_size_and_length() {
        assert_eq!(
            ScenePreview::from_rgba(0, 1, vec![]),
            Err(PreviewError::Empty)
        );
        assert_eq!(
            ScenePreview::from_rgba(1, 2049, vec![]),
            Err(PreviewError::TooLarge {
                width: 1,
                height: 2049
            })
        );
        assert_eq!(
            ScenePreview::from_rgba(1, 1, vec![0; 3]),
            Err(PreviewError::WrongLength {
                expected: 4,
                got: 3
            })
        );
    }

    #[test]
    fn debug_never_prints_pixels() {
        let p = ScenePreview::from_rgba(1, 1, A.to_vec()).unwrap();
        assert_eq!(
            format!("{p:?}"),
            "ScenePreview { width: 1, height: 1, bytes: 4 }"
        );
    }

    #[test]
    fn the_same_size_only_turns_the_rows_bottom_up() {
        let p = ScenePreview::from_rgba(2, 2, [A, B, C, D].concat()).unwrap();
        assert_eq!(p.thumbnail_bottom_up(2, 2), [C, D, A, B].concat());
    }

    #[test]
    fn halving_averages_each_block_of_four() {
        let px = |r: u8| [r, 0, 0, 255];
        let p = ScenePreview::from_rgba(2, 2, [px(0), px(255), px(255), px(0)].concat()).unwrap();
        assert_eq!(p.thumbnail_bottom_up(1, 1), [128, 0, 0, 255]);
    }

    #[test]
    fn growing_300_to_380_keeps_a_flat_colour_flat() {
        let p = ScenePreview::from_rgba(300, 300, [12, 34, 56, 255].repeat(300 * 300)).unwrap();
        let t = p.thumbnail_bottom_up(380, 380);
        assert_eq!(t.len(), 380 * 380 * 4);
        assert!(t.chunks(4).all(|px| px == [12, 34, 56, 255]));
    }

    #[test]
    fn growing_blends_neighbours_by_area() {
        let p = ScenePreview::from_rgba(2, 1, vec![0, 0, 0, 255, 255, 255, 255, 255]).unwrap();
        let t = p.thumbnail_bottom_up(3, 1);
        let r: Vec<u8> = t.chunks(4).map(|px| px[0]).collect();
        assert_eq!(r, [0, 128, 255]);
    }

    #[test]
    fn a_wide_snapshot_is_cropped_to_its_middle_not_squashed() {
        let p = ScenePreview::from_rgba(4, 1, [A, B, C, D].concat()).unwrap();
        assert_eq!(p.thumbnail_bottom_up(2, 1), [B, C].concat());
    }

    #[test]
    fn a_tall_snapshot_is_cropped_to_its_middle() {
        let p = ScenePreview::from_rgba(1, 4, [A, B, C, D].concat()).unwrap();
        assert_eq!(p.thumbnail_bottom_up(1, 2), [C, B].concat());
    }

    #[test]
    fn a_zero_size_or_a_short_buffer_gives_nothing() {
        assert!(cover_scaled_bottom_up(&[0; 4], 1, 1, 0, 5).is_empty());
        assert!(cover_scaled_bottom_up(&[0; 3], 1, 1, 1, 1).is_empty());
        assert!(cover_scaled_bottom_up(&[], 0, 1, 1, 1).is_empty());
    }

    #[test]
    fn no_snapshot_crosses_the_bridge_as_width_zero() {
        let none = bridge_preview(None);
        assert_eq!((none.width, none.height, none.rgba.len()), (0, 0, 0));
        let p = ScenePreview::from_rgba(1, 1, vec![1, 2, 3, 4]).unwrap();
        let some = bridge_preview(Some(&p));
        assert_eq!(
            (some.width, some.height, some.rgba),
            (1, 1, vec![1, 2, 3, 4])
        );
    }
}
