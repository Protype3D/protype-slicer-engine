//! The four checks.
//!
//! Each one measures the mesh — never the parameters a mesh was built
//! from — and each returns a [`Verdict`] whose `proven_falsifiable` says
//! whether this check has been watched failing in this session.

use serde::{Deserialize, Serialize};
use slicer_geometry::mesh::TriangleMesh;
use slicer_geometry::section::{section_at_z, Point2};
use slicer_geometry::{measure, MeshError};

use crate::{Check, Verdict};

/// Z step for scans that sample. Finer than any layer height, so an
/// overlap or an obstruction thinner than this is thinner than anything
/// the printer could express anyway.
pub const DEFAULT_STEP_MM: f64 = 0.2;

// ------------------------------------------------------------------ bore

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoreResult {
    pub need_diameter_mm: f64,
    pub min_diameter_mm: f64,
    pub at_z: f64,
    pub shortfall_mm: f64,
    pub samples: usize,
    /// Heights at which the axis was not surrounded by material.
    ///
    /// A bore that is open on one side is not a bore, and an axis beside
    /// the part is not in one at all — both measure a large, confident
    /// number. Without this the check passes on an axis aimed at nothing,
    /// which it did, on a real part, on the first end-to-end run.
    pub unenclosed_at_z: Vec<f64>,
}

/// Is the bore about `axis` clear to `need_diameter_mm` over the whole
/// depth?
///
/// Over the whole depth, not at one height: measured at a single Z, the
/// bore that a crossbar was poking into read exactly as designed.
pub fn bore(
    mesh: &TriangleMesh,
    axis: Point2,
    z_from: f64,
    z_to: f64,
    need_diameter_mm: f64,
    step_mm: f64,
    rays: usize,
) -> Result<Verdict, MeshError> {
    let samples = measure::free_circle_over(mesh, axis, z_from, z_to, step_mm, rays)?;
    let tightest = samples
        .iter()
        .min_by(|a, b| a.diameter_mm.total_cmp(&b.diameter_mm))
        .ok_or(MeshError::NoSection {
            z: z_from,
            min_z: z_from,
            max_z: z_to,
        })?;

    let unenclosed_at_z: Vec<f64> = samples
        .iter()
        .filter(|s| !s.enclosed)
        .map(|s| s.z)
        .collect();

    let result = BoreResult {
        need_diameter_mm,
        min_diameter_mm: tightest.diameter_mm,
        at_z: tightest.z,
        shortfall_mm: (need_diameter_mm - tightest.diameter_mm).max(0.0),
        samples: samples.len(),
        unenclosed_at_z,
    };

    // An unenclosed sample is not a narrow bore, it is not a bore. Answer
    // that instead of a diameter, and answer it first: a wrongly aimed
    // axis produces a *large* diameter, so the size test would pass.
    if !result.unenclosed_at_z.is_empty() {
        let summary = format!(
            "not a closed bore: at {} of {} heights (first Z{:.2}) the axis at ({:.2}, {:.2})              is not surrounded by material. The Ø{:.2} measured here is the distance to              whatever happened to be nearest, not a bore",
            result.unenclosed_at_z.len(),
            result.samples,
            result.unenclosed_at_z[0],
            axis[0],
            axis[1],
            result.min_diameter_mm
        );
        return Ok(Verdict::new(Check::Bore, false, summary, &result));
    }

    let ok = tightest.diameter_mm >= need_diameter_mm;
    let summary = if ok {
        format!(
            "clear to Ø{:.2} everywhere over Z{z_from}..{z_to} ({} samples); needed Ø{need_diameter_mm:.2}",
            result.min_diameter_mm, result.samples
        )
    } else {
        format!(
            "pinched to Ø{:.2} at Z{:.2}, short of Ø{need_diameter_mm:.2} by {:.2} mm",
            result.min_diameter_mm, result.at_z, result.shortfall_mm
        )
    };
    Ok(Verdict::new(Check::Bore, ok, summary, &result))
}

// ------------------------------------------------------------------- fit

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FitResult {
    pub hole_min_diameter_mm: f64,
    pub hole_tight_at_z: f64,
    pub peg_max_diameter_mm: f64,
    pub peg_wide_at_z: f64,
    pub clearance_mm: f64,
    pub wanted_clearance_mm: f64,
}

/// Will the peg pass into the hole, and with how much to spare?
///
/// The narrowest the hole gets anywhere against the widest the peg gets
/// anywhere. Both extremes, because a fit is decided by the worst pair of
/// heights and not by the nominal numbers at one.
#[allow(clippy::too_many_arguments)]
pub fn fit(
    hole: &TriangleMesh,
    hole_axis: Point2,
    hole_z: (f64, f64),
    peg: &TriangleMesh,
    peg_axis: Point2,
    peg_z: (f64, f64),
    wanted_clearance_mm: f64,
    step_mm: f64,
    rays: usize,
) -> Result<Verdict, MeshError> {
    let bores = measure::free_circle_over(hole, hole_axis, hole_z.0, hole_z.1, step_mm, rays)?;
    let tightest = bores
        .iter()
        .min_by(|a, b| a.diameter_mm.total_cmp(&b.diameter_mm))
        .ok_or(MeshError::NoSection {
            z: hole_z.0,
            min_z: hole_z.0,
            max_z: hole_z.1,
        })?;

    let profile = measure::profile(peg, peg_axis, peg_z.0, peg_z.1, step_mm)?;
    let widest = profile
        .iter()
        .filter_map(|s| s.max_reach_mm.map(|r| (r, s.z)))
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .ok_or(MeshError::NoSection {
            z: peg_z.0,
            min_z: peg_z.0,
            max_z: peg_z.1,
        })?;

    let result = FitResult {
        hole_min_diameter_mm: tightest.diameter_mm,
        hole_tight_at_z: tightest.z,
        peg_max_diameter_mm: widest.0 * 2.0,
        peg_wide_at_z: widest.1,
        clearance_mm: tightest.diameter_mm - widest.0 * 2.0,
        wanted_clearance_mm,
    };
    let ok = result.clearance_mm >= wanted_clearance_mm;
    let summary = format!(
        "hole Ø{:.2} at its tightest (Z{:.2}) against peg Ø{:.2} at its widest (Z{:.2}) — \
         {:+.2} mm of clearance, wanted {wanted_clearance_mm:+.2}",
        result.hole_min_diameter_mm,
        result.hole_tight_at_z,
        result.peg_max_diameter_mm,
        result.peg_wide_at_z,
        result.clearance_mm
    );
    Ok(Verdict::new(Check::Fit, ok, summary, &result))
}

// ---------------------------------------------------------- interference

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Overlap {
    pub a: String,
    pub b: String,
    pub first_at_z: f64,
    pub worst_at_z: f64,
    /// Estimated overlapping volume. See [`InterferenceResult`] for what
    /// "estimated" means here.
    pub estimated_volume_mm3: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterferenceResult {
    pub pairs_checked: usize,
    pub overlaps: Vec<Overlap>,
    /// Z sampling step. Detection is exact within each sampled plane; a
    /// contact thinner than this in Z can be missed. Reported rather than
    /// buried, because it is the one way this check can be wrong.
    pub z_resolution_mm: f64,
    /// XY grid used only for the *volume estimate*. Detection does not
    /// depend on it.
    pub xy_samples_per_axis: u32,
}

const XY_SAMPLES: u32 = 64;

/// Do any of these parts occupy the same space?
pub fn interference(parts: &[(String, TriangleMesh)], step_mm: f64) -> Result<Verdict, MeshError> {
    // Same guard as every other scan in `measure`, and for the same
    // reason: a non-positive step makes `(hi - lo) / step` infinite, the
    // loop bound saturates to `i64::MAX`, and the sampling loop grinds
    // on one Z forever instead of returning.
    if !(step_mm.is_finite() && step_mm > 0.0) {
        return Err(MeshError::BadStep(step_mm));
    }
    let mut overlaps: Vec<Overlap> = Vec::new();
    let mut pairs = 0usize;

    for (i, (name_a, a)) in parts.iter().enumerate() {
        for (name_b, b) in parts.iter().skip(i + 1) {
            pairs += 1;
            let (Some((a_lo, a_hi)), Some((b_lo, b_hi))) = (a.bounds(), b.bounds()) else {
                continue;
            };
            // Cheap reject before any sectioning.
            if a_hi[0] < b_lo[0]
                || b_hi[0] < a_lo[0]
                || a_hi[1] < b_lo[1]
                || b_hi[1] < a_lo[1]
                || a_hi[2] < b_lo[2]
                || b_hi[2] < a_lo[2]
            {
                continue;
            }
            let lo = a_lo[2].max(b_lo[2]);
            let hi = a_hi[2].min(b_hi[2]);
            let box_lo = [a_lo[0].max(b_lo[0]), a_lo[1].max(b_lo[1])];
            let box_hi = [a_hi[0].min(b_hi[0]), a_hi[1].min(b_hi[1])];

            let mut first_at = None;
            let mut worst_at = lo;
            let mut worst_area = 0.0f64;
            let mut volume = 0.0f64;

            let steps = ((hi - lo) / step_mm).floor().max(0.0) as i64;
            for index in 0..=steps {
                let z = lo + step_mm * index as f64;
                let (Ok(sa), Ok(sb)) = (section_at_z(a, z), section_at_z(b, z)) else {
                    continue;
                };
                let (segments_a, segments_b) = (sa.segments(), sb.segments());
                if segments_a.is_empty() || segments_b.is_empty() {
                    continue;
                }
                let area = overlap_area(&segments_a, &segments_b, box_lo, box_hi);
                if area <= 0.0 {
                    continue;
                }
                first_at.get_or_insert(z);
                volume += area * step_mm;
                if area > worst_area {
                    worst_area = area;
                    worst_at = z;
                }
            }

            if let Some(first_at_z) = first_at {
                overlaps.push(Overlap {
                    a: name_a.clone(),
                    b: name_b.clone(),
                    first_at_z,
                    worst_at_z: worst_at,
                    estimated_volume_mm3: volume,
                });
            }
        }
    }

    let result = InterferenceResult {
        pairs_checked: pairs,
        z_resolution_mm: step_mm,
        xy_samples_per_axis: XY_SAMPLES,
        overlaps,
    };
    let ok = result.overlaps.is_empty();
    let summary = if ok {
        format!("{pairs} pair(s) checked, none share space")
    } else {
        result
            .overlaps
            .iter()
            .map(|o| {
                format!(
                    "{} and {} overlap from Z{:.2}, worst at Z{:.2}, about {:.1} mm3",
                    o.a, o.b, o.first_at_z, o.worst_at_z, o.estimated_volume_mm3
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    Ok(Verdict::new(Check::Interference, ok, summary, &result))
}

/// Area shared by two sections, by grid sampling inside their common
/// bounding box.
fn overlap_area(a: &[(Point2, Point2)], b: &[(Point2, Point2)], lo: [f64; 2], hi: [f64; 2]) -> f64 {
    let width = hi[0] - lo[0];
    let height = hi[1] - lo[1];
    if width <= 0.0 || height <= 0.0 {
        return 0.0;
    }
    let n = XY_SAMPLES;
    let cell = (width / n as f64) * (height / n as f64);
    let mut area = 0.0;
    for iy in 0..n {
        for ix in 0..n {
            let p = [
                lo[0] + width * (ix as f64 + 0.5) / n as f64,
                lo[1] + height * (iy as f64 + 0.5) / n as f64,
            ];
            if slicer_geometry::section::contains_in(a, p)
                && slicer_geometry::section::contains_in(b, p)
            {
                area += cell;
            }
        }
    }
    area
}

// -------------------------------------------------------------- overhangs

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverhangResult {
    /// Faces further from vertical than this need support.
    pub threshold_deg: f64,
    /// Height treated as the build plate; downward faces on it are
    /// supported and excluded.
    pub bed_z: f64,
    /// Total downward-facing area beyond the threshold.
    pub area_mm2: f64,
    /// The flattest downward face found, in degrees from vertical.
    pub worst_deg: f64,
    pub worst_at_z: f64,
    pub triangles_over_threshold: usize,
}

/// Height within which a downward face counts as resting on the plate.
/// Far below a layer, so only faces genuinely in the bed plane qualify.
const ON_THE_BED_MM: f64 = 1e-6;

/// Downward-facing surfaces too flat to print without support.
///
/// Angles are measured **from vertical**: a vertical wall is 0 degrees, a
/// horizontal ceiling is 90. The usual "45 degree rule" is therefore
/// `threshold_deg = 45`. Stated explicitly because the opposite
/// convention is equally common and the two differ by exactly the amount
/// that matters.
///
/// The part's own footprint on the build plate is excluded: it is a
/// downward face at 90 degrees, and it is held up by the plate. Taking
/// the bed as the mesh's lowest point is what "the part sits on the
/// plate" means; use [`overhangs_above`] where it does not.
pub fn overhangs(mesh: &TriangleMesh, threshold_deg: f64) -> Result<Verdict, MeshError> {
    let (min, _) = mesh.bounds().ok_or(MeshError::Empty)?;
    overhangs_above(mesh, threshold_deg, min[2])
}

/// As [`overhangs`], with the build plate stated explicitly.
pub fn overhangs_above(
    mesh: &TriangleMesh,
    threshold_deg: f64,
    bed_z: f64,
) -> Result<Verdict, MeshError> {
    if mesh.is_empty() {
        return Err(MeshError::Empty);
    }
    if !(threshold_deg.is_finite() && (0.0..=90.0).contains(&threshold_deg)) {
        return Err(MeshError::NotFinite {
            field: "threshold_deg",
            value: threshold_deg,
        });
    }

    let mut area = 0.0f64;
    let mut worst_deg = 0.0f64;
    let mut worst_at_z = 0.0f64;
    let mut count = 0usize;

    for [a, b, c] in &mesh.triangles {
        let u = sub(*b, *a);
        let v = sub(*c, *a);
        let n = cross(u, v);
        let length = norm(n);
        if length <= 0.0 {
            continue;
        }
        // Only downward-facing surfaces can be unsupported.
        let downward = -n[2] / length;
        if downward <= 0.0 {
            continue;
        }
        // …and a downward face lying on the plate is held up by it. Left
        // in, a part's own footprint reads as the flattest overhang it
        // has, and every solid on a bed fails.
        if a[2].max(b[2]).max(c[2]) <= bed_z + ON_THE_BED_MM {
            continue;
        }
        let from_vertical = downward.clamp(-1.0, 1.0).asin().to_degrees();
        if from_vertical <= threshold_deg {
            continue;
        }
        count += 1;
        area += length / 2.0;
        if from_vertical > worst_deg {
            worst_deg = from_vertical;
            worst_at_z = (a[2] + b[2] + c[2]) / 3.0;
        }
    }

    let result = OverhangResult {
        threshold_deg,
        bed_z,
        area_mm2: area,
        worst_deg,
        worst_at_z,
        triangles_over_threshold: count,
    };
    let ok = count == 0;
    let summary = if ok {
        format!("no downward face is more than {threshold_deg:.0}° from vertical")
    } else {
        format!(
            "{:.1} mm2 of downward surface past {threshold_deg:.0}° from vertical; the flattest \
             is {:.1}° at Z{:.2}",
            result.area_mm2, result.worst_deg, result.worst_at_z
        )
    };
    Ok(Verdict::new(Check::Overhangs, ok, summary, &result))
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn norm(a: [f64; 3]) -> f64 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}
