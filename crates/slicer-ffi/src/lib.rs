//! Rust ↔ C++ FFI bridge to the slicer engine.
//!
//! Runtime flow:
//!   slicer-rpc.dispatch.fake_slice (`slice.request`)  →  ffi::run_slice
//!   pipeline::run_slice_blocking (CLI, `slice.estimate`)  →  ffi::run_slice
//!     ffi::run_slice  →  cxx::bridge → C++ run_slice (cpp/)
//!
//! The C++ side currently ships a stub that produces deterministic
//! rectangle G-code mirroring the old Rust fake_slice. Once libslic3r
//! integration is wired behind the `with-libslic3r` feature, the bridge
//! impl swaps over without touching Rust callers.

/// The B-rep evaluator for `CsgNode` trees. Gated with the OCCT build:
/// without it every bridge entry point below answers "failed", so a
/// module that could only ever return errors would be a trap rather than
/// a fallback.
#[cfg(feature = "with-libslic3r")]
pub mod brep;
mod calibration_test;
mod default_preset;
pub mod gcode_header;
mod idex;
mod preview;

// cxx 1.0.194 derives Clone by cloning every field of non-Copy structs,
// including scalars. Rust 1.99 lints those generated calls; keep the allowance
// on the generated bridge so handwritten Rust still gets clone_on_copy checks.
#[allow(clippy::clone_on_copy)]
#[cxx::bridge(namespace = "protype::ffi")]
mod ffi_bridge {
    /// Bounding box in mm, expressed in the project's Z-up coordinate
    /// system (X/Y on the bed, Z vertical). Crossed verbatim into C++.
    #[derive(Debug, Clone, Copy)]
    struct BridgeBbox {
        min_x: f32,
        min_y: f32,
        min_z: f32,
        max_x: f32,
        max_y: f32,
        max_z: f32,
    }

    /// Column-major 4x4 transform matching the UI/Three.js matrix layout.
    /// It is applied to the source mesh after normalizing the imported mesh
    /// around the same visual origin the viewport uses.
    #[derive(Debug, Clone, Copy)]
    struct BridgeObjectTransform {
        m00: f32,
        m01: f32,
        m02: f32,
        m03: f32,
        m04: f32,
        m05: f32,
        m06: f32,
        m07: f32,
        m08: f32,
        m09: f32,
        m10: f32,
        m11: f32,
        m12: f32,
        m13: f32,
        m14: f32,
        m15: f32,
    }

    /// Process-level scalar overrides from the daemon snapshot. The bridge
    /// applies these as `DynamicPrintConfig::set_key_value` overrides on top
    /// of the bundled OrcaSlicer flat preset, which keeps legacy daemon
    /// overrides effective until Profile v2 fully owns slice preparation.
    #[derive(Debug, Clone)]
    struct BridgeConfigOverride {
        key: String,
        value: String,
    }

    /// One Z-range setting override for the (single) sliced object.
    /// Maps onto an entry of libslic3r's `ModelObject::layer_config_ranges`.
    #[derive(Debug, Clone)]
    struct BridgeLayerConfigRange {
        id: String,
        z_min: f64,
        z_max: f64,
        // Override keys must be in LAYER_RANGE_OVERRIDABLE_KEYS
        // (slicer-types). Values are serialised to libslic3r-style text
        // so the C++ bridge can call `DynamicPrintConfig::set_deserialize`
        // without an extra type table on the Rust side. Reuses the same
        // (key, value) shape as the existing BridgeConfigOverride. A value
        // libslic3r drops or cannot read is skipped and named in the
        // outcome's warnings.
        overrides: Vec<BridgeConfigOverride>,
        // Fan paint for the range's layers, encoded like a fan-mask facet
        // (`1 + (lines - 1) * 21 + speed / 5`, 1..=147); 0 = no fan band. The
        // bridge keeps a range that carries only a fan value.
        fan_value: u8,
    }

    /// Per-object inputs for one entry on the plate. Replaces the old
    /// flat single-object fields on `BridgeProcessSettings`
    /// (`object_transform`, `seam_facets_rle`, `support_facets_rle`,
    /// `layer_height_profile`, `layer_ranges`) now that a slice request
    /// carries N objects instead of one.
    #[derive(Debug, Clone)]
    struct BridgeObjectInputs {
        input_path: String,
        /// STEP part index; -1 for STL/3MF single-mesh objects.
        part_index: i32,
        object_transform: BridgeObjectTransform,
        seam_facets_rle: String,
        support_facets_rle: String,
        /// Fan paint mask, the same wire format as the other two; its values
        /// encode fan speed and lines (0..=147). Empty = none.
        fan_facets_rle: String,
        filament_facets_rle: String,
        layer_height_profile: Vec<f32>,
        layer_ranges: Vec<BridgeLayerConfigRange>,
        object_overrides: Vec<BridgeConfigOverride>,
    }

    /// libslic3r `CalibMode` for this slice: 0 none, 1 retraction tower,
    /// 2 PA tower, 3 volumetric-speed tower. See `CalibrationTestMode`.
    #[derive(Debug, Clone, Copy)]
    struct BridgeCalibTest {
        mode: i32,
        start: f64,
        end: f64,
        step: f64,
        /// `CALIB_RETRACTION_STEPS` only: band layout and speed series (mm, mm/s).
        band_height: f64,
        base_height: f64,
        speed_start: f64,
        speed_step: f64,
    }

    /// Scene snapshot for the G-code thumbnails: RGBA8, first row on top.
    /// `width == 0`: no snapshot.
    #[derive(Debug, Clone)]
    struct BridgeScenePreview {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    }

    #[derive(Debug, Clone)]
    struct BridgeProcessSettings {
        layer_height: f32,
        wall_loops: i32,
        sparse_infill_density_pct: f32,
        // Pattern key as understood by libslic3r (`grid`, `gyroid`,
        // `honeycomb`, `lines`, `triangles`, ...). Empty string = leave
        // preset default in place.
        sparse_infill_pattern: String,
        enable_support: bool,
        // Single "print speed" knob mapped onto outer/inner wall + sparse
        // infill speeds. 0 = leave preset defaults.
        print_speed_mm_s: f32,
        // Top solid skin thickness in mm. Bridge converts to
        // `top_shell_layers = ceil(this / layer_height)` so the seal stays
        // ~constant when the user changes layer_height. 0 = leave the
        // bridge's built-in 1mm fallback in place.
        top_shell_thickness_mm: f32,
        // Perimeter generator: `classic` (fixed-width) or `arachne`
        // (variable-width). Empty = bridge applies its own default
        // (`arachne` — better for thin features like benchy chimneys).
        wall_generator: String,
        // Flat OrcaSlicer preset the libslic3r path loads before applying
        // `config_overrides`: the embedded copy, staged to a temp file for
        // this slice (see `default_preset`). The stub ignores it.
        preset_path: String,
        // Resolved profile values carried by the agent. These are applied
        // to DynamicPrintConfig by key after loading the bundled preset,
        // so process knobs such as brim/skirt/cooling don't need bespoke
        // bridge fields.
        config_overrides: Vec<BridgeConfigOverride>,
        // 1 = single extruder; 2 = IDEX dual; values >2 are rejected by
        // Sub-C and reserved for later.
        extruder_count: u8,
        // Single-head selector for IDEX. -1 = normal multi-tool slice
        // (per-object assignment). 0/1 = print the whole plate with that
        // physical head: the bridge routes every object to that extruder
        // and pads `filament_diameter` to `extruder_count` so ToolOrdering
        // emits `T{n}` / `initial_extruder = n`. Mirrors
        // `IdexInputs::print_head` (Option<u8>) flattened to a sentinel.
        active_extruder: i32,
        // true: every object goes down on the bed with the plate of objects
        // without a transform (`place_authored_plate_on_bed`) — a transform
        // shapes its object but does not place it. false: an object with a
        // transform stands where the transform puts it. See
        // `SliceParams::place_plate_on_bed`.
        place_plate_on_bed: bool,
        // libslic3r per-layer calibration mode (mode 0 = none).
        calib_test: BridgeCalibTest,
        // One entry per object on the plate, in deterministic order.
        // Carries the per-object fields that used to live flat on this
        // struct: input_path, part_index, object_transform,
        // seam_facets_rle, support_facets_rle, layer_height_profile,
        // layer_ranges, object_overrides.
        objects: Vec<BridgeObjectInputs>,
        // Validated producer line for the G-code header; never empty.
        producer_line: String,
        preview: BridgeScenePreview,
        // true: the machine is a Bambu one and keeps the Bambu keys of the
        // built-in preset.
        is_bbl_machine: bool,
    }

    /// Per-slot extruder inputs. Length of every populated Vec must equal
    /// `extruder_count` (which is `Vec::len()` on `nozzle_diameter`).
    ///
    /// Mirrors the per-slot wire format the agent emits in `flat_map.rs`
    /// (Sub-A1). Field order, units, and key naming match the libslic3r
    /// `ConfigOptionVector*` keys 1:1 so the C++ bridge can fan them out
    /// without re-mapping names.
    #[derive(Debug, Clone)]
    struct BridgeIdexSettings {
        // X,Y offsets in mm. Slot 0 is canonical (0,0) for typical IDEX
        // printers; slot 1 is the head's docked offset.
        extruder_offset_x: Vec<f32>,
        extruder_offset_y: Vec<f32>,
        // mm.
        nozzle_diameter: Vec<f32>,
        // Retraction-group. Empty Vec = "let preset pad with
        // FullPrintConfig::defaults() via set_num_extruders".
        retraction_length: Vec<f32>,
        retraction_speed: Vec<f32>,
        retract_restart_extra: Vec<f32>,
        deretraction_speed: Vec<f32>,
        retraction_minimum_travel: Vec<f32>,
        retract_when_changing_layer: Vec<bool>,
        wipe: Vec<bool>,
        wipe_distance: Vec<f32>,
        retract_before_wipe_pct: Vec<f32>,
        retract_length_toolchange: Vec<f32>,
        // z_hop group.
        z_hop: Vec<f32>,
        // libslic3r enum strings: "none", "normal", "slope", "spiral".
        z_hop_types: Vec<String>,
        travel_slope_deg: Vec<f32>,
        retract_lift_above: Vec<f32>,
        retract_lift_below: Vec<f32>,
        // libslic3r `RetractLiftEnforceType` spellings ("All Surfaces", "Top
        // Only", "Bottom Only", "Top and Bottom"); the bridge also maps the
        // agent's snake_case. Empty = let the preset decide.
        retract_lift_enforce: Vec<String>,
        // layer_limits group.
        min_layer_height: Vec<f32>,
        max_layer_height: Vec<f32>,
        // Filament-side per-slot vectors. The agent already merges
        // Filament[slot=i].* over Extruder[slot=i].* in resolve, so these
        // arrive in their final form.
        filament_type: Vec<String>,
        filament_colour: Vec<String>,
        nozzle_temperature_initial_layer: Vec<i32>,
        nozzle_temperature: Vec<i32>,
        bed_temperature_initial_layer_single: Vec<i32>,
        bed_temperature: Vec<i32>,
        // Toolchange G-code template ('change_filament_gcode' in
        // OrcaSlicer parlance). Single string, applies across all
        // toolchanges. Empty string = no template.
        change_filament_gcode: String,
        // Machine macros that the agent ships when extruder_count > 1.
        // Each is an opt-in override that, when non-empty, replaces the
        // preset's default.
        machine_pause_gcode: String,
        time_lapse_gcode: String,
        change_extrusion_role_gcode: String,
        printing_by_object_gcode: String,
    }

    /// Outcome returned from the C++ slicer.
    #[derive(Debug, Clone)]
    struct BridgeSliceOutcome {
        success: bool,
        /// True when the slice exited via cooperative cancellation (the
        /// caller flipped the cancel flag mid-process). When set,
        /// `success` is false and `error_message` carries the human
        /// reason ("canceled by user"). Distinguishes user-cancel from
        /// genuine slicing failures so the daemon can emit
        /// `SliceCanceled` instead of `SliceFailed`.
        canceled: bool,
        total_layers: u32,
        total_segments: u64,
        error_message: String,
        /// Filename libslic3r recommends for this slice's G-code based
        /// on the active `filename_format` config (default:
        /// `{input_filename_base}_{filament_type[initial_tool]}_{print_time}.gcode`).
        /// Concrete examples: `OrcaToleranceTest_PA-CF_15m.gcode`,
        /// `cube_PETG-CF_2h45m.gcode`. Used by the UI as the default
        /// name in the Save As dialog. Empty when the stub path runs
        /// (no preset → no format) — daemon falls back to
        /// `<input_basename>.gcode` in that case.
        suggested_filename: String,
        /// Non-fatal warning from `Print::validate`, whole, then one naming
        /// the layer-range overrides libslic3r did not take, if any. Empty
        /// under the stub path. See `SliceResult::warnings`.
        warnings: Vec<String>,
    }

    /// Per-object inspection record. STEP files commonly produce multi-part
    /// assemblies (libslic3r emits one `ModelObject` per part); STL/3MF and
    /// other single-part inputs produce a one-element vec. `part_index` is
    /// the object's position in `Slic3r::Model::objects` and is stable
    /// across a single `inspect_model` call.
    #[derive(Debug, Clone)]
    struct BridgeInspectedPart {
        part_name: String,
        part_index: u32,
        triangle_count: u64,
        min_x: f32,
        min_y: f32,
        min_z: f32,
        max_x: f32,
        max_y: f32,
        max_z: f32,
    }

    #[derive(Debug, Clone)]
    struct BridgeInspectModelResult {
        success: bool,
        error_message: String,
        parts: Vec<BridgeInspectedPart>,
    }

    /// Outcome of `bridge_query_model_faces`: tessellate one part of a
    /// STEP/CAD file, write a binary STL plus a per-face triangle-range
    /// map (JSON), and report the counts. A "part" is a libslic3r
    /// `ModelObject` (the same unit `inspect_model`/`run_slice` index by
    /// `part_index`), so its geometry is the union of every solid that
    /// object owns — for a STEP that is the whole file. `face_count` is the
    /// number of B-rep faces emitted into the facemap; `triangle_count` is
    /// the total number of triangles in the written STL (the facemap ranges
    /// cover exactly `[0, triangle_count)` contiguously) and equals the
    /// `triangle_count` `inspect_model` reports for the same `part_index`.
    /// On failure `success` is false, `error_message` explains why, and no
    /// files are written.
    #[derive(Debug, Clone)]
    struct BridgeFaceQueryOutcome {
        success: bool,
        error_message: String,
        face_count: u32,
        triangle_count: u64,
    }

    /// Per-part record produced by `split_model_to_files`. Each entry
    /// corresponds to a single output mesh file (one per top-level
    /// `ModelObject` in libslic3r). `part_index` is the object's
    /// position in the source `Slic3r::Model::objects` vector and is
    /// stable across the call.
    #[derive(Debug, Clone)]
    struct BridgeSplitPartFile {
        out_path: String,
        part_index: u32,
        triangle_count: u64,
        min_x: f32,
        min_y: f32,
        min_z: f32,
        max_x: f32,
        max_y: f32,
        max_z: f32,
    }

    #[derive(Debug, Clone)]
    struct BridgeSplitToFilesResult {
        success: bool,
        error_message: String,
        parts: Vec<BridgeSplitPartFile>,
    }

    /// An op `apply_operations` left out. `code` is stable and made for a
    /// program to branch on; `reason` is a sentence for whoever has to aim
    /// the op again, an agent that cannot see the part included.
    #[derive(Debug, Clone)]
    struct BridgeSkippedOp {
        id: String,
        code: String,
        reason: String,
    }

    /// Outcome of `apply_operations`: subtract the op-stack's cylindrical
    /// holes from a part's mesh and write the result as a binary STL.
    /// `applied_count` is the number of `kind=="hole"` ops whose boolean
    /// produced a non-empty result; `skipped_count` counts ops that were
    /// attempted but whose cutter/boolean yielded an empty or failed result.
    /// Suppressed ops are ignored entirely; ops with a `kind` this executor
    /// doesn't implement now count as skipped and appear in
    /// `skipped_op_ids`. `triangle_count` is the final mesh's triangle
    /// count. On failure `success` is false and `error_message` explains
    /// why.
    #[derive(Debug, Clone)]
    struct BridgeApplyOpsOutcome {
        success: bool,
        error_message: String,
        triangle_count: u64,
        applied_count: u32,
        skipped_count: u32,
        /// Ids of every op that did NOT make it into the result — failed
        /// booleans, degenerate cutters, and (since this change) ops whose
        /// `kind` this executor doesn't implement. Always the same length as
        /// `skipped_count`; the UI needs names, not just a tally, to mark
        /// the offending rows in the operation tree.
        skipped_op_ids: Vec<String>,
        /// The same ops in the same order, each with why it was left out.
        skipped_ops: Vec<BridgeSkippedOp>,
    }

    extern "Rust" {
        /// The snapshot scaled to `width` x `height`, RGBA rows bottom-up, as
        /// Orca's `ThumbnailData` takes them; empty when unusable.
        fn preview_thumbnail_bottom_up(
            preview: &BridgeScenePreview,
            width: u32,
            height: u32,
        ) -> Vec<u8>;
    }

    unsafe extern "C++" {
        include!("slicer-ffi/cpp/protype_slicer_bridge.h");

        /// Assemble a `Slic3r::Model` from `settings.objects` (one
        /// `ModelObject` per plate entry — each carrying its own
        /// `input_path`, transform, paint masks, and layer profile/ranges)
        /// and slice it to G-code at `output_path`.
        ///
        /// `cancel_flag_addr` is an opaque address: zero means "no
        /// cancellation hook", non-zero means "treat this as a pointer
        /// to a single byte that the caller will flip to non-zero when
        /// it wants the slice to abort". The bridge spawns a watcher
        /// thread that polls that byte and calls `print.cancel()` when
        /// it flips. Layout: Rust's `AtomicBool` is a single byte, the
        /// C++ side reads it as a `volatile uint8_t*` — no
        /// cross-language atomic-ABI concerns for a relaxed-ordering
        /// "did the user say cancel yet?" poll.
        fn run_slice(
            output_path: &str,
            settings: BridgeProcessSettings,
            idex: BridgeIdexSettings,
            bbox: BridgeBbox,
            cancel_flag_addr: u64,
        ) -> BridgeSliceOutcome;

        fn inspect_model(input_path: &str) -> BridgeInspectModelResult;

        /// Tessellate part `part_index` of a STEP/CAD file, writing a binary
        /// STL to `out_stl_path` and a per-face triangle-range map (JSON) to
        /// `out_facemap_path`. The facemap lets callers map a
        /// clicked/selected mesh triangle back to its originating B-rep face
        /// and that face's analytic surface (plane/cylinder/cone/…).
        ///
        /// `part_index` is a libslic3r `ModelObject` index — the SAME unit
        /// `inspect_model` enumerates and `run_slice` slices (which indexes
        /// `loaded.objects[part_index]`), NOT a raw solid index. Upstream
        /// `load_step` packs every solid of a STEP file into one
        /// `ModelObject`, so a STEP has a single part (index 0) covering all
        /// solids; the emitted STL therefore matches the geometry `run_slice`
        /// slices for the same `part_index`, and its `triangle_count` matches
        /// `inspect_model`'s. An out-of-range index yields `success=false`.
        ///
        /// Requires the `with-libslic3r` (OCCT) build; the stub bridge
        /// reports `success=false` with a "libslic3r" error message.
        fn bridge_query_model_faces(
            source_path: &str,
            part_index: i32,
            out_stl_path: &str,
            out_facemap_path: &str,
        ) -> BridgeFaceQueryOutcome;

        /// Split a multi-part input mesh into one file per top-level
        /// `ModelObject`. Output files are written under `output_dir` as
        /// `part_<i>.stl` (the caller is responsible for creating/cleaning
        /// the directory). The returned `parts` vec records the on-disk
        /// path and bbox for each emitted file.
        ///
        /// Idempotent: if `<output_dir>/part_<i>.stl` already exists, the
        /// bridge reuses it without re-tessellating. The cache is
        /// content-addressed by the caller (via source_hash in the path);
        /// no invalidation happens here.
        ///
        /// If the source decomposes to exactly one shell across all input
        /// objects, returns `success=false, error_message="single shell"`
        /// — callers should surface this to users rather than treat it as
        /// an error.
        fn split_model_to_files(source_path: &str, output_dir: &str) -> BridgeSplitToFilesResult;

        /// Cut a part's mesh with a plane and write each kept side as a
        /// binary STL in `output_dir`. See [`cut_model_with_plane`], the
        /// public wrapper.
        fn cut_model_with_plane(
            source_path: &str,
            part_index: i32,
            recenter: bool,
            origin: [f32; 3],
            normal: [f32; 3],
            keep: &str,
            output_dir: &str,
        ) -> BridgeSplitToFilesResult;

        /// Phase-2 link-gate smoke: run a fixed `cube - cylinder` mcut
        /// DIFFERENCE (`A_NOT_B`), write the manifold result as a binary STL
        /// to `out_stl_path`, and report success. Its sole purpose is to prove
        /// the mcut + CGAL boolean static archives link. Returns `false` under
        /// the stub build (no `with-libslic3r`).
        fn mcut_subtract_smoke(out_stl_path: &str) -> bool;

        /// Apply an op-stack (`ops_json`) to part `part_index` of a mesh
        /// file, subtracting each non-suppressed `kind=="hole"` op as a
        /// cylindrical cutter (mcut `A_NOT_B`), and write the result as a
        /// binary STL to `out_stl_path`.
        ///
        /// The object's combined (`raw_mesh`) geometry is recentered to its
        /// local bbox frame (XY-center→0, z-min→0) BEFORE any op is applied,
        /// mirroring the UI's `recenterGeometryToLocalBbox` so op coordinates
        /// (`origin`/`axis`) and the written mesh share one coordinate frame.
        /// `part_index` is a libslic3r `ModelObject` index (`-1` → first
        /// object), the SAME unit `inspect_model`/`run_slice` use.
        ///
        /// A per-op boolean that throws or yields an empty mesh is skipped
        /// (`skipped_count`++, id appended to `skipped_op_ids`) and the
        /// pipeline continues — one bad op never aborts the whole call.
        /// Suppressed ops are ignored without counting as skipped; ops whose
        /// `kind` this executor doesn't implement are now skipped the same
        /// way (named in `skipped_op_ids`) instead of vanishing uncounted.
        /// The STL is written even when no op applied (the recentered
        /// source mesh). Returns `success=false` with a
        /// "libslic3r"-mentioning error under the stub build (no
        /// `with-libslic3r`).
        fn apply_operations(
            source_path: &str,
            part_index: i32,
            ops_json: &str,
            out_stl_path: &str,
        ) -> BridgeApplyOpsOutcome;

        /// B-rep construction, handle-based on purpose: the CSG tree
        /// recursion stays in Rust, where it is already written and tested
        /// against the mesh kernel, and C++ holds nothing but a shape table.
        /// A returned `0` means the operation failed — OCCT reports by
        /// throwing, and an exception must not cross this boundary.
        fn brep_box(x: f64, y: f64, z: f64) -> u64;
        /// Equal radii give a cylinder, unequal ones a cone or frustum. A
        /// zero top is the cone tip — the one place a zero radius is legal.
        fn brep_cylinder(radius_bottom: f64, radius_top: f64, height: f64) -> u64;
        /// Sphere centred on the origin. `segments` has no counterpart
        /// here on purpose: a B-rep sphere is the sphere itself.
        fn brep_sphere(radius: f64) -> u64;
        /// `outline_xy` is a flat `x,y,x,y,…` list; the bridge closes the
        /// polygon itself and refuses fewer than three points.
        fn brep_prism(outline_xy: &[f64], height: f64) -> u64;
        /// `outline_rz` is a flat `r,z,r,z,…` profile in the XZ
        /// half-plane, swept `degrees` around the Z axis.
        fn brep_revolve(outline_rz: &[f64], degrees: f64) -> u64;
        /// Mirror across the plane through the origin with this normal.
        fn brep_mirror(handle: u64, nx: f64, ny: f64, nz: f64) -> u64;
        /// Round the selected edges. `selector`: 0 all, 1 vertical,
        /// 2 top, 3 bottom, 4 horizontal. A selector matching no edge is
        /// a refusal, not a no-op — an unchanged part reads as filleted.
        fn brep_fillet(handle: u64, radius: f64, selector: u8) -> u64;
        /// Flat-cut the selected edges; same selector encoding.
        fn brep_chamfer(handle: u64, distance: f64, selector: u8) -> u64;
        /// Hollow the body out. `open`: 0 none, 1 top, 2 bottom.
        fn brep_shell(handle: u64, thickness: f64, open: u8) -> u64;
        /// A solid through stacked cross-sections. `flat_sections` is a
        /// flat `x,y,…` list, `section_lengths` the point count of each
        /// section, `heights` their z — all three in bottom-up order.
        fn brep_loft(flat_sections: &[f64], section_lengths: &[u32], heights: &[f64]) -> u64;
        /// Rotation is applied about X, then Y, then Z, then the
        /// translation — the order `CsgNode::Rotate` documents.
        fn brep_transform(
            handle: u64,
            tx: f64,
            ty: f64,
            tz: f64,
            rx_deg: f64,
            ry_deg: f64,
            rz_deg: f64,
        ) -> u64;
        /// `op`: 0 union, 1 difference, 2 intersection.
        fn brep_boolean(op: u8, a: u64, b: u64) -> u64;
        /// Volume in mm³, or 0.0 for a handle nobody holds.
        fn brep_volume(handle: u64) -> f64;
        fn brep_write_step(handle: u64, out_path: &str) -> bool;
        fn brep_release(handle: u64);

        fn version() -> String;

        /// How libslic3r serialises a key's per-slot values: 0 = unknown key,
        /// 1 = scalar, 2 = vector joined with `,`, 3 = `coStrings` (`;`-joined,
        /// C-style escaped). Reads `print_config_def`; the stub answers 0.
        fn config_option_join_kind(key: &str) -> u8;

        /// Whether libslic3r applies `key` from a layer range: `layer_height`
        /// or a `PrintRegionConfig` option. The stub answers false.
        fn config_option_applies_per_layer_range(key: &str) -> bool;

        /// How `run_slice` names a plate override libslic3r would drop, or
        /// empty when it applies it. The stub answers empty.
        fn ignored_config_override(key: &str, value: &str) -> String;
    }
}

pub use calibration_test::{CalibrationTest, CalibrationTestMode, RetractionSteps};
pub use ffi_bridge::{
    BridgeBbox, BridgeObjectTransform, BridgeProcessSettings, BridgeSliceOutcome,
};
pub use idex::IdexInputs;
use preview::{bridge_preview, preview_thumbnail_bottom_up};
pub use preview::{PreviewError, ScenePreview, MAX_PREVIEW_DIMENSION};

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub const IDENTITY_OBJECT_TRANSFORM: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 1.0, //
];

/// Cooperative cancellation handle for an in-flight slice.
///
/// The dispatcher creates one of these per slice job and stores the `Arc`
/// in a job-id keyed map. `slice.cancel` looks up the entry and flips the
/// flag. The C++ bridge polls the flag from a watcher thread and calls
/// `print.cancel()`, which makes the next `throw_if_canceled()` inside
/// libslic3r raise `Slic3r::CanceledException`. The bridge catches that,
/// reports `canceled = true`, and the dispatcher emits `SliceCanceled`.
///
/// The handle stays alive (via the held `Arc`) for the entire duration of
/// the FFI call so the C++ side never reads freed memory.
#[derive(Debug, Clone, Default)]
pub struct CancelHandle {
    flag: Arc<AtomicBool>,
}

impl CancelHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Idempotent — safe to call after the slice
    /// has already finished. Returns true if the flag transitioned from
    /// false → true (i.e. this call actually requested the cancel).
    pub fn cancel(&self) -> bool {
        !self.flag.swap(true, std::sync::atomic::Ordering::Release)
    }

    pub fn is_canceled(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Raw byte address of the underlying atomic, for crossing the FFI
    /// boundary. The caller must keep `self` (or a clone) alive for the
    /// lifetime of the C++ read or it'll dereference freed memory.
    fn raw_addr(&self) -> u64 {
        Arc::as_ptr(&self.flag) as u64
    }
}

/// Friendlier Rust wrapper. Callers pass paths as `&str` and a Rust-shaped
/// bbox; we translate into the cxx types and back.
#[derive(Debug, Clone)]
pub struct ConfigOverride {
    pub key: String,
    pub value: String,
}

/// Rust-side input mirror of `BridgeLayerConfigRange`. Lives in this
/// crate rather than slicer-types because it carries the bridge-text
/// value encoding (not the typed `SettingValue` discriminator).
#[derive(Debug, Clone)]
pub struct LayerConfigRangeInputs {
    pub id: String,
    pub z_min: f64,
    pub z_max: f64,
    /// (key, libslic3r-text-encoded-value) pairs.
    pub overrides: Vec<(String, String)>,
    /// Encoded fan paint for the range's layers (see
    /// `BridgeLayerConfigRange::fan_value`); 0 = none.
    pub fan_value: u8,
}

/// Rust-side input mirror of `BridgeObjectInputs`. One entry per object
/// on the plate; replaces the old flat single-object fields that used to
/// live on `SliceParams` (`input_path`, `object_transform`,
/// `seam_facets_rle`, `support_facets_rle`, `layer_height_profile`,
/// `layer_ranges`).
#[derive(Debug, Clone)]
pub struct ObjectInputs {
    pub input_path: String,
    /// STEP part index; -1 for STL/3MF.
    pub part_index: i32,
    pub object_transform: [f32; 16],
    pub seam_facets_rle: String,
    pub support_facets_rle: String,
    /// Fan paint mask (base64 RLE, one encoded value per facet); `None` = none.
    pub fan_facets_rle: Option<String>,
    pub filament_facets_rle: Option<String>,
    pub layer_height_profile: Vec<f32>,
    pub layer_ranges: Vec<LayerConfigRangeInputs>,
    pub object_overrides: Vec<ConfigOverride>,
}

/// What a caller says about the file beyond the slice itself. The default is
/// what `slice.estimate` and the CLI send.
#[derive(Debug, Clone, Default)]
pub struct SliceContext {
    /// Line written under the generation banner; `None` = the engine's own.
    pub producer_line: Option<String>,
    /// Scene snapshot the thumbnails are made from; `None` = no thumbnails.
    pub preview: Option<ScenePreview>,
    pub is_bbl_machine: bool,
}

#[derive(Debug, Clone)]
pub struct SliceParams<'a> {
    pub output_path: &'a str,
    /// The plate-wide layer height: libslic3r slices every object at it
    /// that carries no `layer_height` in its `ObjectInputs::object_overrides`
    /// — so it is the project's, not the plate's finest pitch. The stub draws
    /// the whole plate at the finest of this and those overrides.
    /// Always forced; below 0.01 mm it is clamped to 0.01.
    pub layer_height: f32,
    /// 0 = keep the config's value (the preset's, or a `config_overrides`
    /// entry's).
    pub wall_loops: u32,
    /// Negative = keep the config's value. Above 100 is clamped to 100.
    pub sparse_infill_density_pct: f32,
    /// libslic3r pattern key (`grid`, `gyroid`, `honeycomb`, `lines`,
    /// `triangles`, ...). Empty = keep the config's value.
    pub sparse_infill_pattern: String,
    pub enable_support: bool,
    /// Print speed in mm/s. 0 = preserve preset defaults.
    pub print_speed_mm_s: f32,
    /// Top solid-skin thickness in mm. 0 = use the bridge's built-in
    /// 1mm fallback (which auto-scales `top_shell_layers` with
    /// layer_height).
    pub top_shell_thickness_mm: f32,
    /// `classic` or `arachne`. Empty = bridge default (`arachne`).
    pub wall_generator: String,
    /// Applied first. The scalars above are forced over them afterwards —
    /// all but a scalar at its keep value, `top_shell_thickness_mm` when a
    /// shell key is named here, and `print_speed_mm_s` when one of the three
    /// speeds it fans out to is.
    pub config_overrides: Vec<ConfigOverride>,
    pub bbox_min: [f32; 3],
    pub bbox_max: [f32; 3],
    /// Who places the objects. `false` is the window's plate: an object
    /// with a transform stands where the transform puts it, in bed
    /// coordinates, and one it leaves off the bed is refused; objects
    /// without one keep the arrangement their files give them and go down
    /// on the bed together (`place_authored_plate_on_bed` in the bridge).
    /// `true` is for a caller that costs a part rather than prints a plate
    /// (`slice.estimate`): transforms still scale and turn their objects,
    /// but every object goes down on the bed with that plate, so where a
    /// transform left one is not kept and cannot make the slice fail.
    pub place_plate_on_bed: bool,
    /// libslic3r per-layer calibration mode; `None` slices normally.
    pub calibration_test: Option<CalibrationTest>,
    /// One entry per object on the plate, in deterministic order.
    pub objects: Vec<ObjectInputs>,
    /// Optional cancel hook. `None` = run-to-completion (used by tests
    /// and headless flows that don't expose mid-slice cancel).
    pub cancel: Option<CancelHandle>,
    /// Optional IDEX per-slot inputs. `None` preserves the existing
    /// single-extruder path.
    pub idex: Option<IdexInputs>,
    pub context: SliceContext,
}

#[derive(Debug, Clone)]
pub struct SliceResult {
    pub success: bool,
    /// True when the slice ended because the caller flipped the cancel
    /// flag. Distinct from `success = false` for genuine errors.
    pub canceled: bool,
    pub error_message: String,
    pub total_layers: u32,
    pub total_segments: u64,
    /// libslic3r-recommended file name for this slice's G-code (no
    /// directory). Empty under the stub path.
    pub suggested_filename: String,
    /// Non-fatal warnings libslic3r's `Print::validate` raised, in its own
    /// (English) words. `validate` keeps only its last warning, so there is
    /// at most one such entry, and a message may span several lines.
    /// The slice still ran, but may not be what was asked for: when the used
    /// filaments disagree on shrinkage, libslic3r prints the whole plate
    /// without shrinkage compensation and this is the only place that says
    /// so. After it, the bridge adds one entry naming the layer-range
    /// overrides libslic3r did not take ("libslic3r ignored these
    /// layer-range settings: …"), which it skipped. Empty under the stub path.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct InspectedPart {
    /// Object name as libslic3r resolved it. May be empty (libslic3r emits
    /// "" for STL-derived objects). Callers fall back to a synthesized
    /// name when this is empty.
    pub part_name: String,
    /// Position in `Slic3r::Model::objects`. Stable for one
    /// `inspect_model` call but not cross-call.
    pub part_index: u32,
    pub triangle_count: u64,
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
}

#[derive(Debug, Clone)]
pub struct InspectModelResult {
    pub success: bool,
    pub error_message: String,
    /// One entry per `ModelObject`. STEP multi-part assemblies surface as
    /// N entries; STL/single-part inputs as one. Empty when `success` is
    /// false.
    pub parts: Vec<InspectedPart>,
}

/// How libslic3r wants a key's per-slot values serialised on the flat
/// override path (`ConfigOption*::deserialize` text format).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionJoinKind {
    /// Not in `print_config_def` — or the stub build, which has no table.
    Unknown,
    /// A single value; the text is stored verbatim (`coString` included).
    Scalar,
    /// `coFloats`/`coInts`/`coBools`/`coPercents`/`coEnums`/…: one value per
    /// extruder, joined with `,`.
    Vector,
    /// `coStrings`: one string per extruder, joined with `;` after C-style
    /// escaping (`escape_strings_cstyle`).
    Strings,
}

/// Ask libslic3r's option table how `key` is serialised. Aliases handled by
/// `PrintConfigDef::handle_legacy` are not resolved here — map the key first.
pub fn config_option_join_kind(key: &str) -> OptionJoinKind {
    match ffi_bridge::config_option_join_kind(key) {
        1 => OptionJoinKind::Scalar,
        2 => OptionJoinKind::Vector,
        3 => OptionJoinKind::Strings,
        _ => OptionJoinKind::Unknown,
    }
}

/// Ask libslic3r whether a layer range can carry `key`. A range's config is
/// copied onto `PrintRegionConfig` only (`apply_to_print_region_config`), and
/// `layer_height` is read straight from the range for the layer-height
/// profile; every other key deserialises fine and is then silently ignored.
/// The stub build has no option table and answers `false`.
pub fn config_option_applies_per_layer_range(key: &str) -> bool {
    ffi_bridge::config_option_applies_per_layer_range(key)
}

/// How `run_slice` names a plate `config_overrides` entry libslic3r would drop
/// without applying it — `sparse_infil_density (unknown or obsolete key)`,
/// `outer_wall_speed=50% (value in a legacy form)` — or `None` when libslic3r
/// applies it, a legacy spelling included (`enable_wipe_tower` sets
/// `enable_prime_tower`). `PrintConfigDef::handle_legacy` drops such an
/// override and the setter still reports success, so `run_slice` refuses a
/// slice that carries one; ask here first to leave it out instead. The stub
/// build applies no override and answers `None`.
pub fn describe_ignored_config_override(key: &str, value: &str) -> Option<String> {
    let description = ffi_bridge::ignored_config_override(key, value);
    (!description.is_empty()).then_some(description)
}

pub fn version() -> String {
    ffi_bridge::version()
}

/// Phase-2 link-gate smoke hook. Runs a fixed `cube - cylinder` (`A_NOT_B`)
/// boolean via `MeshBoolean::mcut::make_boolean`, writes the manifold result
/// as a binary STL to `out_stl_path`, and returns whether it succeeded.
///
/// Under the stub build (no `with-libslic3r`) the C++ side returns `false`
/// without referencing libslic3r. This exists purely to prove the mcut + CGAL
/// static archives link on this host; it is not part of the operation API.
pub fn mcut_subtract_smoke(out_stl_path: &str) -> bool {
    ffi_bridge::mcut_subtract_smoke(out_stl_path)
}

pub fn inspect_model(input_path: &str) -> InspectModelResult {
    let result = ffi_bridge::inspect_model(input_path);
    let parts = result
        .parts
        .into_iter()
        .map(|p| InspectedPart {
            part_name: p.part_name,
            part_index: p.part_index,
            triangle_count: p.triangle_count,
            bounds_min: [p.min_x, p.min_y, p.min_z],
            bounds_max: [p.max_x, p.max_y, p.max_z],
        })
        .collect();
    InspectModelResult {
        success: result.success,
        error_message: result.error_message,
        parts,
    }
}

/// Result of [`query_model_faces`]. Re-exports the cxx shared struct
/// directly (fields: `success`, `error_message`, `face_count`,
/// `triangle_count`) so callers get the counts without an extra copy.
pub type FaceQueryOutcome = ffi_bridge::BridgeFaceQueryOutcome;

/// Tessellate one part of a STEP/CAD file and write a mesh + face map.
///
/// - `source_path`: STEP/STP file to load.
/// - `part_index`: which part (libslic3r `ModelObject`) to tessellate — the
///   SAME index `inspect_model` enumerates and `run_slice` slices, NOT a raw
///   solid index. A STEP file is a single part (index 0) whose geometry is
///   every solid it contains; the emitted `triangle_count` matches what
///   `inspect_model` reports for that `part_index`. Negative values are
///   treated as 0; an out-of-range index returns `success=false`.
/// - `out_stl_path`: destination for the binary STL of that part.
/// - `out_facemap_path`: destination for the JSON facemap
///   (`{"faces":[{"face_id":0,"surface":"cylinder","origin":[…],"axis":[…],
///   "radius_mm":2.5,"tri_start":0,"tri_count":128}, …]}`) where the
///   `tri_start`/`tri_count` ranges are contiguous and cover every
///   triangle in the written STL.
///
/// Returns `success=false` with a "libslic3r"-mentioning error when built
/// without the `with-libslic3r` feature (the OCCT tessellator is absent).
pub fn query_model_faces(
    source_path: &str,
    part_index: i32,
    out_stl_path: &str,
    out_facemap_path: &str,
) -> FaceQueryOutcome {
    ffi_bridge::bridge_query_model_faces(source_path, part_index, out_stl_path, out_facemap_path)
}

pub use ffi_bridge::{
    brep_boolean, brep_box, brep_chamfer, brep_cylinder, brep_fillet, brep_loft, brep_mirror,
    brep_prism, brep_release, brep_revolve, brep_shell, brep_sphere, brep_transform, brep_volume,
    brep_write_step,
};

/// Result of [`apply_operations`]. Re-exports the cxx shared struct directly
/// (fields: `success`, `error_message`, `triangle_count`, `applied_count`,
/// `skipped_count`, `skipped_op_ids`, `skipped_ops`).
pub type ApplyOpsOutcome = ffi_bridge::BridgeApplyOpsOutcome;

/// One entry of [`ApplyOpsOutcome::skipped_ops`].
pub type SkippedOp = ffi_bridge::BridgeSkippedOp;

/// Apply an op-stack to a part's mesh, subtracting cylindrical holes.
///
/// - `source_path`: STL/3MF/STEP file to load.
/// - `part_index`: which part (libslic3r `ModelObject`) to operate on — the
///   same index `inspect_model`/`run_slice` use. Negative values map to the
///   first object; an out-of-range index returns `success=false`.
/// - `ops_json`: `{"ops":[{"id","kind","suppressed","origin":[x,y,z],
///   "axis":[x,y,z],"radius_mm","depth_mm","through"}, …]}`. Coordinates are
///   in the recentered local frame (XY-bbox-center→0, z-min→0), matching the
///   frame the UI produces. `suppressed` ops are ignored entirely; a
///   non-`hole` `kind` is now treated as skipped rather than ignored (see
///   below).
/// - `out_stl_path`: destination for the binary STL of the resulting mesh.
///
/// A cutter/boolean that fails or empties for one op — or a `kind` this
/// executor doesn't implement — is skipped (counted in `skipped_count`,
/// its `id` appended to `skipped_op_ids` and its reason to `skipped_ops`),
/// never aborting the whole call.
/// Returns `success=false` with a "libslic3r"-mentioning error when built
/// without `with-libslic3r`.
pub fn apply_operations(
    source_path: &str,
    part_index: i32,
    ops_json: &str,
    out_stl_path: &str,
) -> ApplyOpsOutcome {
    ffi_bridge::apply_operations(source_path, part_index, ops_json, out_stl_path)
}

#[derive(Debug, Clone)]
pub struct SplitPartFile {
    /// Absolute on-disk path to the emitted mesh file (typically
    /// `<output_dir>/part_<i>.stl`). The caller owns the file's
    /// lifetime; this struct just records where the bridge wrote it.
    pub out_path: String,
    /// Position in the source `Slic3r::Model::objects` vector. Stable
    /// across a single `split_model_to_files` call.
    pub part_index: u32,
    pub triangle_count: u64,
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
}

#[derive(Debug, Clone)]
pub struct SplitToFilesOutcome {
    pub success: bool,
    /// Empty on success. When a multi-part source decomposes to exactly
    /// one shell, this is `"single shell"` and `success` is false —
    /// callers should surface that to users rather than treat it as an
    /// error.
    pub error_message: String,
    /// One entry per emitted file. Empty when `success` is false.
    pub parts: Vec<SplitPartFile>,
}

pub fn split_model_to_files(source_path: &str, output_dir: &str) -> SplitToFilesOutcome {
    ffi_bridge::split_model_to_files(source_path, output_dir).into()
}

/// Cut a part's mesh with a plane, one binary STL for each kept side in
/// `output_dir`, upper side first. The same answer as
/// [`split_model_to_files`]: part files with their bounds.
///
/// - `recenter`: bring the mesh into the frame [`apply_operations`] places
///   holes in (XY centred on its bounding box, lowest point at z=0). `false`
///   for a file `apply_operations` wrote: it is in that frame already, and its
///   own bounding box may no longer be the part's.
/// - `origin`, `normal`: a point on the plane and the way it faces, in that
///   frame. The files are in that frame too.
/// - `keep`: `"both"`, `"upper"` or `"lower"`, upper being where `normal`
///   points.
///
/// A plane that leaves the whole part on one side is `success=false`. Returns
/// `success=false` with a "libslic3r"-mentioning error when built without
/// `with-libslic3r`.
pub fn cut_model_with_plane(
    source_path: &str,
    part_index: i32,
    recenter: bool,
    origin: [f32; 3],
    normal: [f32; 3],
    keep: &str,
    output_dir: &str,
) -> SplitToFilesOutcome {
    ffi_bridge::cut_model_with_plane(
        source_path,
        part_index,
        recenter,
        origin,
        normal,
        keep,
        output_dir,
    )
    .into()
}

impl From<ffi_bridge::BridgeSplitToFilesResult> for SplitToFilesOutcome {
    fn from(result: ffi_bridge::BridgeSplitToFilesResult) -> Self {
        let parts = result
            .parts
            .into_iter()
            .map(|p| SplitPartFile {
                out_path: p.out_path,
                part_index: p.part_index,
                triangle_count: p.triangle_count,
                bounds_min: [p.min_x, p.min_y, p.min_z],
                bounds_max: [p.max_x, p.max_y, p.max_z],
            })
            .collect();
        Self {
            success: result.success,
            error_message: result.error_message,
            parts,
        }
    }
}

/// Serializes slices against **other slices**, and nothing else.
///
/// Measured 2026-08-07: four threads slicing the same STL in one process
/// produced four different G-code files. Not different comments —
/// different toolpaths. 42651 motion lines against 42842, 3977 differing
/// lines, and the same move `G1 X120.051 Y123.564` extruding `E.15452` in
/// one and `E.00348` in another. Layer counts agreed, so the geometry is
/// fine; it is the extrusion and retraction state that is shared. Time
/// estimates for one input spread over 12m34s..12m58s.
///
/// The daemon dispatches slices through a detached `tokio::spawn`, so two
/// requests really can overlap, and neither caller would be told that the
/// G-code it got back was assembled out of both.
///
/// Deliberately not `STEP_FFI_LOCK` (see `slicer-rpc`'s dispatch, which
/// explains at length why a slice must not hold that one): holding the
/// inspector lock for a multi-second slice would stall short queries
/// behind it. This lock is narrower — it makes slices queue behind each
/// other and leaves inspector calls untouched, which is exactly the race
/// observed and no more.
///
/// Lock ordering stays one-way and deadlock-free: a slice takes this and
/// may then take the C++ side's own mutexes; nothing takes this while
/// holding one of those.
static SLICE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const CALIB_NONE: i32 = 0;
const CALIB_RETRACTION_TOWER: i32 = 1;
const CALIB_PA_TOWER: i32 = 2;
const CALIB_VOL_SPEED_TOWER: i32 = 3;
const CALIB_RETRACTION_STEPS: i32 = 4;

fn bridge_calib_test(test: Option<&CalibrationTest>) -> ffi_bridge::BridgeCalibTest {
    let Some(test) = test else {
        return ffi_bridge::BridgeCalibTest {
            mode: CALIB_NONE,
            start: 0.0,
            end: 0.0,
            step: 0.0,
            band_height: 0.0,
            base_height: 0.0,
            speed_start: 0.0,
            speed_step: 0.0,
        };
    };
    if let (Some(r), Some((length_step, speed_step))) =
        (test.retraction.as_ref(), test.retraction_step_sizes())
    {
        return ffi_bridge::BridgeCalibTest {
            mode: CALIB_RETRACTION_STEPS,
            start: test.start,
            end: test.end,
            step: length_step,
            band_height: r.band_height,
            base_height: r.base_height,
            speed_start: r.speed_start,
            speed_step,
        };
    }
    let mode = match test.mode {
        CalibrationTestMode::RetractionTower => CALIB_RETRACTION_TOWER,
        CalibrationTestMode::PaTower => CALIB_PA_TOWER,
        CalibrationTestMode::VolSpeedTower => CALIB_VOL_SPEED_TOWER,
        // Validation refuses retraction_steps without its layout.
        CalibrationTestMode::RetractionSteps => CALIB_NONE,
    };
    ffi_bridge::BridgeCalibTest {
        mode,
        start: test.start,
        end: test.end,
        step: test.step,
        band_height: 0.0,
        base_height: 0.0,
        speed_start: 0.0,
        speed_step: 0.0,
    }
}

pub fn run_slice(p: SliceParams<'_>) -> SliceResult {
    // Poisoning is not a reason to refuse: a previous slice panicking
    // says nothing about this one, and the state the lock protects lives
    // in C++ and is re-initialised per slice anyway.
    let _serialized = SLICE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let producer_line = match gcode_header::producer_line(p.context.producer_line.as_deref()) {
        Ok(line) => line,
        Err(e) => {
            return SliceResult {
                success: false,
                canceled: false,
                error_message: format!("invalid producer_line: {e}"),
                total_layers: 0,
                total_segments: 0,
                suggested_filename: String::new(),
                warnings: Vec::new(),
            }
        }
    };

    // libslic3r loads presets only from files. Staged under SLICE_LOCK and
    // kept alive until the bridge returns.
    let staged_preset = match default_preset::StagedPreset::stage_in(&std::env::temp_dir()) {
        Ok(staged) => staged,
        Err(e) => {
            return SliceResult {
                success: false,
                canceled: false,
                error_message: e.to_string(),
                total_layers: 0,
                total_segments: 0,
                suggested_filename: String::new(),
                warnings: Vec::new(),
            }
        }
    };

    let bbox = BridgeBbox {
        min_x: p.bbox_min[0],
        min_y: p.bbox_min[1],
        min_z: p.bbox_min[2],
        max_x: p.bbox_max[0],
        max_y: p.bbox_max[1],
        max_z: p.bbox_max[2],
    };
    let count = p.idex.as_ref().map(|i| i.extruder_count).unwrap_or(1);
    let objects: Vec<ffi_bridge::BridgeObjectInputs> = p
        .objects
        .iter()
        .map(|o| ffi_bridge::BridgeObjectInputs {
            input_path: o.input_path.clone(),
            part_index: o.part_index,
            object_transform: bridge_object_transform(o.object_transform),
            seam_facets_rle: o.seam_facets_rle.clone(),
            support_facets_rle: o.support_facets_rle.clone(),
            fan_facets_rle: o.fan_facets_rle.clone().unwrap_or_default(),
            filament_facets_rle: o.filament_facets_rle.clone().unwrap_or_default(),
            layer_height_profile: o.layer_height_profile.clone(),
            layer_ranges: o
                .layer_ranges
                .iter()
                .map(|r| ffi_bridge::BridgeLayerConfigRange {
                    id: r.id.clone(),
                    z_min: r.z_min,
                    z_max: r.z_max,
                    fan_value: r.fan_value,
                    overrides: r
                        .overrides
                        .iter()
                        .map(|(k, v)| ffi_bridge::BridgeConfigOverride {
                            key: k.clone(),
                            value: v.clone(),
                        })
                        .collect(),
                })
                .collect(),
            object_overrides: o
                .object_overrides
                .iter()
                .map(|c| ffi_bridge::BridgeConfigOverride {
                    key: c.key.clone(),
                    value: c.value.clone(),
                })
                .collect(),
        })
        .collect();
    let settings = BridgeProcessSettings {
        layer_height: p.layer_height,
        wall_loops: p.wall_loops as i32,
        sparse_infill_density_pct: p.sparse_infill_density_pct,
        sparse_infill_pattern: p.sparse_infill_pattern.clone(),
        enable_support: p.enable_support,
        print_speed_mm_s: p.print_speed_mm_s,
        top_shell_thickness_mm: p.top_shell_thickness_mm,
        wall_generator: p.wall_generator.clone(),
        preset_path: staged_preset.path().to_string_lossy().into_owned(),
        config_overrides: p
            .config_overrides
            .iter()
            .map(|o| ffi_bridge::BridgeConfigOverride {
                key: o.key.clone(),
                value: o.value.clone(),
            })
            .collect(),
        extruder_count: count,
        active_extruder: p
            .idex
            .as_ref()
            .and_then(|i| i.print_head)
            .map(i32::from)
            .unwrap_or(-1),
        place_plate_on_bed: p.place_plate_on_bed,
        calib_test: bridge_calib_test(p.calibration_test.as_ref()),
        objects,
        producer_line,
        preview: bridge_preview(p.context.preview.as_ref()),
        is_bbl_machine: p.context.is_bbl_machine,
    };
    let idex = if let Some(ref i) = p.idex {
        i.assert_slot_lengths();
        let (xs, ys): (Vec<f32>, Vec<f32>) =
            i.extruder_offset_xy.iter().map(|p| (p[0], p[1])).unzip();
        ffi_bridge::BridgeIdexSettings {
            extruder_offset_x: xs,
            extruder_offset_y: ys,
            nozzle_diameter: i.nozzle_diameter.clone(),
            retraction_length: i.retraction_length.clone(),
            retraction_speed: i.retraction_speed.clone(),
            retract_restart_extra: i.retract_restart_extra.clone(),
            deretraction_speed: i.deretraction_speed.clone(),
            retraction_minimum_travel: i.retraction_minimum_travel.clone(),
            retract_when_changing_layer: i.retract_when_changing_layer.clone(),
            wipe: i.wipe.clone(),
            wipe_distance: i.wipe_distance.clone(),
            retract_before_wipe_pct: i.retract_before_wipe_pct.clone(),
            retract_length_toolchange: i.retract_length_toolchange.clone(),
            z_hop: i.z_hop.clone(),
            z_hop_types: i.z_hop_types.clone(),
            travel_slope_deg: i.travel_slope_deg.clone(),
            retract_lift_above: i.retract_lift_above.clone(),
            retract_lift_below: i.retract_lift_below.clone(),
            retract_lift_enforce: i.retract_lift_enforce.clone(),
            min_layer_height: i.min_layer_height.clone(),
            max_layer_height: i.max_layer_height.clone(),
            filament_type: i.filament_type.clone(),
            filament_colour: i.filament_colour.clone(),
            nozzle_temperature_initial_layer: i.nozzle_temperature_initial_layer.clone(),
            nozzle_temperature: i.nozzle_temperature.clone(),
            bed_temperature_initial_layer_single: i.bed_temperature_initial_layer_single.clone(),
            bed_temperature: i.bed_temperature.clone(),
            change_filament_gcode: i.change_filament_gcode.clone(),
            machine_pause_gcode: i.machine_pause_gcode.clone(),
            time_lapse_gcode: i.time_lapse_gcode.clone(),
            change_extrusion_role_gcode: i.change_extrusion_role_gcode.clone(),
            printing_by_object_gcode: i.printing_by_object_gcode.clone(),
        }
    } else {
        build_empty_idex()
    };
    // Hold the Arc alive through the C++ call. `addr` is a raw read-only
    // pointer to the AtomicBool's byte; the watcher thread inside the
    // bridge will dereference it. Drop happens after `run_slice`
    // returns, which is guaranteed to be after the watcher joins.
    let cancel_ref = p.cancel.clone();
    let cancel_addr = cancel_ref.as_ref().map(CancelHandle::raw_addr).unwrap_or(0);
    // The C++ bridge assembles the Slic3r::Model from `settings.objects`
    // (per-object input_path, transform, paint masks, layer profile/ranges),
    // so there are no bare per-object params on the FFI call anymore. An
    // empty `objects` vec is reported by the bridge as a slice error.
    let outcome = ffi_bridge::run_slice(p.output_path, settings, idex, bbox, cancel_addr);
    drop(cancel_ref);
    drop(staged_preset);
    SliceResult {
        success: outcome.success,
        canceled: outcome.canceled,
        error_message: outcome.error_message,
        total_layers: outcome.total_layers,
        total_segments: outcome.total_segments,
        suggested_filename: outcome.suggested_filename,
        warnings: outcome.warnings,
    }
}

fn bridge_object_transform(m: [f32; 16]) -> BridgeObjectTransform {
    BridgeObjectTransform {
        m00: m[0],
        m01: m[1],
        m02: m[2],
        m03: m[3],
        m04: m[4],
        m05: m[5],
        m06: m[6],
        m07: m[7],
        m08: m[8],
        m09: m[9],
        m10: m[10],
        m11: m[11],
        m12: m[12],
        m13: m[13],
        m14: m[14],
        m15: m[15],
    }
}

fn build_empty_idex() -> ffi_bridge::BridgeIdexSettings {
    ffi_bridge::BridgeIdexSettings {
        extruder_offset_x: Vec::new(),
        extruder_offset_y: Vec::new(),
        nozzle_diameter: Vec::new(),
        retraction_length: Vec::new(),
        retraction_speed: Vec::new(),
        retract_restart_extra: Vec::new(),
        deretraction_speed: Vec::new(),
        retraction_minimum_travel: Vec::new(),
        retract_when_changing_layer: Vec::new(),
        wipe: Vec::new(),
        wipe_distance: Vec::new(),
        retract_before_wipe_pct: Vec::new(),
        retract_length_toolchange: Vec::new(),
        z_hop: Vec::new(),
        z_hop_types: Vec::new(),
        travel_slope_deg: Vec::new(),
        retract_lift_above: Vec::new(),
        retract_lift_below: Vec::new(),
        retract_lift_enforce: Vec::new(),
        min_layer_height: Vec::new(),
        max_layer_height: Vec::new(),
        filament_type: Vec::new(),
        filament_colour: Vec::new(),
        nozzle_temperature_initial_layer: Vec::new(),
        nozzle_temperature: Vec::new(),
        bed_temperature_initial_layer_single: Vec::new(),
        bed_temperature: Vec::new(),
        change_filament_gcode: String::new(),
        machine_pause_gcode: String::new(),
        time_lapse_gcode: String::new(),
        change_extrusion_role_gcode: String::new(),
        printing_by_object_gcode: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_string_present() {
        let v = version();
        assert!(!v.is_empty(), "ffi version should be non-empty");
    }

    #[test]
    fn slice_params_default_extruder_count_is_one() {
        let p = SliceParams {
            output_path: "/tmp/x.gcode",
            layer_height: 0.2,
            wall_loops: 0,
            sparse_infill_density_pct: -1.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: Vec::new(),
            bbox_min: [0.0; 3],
            bbox_max: [10.0; 3],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: "x.stl".to_string(),
                part_index: -1,
                object_transform: IDENTITY_OBJECT_TRANSFORM,
                seam_facets_rle: String::new(),
                support_facets_rle: String::new(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: None,
            idex: None,
            context: SliceContext::default(),
        };
        assert!(p.idex.is_none(), "default IDEX is None == single extruder");
    }

    #[test]
    fn run_slice_refuses_a_producer_line_with_a_line_break() {
        let params = SliceParams {
            output_path: "/tmp/x.gcode",
            layer_height: 0.2,
            wall_loops: 0,
            sparse_infill_density_pct: -1.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: Vec::new(),
            bbox_min: [0.0; 3],
            bbox_max: [10.0; 3],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: "x.stl".to_string(),
                part_index: -1,
                object_transform: IDENTITY_OBJECT_TRANSFORM,
                seam_facets_rle: String::new(),
                support_facets_rle: String::new(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: None,
            idex: None,
            context: SliceContext {
                producer_line: Some("Protype Slicer 1\nG28".into()),
                ..SliceContext::default()
            },
        };
        let r = run_slice(params);
        assert!(!r.success);
        assert!(
            r.error_message.contains("invalid producer_line"),
            "{}",
            r.error_message
        );
    }

    #[cfg(feature = "with-libslic3r")]
    #[test]
    fn version_reports_libslic3r_when_feature_on() {
        let v = version();
        assert!(
            v.starts_with("ffi-libslic3r-"),
            "expected libslic3r-prefixed version under feature gate, got {v:?}"
        );
    }

    #[cfg(not(feature = "with-libslic3r"))]
    #[test]
    fn version_reports_stub_when_feature_off() {
        let v = version();
        assert!(
            v.starts_with("ffi-stub-"),
            "expected stub-prefixed version with feature off, got {v:?}"
        );
    }

    /// Locate a usable test STL inside the upstream OrcaSlicer submodule.
    /// `OrcaToleranceTest.stl` is a known-good calibration mesh that ships
    /// with the daemon repo — using it keeps the test fully offline.
    #[cfg(feature = "with-libslic3r")]
    fn fixture_stl() -> std::path::PathBuf {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        manifest
            .parent()
            .and_then(|p| p.parent())
            .map(|root| {
                root.join("upstream/libslic3r/resources/handy_models/OrcaToleranceTest.stl")
            })
            .expect("workspace root not resolvable from slicer-ffi/")
    }

    #[cfg(feature = "with-libslic3r")]
    #[test]
    fn libslic3r_slices_real_stl_to_gcode() {
        let stl = fixture_stl();
        assert!(
            stl.exists(),
            "fixture STL missing: {} (run `git submodule update --init upstream/libslic3r`)",
            stl.display()
        );

        let dir = std::env::temp_dir().join(format!(
            "ffi-libslic3r-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");

        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.2,
            wall_loops: 3,
            sparse_infill_density_pct: 20.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: vec![
                ConfigOverride {
                    key: "machine_start_gcode".to_string(),
                    value: "M140 S130 ; protype start bed\nG28 ; protype home\nG1 Z25 F5000 ; protype start lift\n".to_string(),
                },
                ConfigOverride {
                    key: "machine_end_gcode".to_string(),
                    value: "M104 S0\nG91\nG1 Z10 F600 ; protype end lift\nG90\nG1 X0 Y200 F6000 ; protype park\nM84\n".to_string(),
                },
                ConfigOverride {
                    key: "fan_min_speed".to_string(),
                    value: "35".to_string(),
                },
                ConfigOverride {
                    key: "fan_max_speed".to_string(),
                    value: "80".to_string(),
                },
                ConfigOverride {
                    key: "close_fan_the_first_x_layers".to_string(),
                    value: "3".to_string(),
                },
            ],
            // bbox is informational once libslic3r owns slicing; the bundled
            // preset's printable_area governs the actual build volume.
            bbox_min: [0.0, 0.0, 0.0],
            bbox_max: [256.0, 256.0, 250.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: stl.to_str().unwrap().to_string(),
                part_index: -1,
                object_transform: IDENTITY_OBJECT_TRANSFORM,
                seam_facets_rle: String::new(),
                support_facets_rle: String::new(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: None,
            idex: Some(IdexInputs {
                nozzle_diameter: vec![0.6],
                extruder_offset_xy: vec![[0.0, 0.0]],
                retraction_length: vec![1.2],
                retraction_speed: vec![45.0],
                retract_restart_extra: vec![0.08],
                ..IdexInputs::empty_for_count(1)
            }),
            context: SliceContext::default(),
        });
        assert!(r.success, "slice failed: {}", r.error_message);

        // total_layers is the layers the G-code prints. Day-4 audit measured
        // PrusaSlicer at 31 layers for this mesh at 0.2 mm — libslic3r should
        // land in the same ballpark.
        assert!(
            r.total_layers >= 30,
            "expected ≥30 sliced layers, got {} (mesh height ÷ layer_height should yield ~31)",
            r.total_layers
        );

        let body = std::fs::read_to_string(&out).expect("g-code file should exist");
        assert!(
            body.len() > 8 * 1024,
            "g-code file suspiciously small: {} bytes (expected ≥8 KB for a real slice)",
            body.len()
        );

        // OrcaSlicer's bundled `layer_change_gcode` template emits one
        // `M73 L<n>` progress line per layer (Marlin layer-progress message).
        // Counting these gives an independent layer-count check that doesn't
        // rely on bridge.cpp's metric path.
        let m73_layer_count = body
            .lines()
            .filter(|l| l.trim_start().starts_with("M73 L"))
            .count();
        assert!(
            m73_layer_count >= 30,
            "expected ≥30 M73 L progress lines (one per layer), got {m73_layer_count}"
        );

        // Proof the post-slice footer was written (would be missing if
        // export_gcode bailed after writing layer bodies).
        assert!(
            body.contains("estimated printing time"),
            "expected `estimated printing time` annotation in footer"
        );
        assert!(
            !body.lines().any(|l| {
                matches!(
                    l.split_whitespace().next(),
                    Some("G17") | Some("G2") | Some("G3")
                )
            }),
            "Protype firmware does not support G17/G2/G3; generated g-code must use G1 segments only"
        );
        assert!(
            body.contains("G1 Z10 F600 ; protype end lift")
                && body.contains("G1 X0 Y200 F6000 ; protype park"),
            "profile-provided machine_end_gcode should be preserved in exported g-code"
        );
        assert!(
            body.contains("M140 S130 ; protype start bed")
                && body.contains("G1 Z25 F5000 ; protype start lift"),
            "profile-provided machine_start_gcode should be preserved in exported g-code"
        );
        assert!(
            body.contains("; retraction_length = 1.2")
                && body.contains("; retraction_speed = 45")
                && body.contains("; retract_restart_extra = 0.08")
                && body.contains("; nozzle_diameter = 0.6"),
            "single-extruder resolved extruder settings should reach exported g-code"
        );
        assert!(
            first_positive_fan_command_layer_change_count(&body).is_none_or(|count| count >= 3),
            "part cooling fan must stay off through the first 3 layer changes"
        );

        // libslic3r should have rendered the recommended filename via
        // the bundled preset's `filename_format` (default Orca format
        // is `{input_filename_base}_{filament_type[initial_tool]}_{print_time}.gcode`).
        // Stem of the source STL is `OrcaToleranceTest`; the rest comes
        // from the preset (filament type) and print_statistics
        // (print_time), neither of which we want to overspecify in
        // assertions. Verify the structure: stem prefix + .gcode
        // suffix + non-trivial middle.
        assert!(
            r.suggested_filename.starts_with("OrcaToleranceTest"),
            "expected suggested_filename to start with input stem, got {:?}",
            r.suggested_filename
        );
        assert!(
            r.suggested_filename.ends_with(".gcode"),
            "expected .gcode suffix, got {:?}",
            r.suggested_filename
        );
        assert!(
            r.suggested_filename.len() > "OrcaToleranceTest.gcode".len(),
            "expected more than just `<stem>.gcode` (filament_type + print_time placeholders should expand), got {:?}",
            r.suggested_filename
        );

        // At least one extrusion move — proves we got past the start-gcode
        // ceremony into actual print moves.
        assert!(
            body.lines().any(|l| {
                let t = l.trim_start();
                t.starts_with("G1 ") && t.contains(" E")
            }),
            "expected at least one extrusion move (`G1 ... E…`) in g-code body"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(feature = "with-libslic3r")]
    fn first_positive_fan_command_layer_change_count(body: &str) -> Option<usize> {
        let mut layer_changes_seen = 0;
        for line in body.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with(";LAYER_CHANGE") {
                layer_changes_seen += 1;
                continue;
            }
            if !trimmed.starts_with("M106") {
                continue;
            }
            let has_positive_s = trimmed
                .split_whitespace()
                .find_map(|part| part.strip_prefix('S'))
                .and_then(|raw| raw.parse::<f32>().ok())
                .is_some_and(|value| value > 0.0);
            if has_positive_s {
                return Some(layer_changes_seen);
            }
        }
        None
    }

    #[cfg(feature = "with-libslic3r")]
    fn longest_skirt_extrusion_mm(body: &str) -> Option<f64> {
        let mut x = 0.0_f64;
        let mut y = 0.0_f64;
        let mut in_skirt = false;
        let mut longest: Option<f64> = None;

        for line in body.lines() {
            let trimmed = line.trim();
            if let Some(feature) = trimmed.strip_prefix(";TYPE:") {
                in_skirt = feature == "Skirt";
                continue;
            }
            if !trimmed.starts_with("G0 ") && !trimmed.starts_with("G1 ") {
                continue;
            }

            let mut next_x = x;
            let mut next_y = y;
            let mut extrusion = 0.0_f64;
            for word in trimmed.split_whitespace().skip(1) {
                let Some((axis, raw)) = word.split_at_checked(1) else {
                    continue;
                };
                let Ok(value) = raw.parse::<f64>() else {
                    continue;
                };
                match axis {
                    "X" => next_x = value,
                    "Y" => next_y = value,
                    "E" => extrusion = value,
                    _ => {}
                }
            }
            if in_skirt && extrusion > 0.0 {
                let distance = (next_x - x).hypot(next_y - y);
                longest = Some(longest.map_or(distance, |prev| prev.max(distance)));
            }
            x = next_x;
            y = next_y;
        }

        longest
    }

    #[cfg(feature = "with-libslic3r")]
    #[test]
    fn libslic3r_rejects_floating_object_without_supports() {
        let stl = fixture_stl();
        assert!(
            stl.exists(),
            "fixture STL missing: {} (run `git submodule update --init upstream/libslic3r`)",
            stl.display()
        );

        let dir = std::env::temp_dir().join(format!(
            "ffi-libslic3r-transform-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");
        let object_transform = [
            0.7435386,
            4.094564e-17,
            0.668693,
            0.0, //
            0.0,
            1.0,
            -6.123234e-17,
            0.0, //
            -0.668693,
            4.552861e-17,
            0.7435386,
            0.0, //
            200.0,
            200.0,
            45.562042,
            1.0, //
        ];

        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.2,
            wall_loops: 3,
            sparse_infill_density_pct: 20.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: Vec::new(),
            bbox_min: [120.0, 120.0, 0.0],
            bbox_max: [230.0, 230.0, 100.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: stl.to_str().unwrap().to_string(),
                part_index: -1,
                object_transform,
                seam_facets_rle: String::new(),
                support_facets_rle: String::new(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: None,
            idex: None,
            context: SliceContext::default(),
        });

        assert!(
            !r.success,
            "floating object without supports must be rejected"
        );
        assert!(
            r.error_message.contains("must be on the build plate")
                && r.error_message.contains("enable supports"),
            "unexpected error: {}",
            r.error_message
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(feature = "with-libslic3r")]
    #[test]
    fn libslic3r_slices_floating_object_with_two_material_supports() {
        let stl = fixture_stl();
        assert!(
            stl.exists(),
            "fixture STL missing: {} (run `git submodule update --init upstream/libslic3r`)",
            stl.display()
        );

        let dir = std::env::temp_dir().join(format!(
            "ffi-libslic3r-transform-support-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");
        let object_transform = [
            0.7435386,
            4.094564e-17,
            0.668693,
            0.0, //
            0.0,
            1.0,
            -6.123234e-17,
            0.0, //
            -0.668693,
            4.552861e-17,
            0.7435386,
            0.0, //
            200.0,
            200.0,
            45.562042,
            1.0, //
        ];

        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.2,
            wall_loops: 3,
            sparse_infill_density_pct: 20.0,
            sparse_infill_pattern: String::new(),
            enable_support: true,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: vec![
                ConfigOverride {
                    key: "support_type".to_string(),
                    value: "tree(auto)".to_string(),
                },
                ConfigOverride {
                    key: "support_filament".to_string(),
                    value: "2".to_string(),
                },
                ConfigOverride {
                    key: "support_interface_filament".to_string(),
                    value: "2".to_string(),
                },
                ConfigOverride {
                    key: "filament_shrink".to_string(),
                    value: "100,99".to_string(),
                },
                ConfigOverride {
                    key: "filament_shrinkage_compensation_z".to_string(),
                    value: "100,99".to_string(),
                },
                ConfigOverride {
                    key: "enable_prime_tower".to_string(),
                    value: "1".to_string(),
                },
                ConfigOverride {
                    key: "skirt_loops".to_string(),
                    value: "3".to_string(),
                },
                ConfigOverride {
                    key: "brim_type".to_string(),
                    value: "outer_and_inner".to_string(),
                },
                ConfigOverride {
                    key: "brim_width".to_string(),
                    value: "12".to_string(),
                },
                ConfigOverride {
                    key: "raft_first_layer_expansion".to_string(),
                    value: "2.2".to_string(),
                },
                ConfigOverride {
                    key: "wipe_tower_x".to_string(),
                    value: "15".to_string(),
                },
                ConfigOverride {
                    key: "wipe_tower_y".to_string(),
                    value: "220".to_string(),
                },
                ConfigOverride {
                    key: "machine_end_gcode".to_string(),
                    value: "M140 S0 ; custom end marker".to_string(),
                },
            ],
            bbox_min: [120.0, 120.0, 0.0],
            bbox_max: [230.0, 230.0, 100.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: stl.to_str().unwrap().to_string(),
                part_index: -1,
                object_transform,
                seam_facets_rle: String::new(),
                support_facets_rle: String::new(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: None,
            idex: Some(IdexInputs {
                extruder_count: 2,
                nozzle_diameter: vec![0.4, 0.4],
                extruder_offset_xy: vec![[0.0, 0.0], [18.0, 0.0]],
                ..IdexInputs::empty_for_count(2)
            }),
            context: SliceContext::default(),
        });

        assert!(
            r.success,
            "supported floating slice failed: {}",
            r.error_message
        );
        let body = std::fs::read_to_string(&out).expect("g-code file should exist");
        assert!(
            body.lines().any(|l| {
                let t = l.trim_start();
                t.starts_with("G1 ") && t.contains(" E")
            }),
            "expected extrusion moves in transformed g-code"
        );
        assert!(
            body.to_ascii_lowercase().contains("support"),
            "expected generated g-code to include support toolpaths"
        );
        assert!(
            body.lines().any(|line| line.trim() == "T1"),
            "expected the second material tool to print supports"
        );
        assert!(
            body.contains("T0\nM109 S") || body.contains("T0\r\nM109 S"),
            "the newly selected model hotend must reach temperature before extrusion"
        );
        assert!(
            body.contains("G1 E6 F300 ; restore pressure while the tool is parked"),
            "a tool returning from standby must restore pressure before printing the model"
        );
        assert!(
            body.matches("G1 E6 F300 ; restore pressure while the tool is parked")
                .count()
                >= 2,
            "both the initial support tool and the later model tool must restore pressure"
        );
        let initial_support_tool = body
            .find("T1\n")
            .or_else(|| body.find("T1\r\n"))
            .map(|start| {
                &body[start
                    ..body[start..]
                        .find(";LAYER_CHANGE")
                        .map_or(body.len(), |end| start + end)]
            })
            .expect("expected the support tool to be selected before the first layer");
        assert!(
            initial_support_tool.contains("restore pressure while the tool is parked"),
            "the initially active support tool must restore pressure before printing the skirt"
        );
        let generated_skirt_loops = body
            .lines()
            .find_map(|line| line.strip_prefix("; skirt_loops = "))
            .and_then(|value| value.trim().parse::<i32>().ok())
            .expect("expected skirt_loops in the config footer");
        assert!(
            generated_skirt_loops == 3,
            "brim width must not be converted into skirt loops"
        );
        assert!(
            body.contains("M104 T0 S0 ; turn off model hotend")
                && body.contains("M104 T1 S0 ; turn off support hotend")
                && body.contains("M140 S0 ; custom end marker"),
            "both IDEX hotends must be shut down without discarding the profile end macro"
        );
        assert!(
            body.contains("; single_extruder_multi_material = 0"),
            "a two-carriage IDEX job must not inherit the bundled single-hotend MMU mode"
        );
        assert!(
            body.contains("; raft_first_layer_expansion = 2.2"),
            "brim width must not overwrite the independently configured support expansion"
        );
        assert!(
            body.contains("; brim_type = outer_and_inner") && body.contains("; brim_width = 12"),
            "the native Orca support brim must remain enabled for a floating object"
        );
        assert!(
            body.contains(";TYPE:Brim"),
            "the floating supported object must emit actual brim extrusion, not only a skirt"
        );
        assert!(
            body.contains(";TYPE:Prime tower"),
            "an explicitly enabled prime tower must remain available"
        );
        let longest_skirt = longest_skirt_extrusion_mm(&body)
            .expect("the generated support-brim loops should produce extrusion");
        assert!(
            longest_skirt < 100.0,
            "model skirt must not connect to the distant prime tower; longest extrusion was {longest_skirt:.1} mm"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(feature = "with-libslic3r")]
    #[test]
    fn libslic3r_count_two_with_default_per_slot_succeeds() {
        let stl = fixture_stl();
        if !stl.exists() {
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "ffi-idex-smoke-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");

        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.2,
            wall_loops: 3,
            sparse_infill_density_pct: 20.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: Vec::new(),
            bbox_min: [0.0; 3],
            bbox_max: [256.0, 256.0, 250.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: stl.to_str().unwrap().to_string(),
                part_index: -1,
                object_transform: IDENTITY_OBJECT_TRANSFORM,
                seam_facets_rle: String::new(),
                support_facets_rle: String::new(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: None,
            idex: Some(IdexInputs {
                extruder_count: 2,
                // Resize-alone path: the agent's flatten would populate
                // these, but here we exercise libslic3r default padding.
                nozzle_diameter: vec![0.4, 0.4],
                extruder_offset_xy: vec![[0.0, 0.0], [18.0, 0.0]],
                ..IdexInputs::empty_for_count(2)
            }),
            context: SliceContext::default(),
        });
        assert!(
            r.success,
            "count=2 slice with default per-slot config should succeed: {}",
            r.error_message
        );
        let body = std::fs::read_to_string(&out).unwrap();
        assert!(
            body.contains("filament_diameter = 1.75,1.75")
                || body.contains("filament_diameter = 1.75;1.75"),
            "count=2 slice must keep a two-slot filament_diameter vector so ToolOrdering does not clamp to T0"
        );
        assert!(
            body.contains("; enable_prime_tower = 0")
                && !body.contains(";TYPE:Prime tower"),
            "the bundled X1C preset must not create a prime tower unless the process explicitly enables it"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `; generated by OrcaSlicer <d.d.d> on <dddd>-<dd>-<dd> at <dd>:<dd>:<dd>`,
    /// parsed by hand so the crate needs no regex dependency.
    #[cfg(feature = "with-libslic3r")]
    fn is_orca_banner(line: &str) -> bool {
        fn digits(s: &str, n: usize) -> bool {
            s.len() == n && s.bytes().all(|b| b.is_ascii_digit())
        }
        let Some(rest) = line.strip_prefix("; generated by OrcaSlicer ") else {
            return false;
        };
        let Some((version, stamp)) = rest.split_once(" on ") else {
            return false;
        };
        let parts: Vec<&str> = version.split('.').collect();
        let version_ok = parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        let Some((date, time)) = stamp.split_once(" at ") else {
            return false;
        };
        let date: Vec<&str> = date.split('-').collect();
        let time: Vec<&str> = time.split(':').collect();
        version_ok
            && date.len() == 3
            && digits(date[0], 4)
            && digits(date[1], 2)
            && digits(date[2], 2)
            && time.len() == 3
            && time.iter().all(|t| digits(t, 2))
    }

    /// Slices the fixture into a fresh temp directory and returns the file.
    #[cfg(feature = "with-libslic3r")]
    fn slice_fixture_with(
        tag: &str,
        context: SliceContext,
        config_overrides: Vec<ConfigOverride>,
    ) -> Option<String> {
        let stl = fixture_stl();
        if !stl.exists() {
            return None;
        }
        let dir = std::env::temp_dir().join(format!(
            "ffi-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");

        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.2,
            wall_loops: 3,
            sparse_infill_density_pct: 20.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides,
            bbox_min: [0.0; 3],
            bbox_max: [256.0, 256.0, 250.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: stl.to_str().unwrap().to_string(),
                part_index: -1,
                object_transform: IDENTITY_OBJECT_TRANSFORM,
                seam_facets_rle: String::new(),
                support_facets_rle: String::new(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: None,
            idex: None,
            context,
        });
        assert!(r.success, "{}", r.error_message);
        let body = std::fs::read_to_string(&out).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        Some(body)
    }

    #[cfg(feature = "with-libslic3r")]
    #[test]
    fn libslic3r_gcode_header_names_orca_then_protype_slicer() {
        let Some(body) = slice_fixture_with("orca-header", SliceContext::default(), Vec::new())
        else {
            return;
        };
        let lines: Vec<&str> = body.lines().take(3).collect();
        assert_eq!(lines[0], "; HEADER_BLOCK_START");
        assert!(is_orca_banner(lines[1]), "{}", lines[1]);
        assert!(!lines[1].contains("1970-01-01"), "{}", lines[1]);
        assert_eq!(
            lines[2],
            format!("; {}", gcode_header::DEFAULT_PRODUCER_LINE)
        );
        assert!(!body.contains("; generated by Protype Slicer"));

        let Some(second) = slice_fixture_with(
            "orca-header-named",
            SliceContext {
                producer_line: Some("Protype Slicer 0.1.38".into()),
                ..SliceContext::default()
            },
            Vec::new(),
        ) else {
            return;
        };
        assert_eq!(second.lines().nth(2), Some("; Protype Slicer 0.1.38"));
    }

    #[cfg(feature = "with-libslic3r")]
    #[test]
    fn btt_tft_thumbnails_leave_neither_header_nor_producer_line() {
        let Some(body) = slice_fixture_with(
            "btt-header",
            SliceContext::default(),
            vec![ConfigOverride {
                key: "thumbnails".into(),
                value: "48x48/BTT_TFT".into(),
            }],
        ) else {
            return;
        };
        assert!(!body.contains("; HEADER_BLOCK_START"));
        assert!(!body.contains("; Protype Slicer "));
    }

    #[cfg(feature = "with-libslic3r")]
    #[test]
    fn libslic3r_pressure_advance_vector_uses_first_slot_value() {
        let stl = fixture_stl();
        if !stl.exists() {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "ffi-pa-vector-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");

        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.2,
            wall_loops: 3,
            sparse_infill_density_pct: 20.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: vec![
                ConfigOverride {
                    key: "enable_pressure_advance".to_string(),
                    value: "1,1".to_string(),
                },
                ConfigOverride {
                    key: "pressure_advance".to_string(),
                    value: "0.039,0.04".to_string(),
                },
            ],
            bbox_min: [0.0; 3],
            bbox_max: [256.0, 256.0, 250.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: stl.to_str().unwrap().to_string(),
                part_index: -1,
                object_transform: IDENTITY_OBJECT_TRANSFORM,
                seam_facets_rle: String::new(),
                support_facets_rle: String::new(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: None,
            idex: Some(IdexInputs {
                extruder_count: 2,
                extruder_offset_xy: vec![[0.0, 0.0], [17.5, 0.0]],
                nozzle_diameter: vec![0.4, 0.4],
                ..IdexInputs::empty_for_count(2)
            }),
            context: SliceContext::default(),
        });
        assert!(r.success, "{}", r.error_message);

        let body = std::fs::read_to_string(&out).unwrap();
        assert!(
            body.contains("SET_PRESSURE_ADVANCE ADVANCE=0.039;") || body.contains("M900 K0.039;"),
            "expected slot 0 PA 0.039 in G-code, got PA lines:\n{}",
            body.lines()
                .filter(|line| line.contains("PRESSURE_ADVANCE") || line.contains("M900 K"))
                .take(8)
                .collect::<Vec<_>>()
                .join("\n")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(feature = "with-libslic3r")]
    #[test]
    fn libslic3r_count_two_extruder_offset_emitted_in_gcode_header() {
        let stl = fixture_stl();
        if !stl.exists() {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "ffi-idex-offset-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");

        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.2,
            wall_loops: 3,
            sparse_infill_density_pct: 20.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: Vec::new(),
            bbox_min: [0.0; 3],
            bbox_max: [256.0, 256.0, 250.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: stl.to_str().unwrap().to_string(),
                part_index: -1,
                object_transform: IDENTITY_OBJECT_TRANSFORM,
                seam_facets_rle: String::new(),
                support_facets_rle: String::new(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: None,
            idex: Some(IdexInputs {
                extruder_count: 2,
                extruder_offset_xy: vec![[0.0, 0.0], [17.5, 0.0]],
                nozzle_diameter: vec![0.4, 0.6],
                ..IdexInputs::empty_for_count(2)
            }),
            context: SliceContext::default(),
        });
        assert!(r.success, "{}", r.error_message);

        let body = std::fs::read_to_string(&out).unwrap();
        assert!(
            body.contains("; extruder_offset = 0x0,17.5x0")
                || body.contains("; extruder_offset = 0,0,17.5,0")
                || body.contains("17.5"),
            "offset for slot 1 should appear in g-code config footer; got first 200 chars after `extruder_offset`: {:?}",
            body.split("extruder_offset")
                .nth(1)
                .map(|s| s.chars().take(200).collect::<String>())
        );
        assert!(
            body.contains("nozzle_diameter = 0.4,0.6") || body.contains("0.4,0.6"),
            "nozzle_diameter should serialize as 0.4,0.6"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(feature = "with-libslic3r")]
    #[test]
    fn libslic3r_count_two_per_slot_vectors_emitted_in_gcode_header() {
        let stl = fixture_stl();
        if !stl.exists() {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "ffi-idex-vectors-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");

        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.2,
            wall_loops: 3,
            sparse_infill_density_pct: 20.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: Vec::new(),
            bbox_min: [0.0; 3],
            bbox_max: [256.0, 256.0, 250.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: stl.to_str().unwrap().to_string(),
                part_index: -1,
                object_transform: IDENTITY_OBJECT_TRANSFORM,
                seam_facets_rle: String::new(),
                support_facets_rle: String::new(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: None,
            idex: Some(IdexInputs {
                extruder_count: 2,
                extruder_offset_xy: vec![[0.0, 0.0], [18.0, 0.0]],
                nozzle_diameter: vec![0.4, 0.4],
                retraction_length: vec![0.8, 1.2],
                retract_when_changing_layer: vec![true, false],
                z_hop: vec![0.2, 0.4],
                z_hop_types: vec!["normal".into(), "slope".into()],
                filament_type: vec!["PLA".into(), "PETG".into()],
                filament_colour: vec!["#018001".into(), "#FF0080".into()],
                nozzle_temperature: vec![210, 240],
                bed_temperature: vec![55, 70],
                ..IdexInputs::empty_for_count(2)
            }),
            context: SliceContext::default(),
        });
        assert!(r.success, "{}", r.error_message);

        let body = std::fs::read_to_string(&out).unwrap();
        assert!(
            body.contains("filament_type = PLA;PETG") || body.contains("filament_type = PLA,PETG"),
            "filament_type should serialize with both slot values"
        );
        assert!(
            body.contains("filament_colour = #018001;#FF0080")
                || body.contains("filament_colour = #018001,#FF0080"),
            "filament_colour should serialize with both slot colors"
        );
        assert!(
            body.contains("nozzle_temperature = 210,240"),
            "nozzle_temperature should serialize as 210,240"
        );
        assert!(
            body.contains("retraction_length = 0.8,1.2"),
            "retraction_length should serialize as 0.8,1.2"
        );
        assert!(
            body.contains("z_hop = 0.2,0.4"),
            "z_hop should serialize as 0.2,0.4"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(feature = "with-libslic3r")]
    #[test]
    fn libslic3r_count_two_emits_toolchange_gcode() {
        let stl = fixture_stl();
        if !stl.exists() {
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "ffi-idex-tc-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");

        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.2,
            wall_loops: 3,
            sparse_infill_density_pct: 20.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: Vec::new(),
            bbox_min: [0.0; 3],
            bbox_max: [256.0, 256.0, 250.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: stl.to_str().unwrap().to_string(),
                part_index: -1,
                object_transform: IDENTITY_OBJECT_TRANSFORM,
                seam_facets_rle: String::new(),
                support_facets_rle: String::new(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: None,
            idex: Some(IdexInputs {
                extruder_count: 2,
                extruder_offset_xy: vec![[0.0, 0.0], [18.0, 0.0]],
                nozzle_diameter: vec![0.4, 0.4],
                change_filament_gcode: "T{next_extruder} ; protype-tc-marker".to_string(),
                ..IdexInputs::empty_for_count(2)
            }),
            context: SliceContext::default(),
        });
        assert!(r.success, "{}", r.error_message);
        let body = std::fs::read_to_string(&out).unwrap();
        assert!(
            body.contains("protype-tc-marker"),
            "agent-supplied change_filament_gcode should be in the config footer"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(feature = "with-libslic3r")]
    #[test]
    fn idex_fixture_parses_through_libslic3r() {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let preset = manifest
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("tests/fixtures/idex/x1c-dual-pla-petg.json");
        assert!(
            preset.exists(),
            "missing IDEX fixture: {}",
            preset.display()
        );
    }

    #[test]
    fn cancel_handle_is_idempotent() {
        let h = CancelHandle::new();
        assert!(!h.is_canceled());
        assert!(h.cancel(), "first cancel() returns true");
        assert!(h.is_canceled());
        assert!(!h.cancel(), "second cancel() is a noop, returns false");
    }

    #[test]
    fn cancel_before_slice_aborts_immediately() {
        // Pre-flipped flag must short-circuit the slice without writing
        // a g-code file. Works under both the rectangle stub and the
        // libslic3r path: the watcher polls the same byte, sees it set,
        // and either skips work entirely (stub) or calls print.cancel()
        // before any meaningful loop iteration (real path).
        let dir = std::env::temp_dir().join(format!(
            "ffi-cancel-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");

        let cancel = CancelHandle::new();
        cancel.cancel();
        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.4,
            wall_loops: 3,
            sparse_infill_density_pct: 20.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: Vec::new(),
            bbox_min: [-10.0, -10.0, 0.0],
            bbox_max: [10.0, 10.0, 20.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: "fake.stl".to_string(),
                part_index: -1,
                object_transform: IDENTITY_OBJECT_TRANSFORM,
                seam_facets_rle: String::new(),
                support_facets_rle: String::new(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: Some(cancel),
            idex: None,
            context: SliceContext::default(),
        });
        assert!(!r.success, "pre-canceled slice must not report success");
        assert!(r.canceled, "outcome must flag canceled=true");
        std::fs::remove_dir_all(&dir).ok();
    }

    // Stub G-code contract: with libslic3r linked a mesh that is not on disk
    // fails the slice instead, and "fake.stl" was never written (the e2e for
    // that is slicer-rpc's `slice_missing_source_e2e`).
    #[cfg(not(feature = "with-libslic3r"))]
    #[test]
    fn stub_slice_writes_gcode() {
        let dir = std::env::temp_dir().join(format!(
            "ffi-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");
        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.4,
            wall_loops: 3,
            sparse_infill_density_pct: 20.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: Vec::new(),
            bbox_min: [-10.0, -10.0, 0.0],
            bbox_max: [10.0, 10.0, 20.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: "fake.stl".to_string(),
                part_index: -1,
                object_transform: IDENTITY_OBJECT_TRANSFORM,
                seam_facets_rle: String::new(),
                support_facets_rle: String::new(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: None,
            idex: None,
            context: SliceContext::default(),
        });
        assert!(r.success, "slice failed: {}", r.error_message);
        // 20mm tall at 0.4mm = 50 layers.
        assert_eq!(r.total_layers, 50);
        let body = std::fs::read_to_string(&out).unwrap();
        assert!(body.contains(";LAYER:0"));
        assert!(body.contains(";LAYER:49"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Verify that `seam_facets_rle` is a first-class field in the
    /// `SliceParams` API contract. Passing a non-empty (but trivially
    /// short) base64 string exercises the code-path that reaches the
    /// C++ bridge; the bridge gracefully ignores a blob that cannot be
    /// decoded into a valid `TriangleSplittingData` (under stub mode the
    /// field is stored but not interpreted). The test is intentionally
    /// independent of `with-libslic3r` — it only checks that the field
    /// is accepted by the constructor without a compile error.
    #[test]
    fn run_slice_accepts_seam_facets_rle() {
        let dir = std::env::temp_dir().join(format!(
            "ffi-seam-rle-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");
        let cancel = CancelHandle::new();
        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.2,
            wall_loops: 2,
            sparse_infill_density_pct: 15.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: vec![],
            bbox_min: [0.0, 0.0, 0.0],
            bbox_max: [20.0, 20.0, 20.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: "fake.stl".to_string(),
                part_index: -1,
                object_transform: IDENTITY_OBJECT_TRANSFORM,
                // "AAAAAA==" is a valid base64 string (decodes to 4 zero
                // bytes). The bridge attempts to deserialize this as a
                // cereal-binary TriangleSplittingData; the short payload
                // causes a decode failure which is logged and ignored —
                // slice proceeds with the default seam. Under the stub path
                // the field is accepted verbatim.
                seam_facets_rle: "AAAAAA==".to_string(),
                support_facets_rle: String::new(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: Some(cancel),
            idex: None,
            context: SliceContext::default(),
        });
        // Under both the stub and libslic3r paths the slice either
        // succeeds or fails for reasons unrelated to seam_facets_rle
        // (e.g. "fake.stl" does not exist on the libslic3r path). We
        // only assert that the function returns without panicking and
        // that the seam_facets_rle field was accepted at the type level.
        let _ = r;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(not(feature = "with-libslic3r"))]
    #[test]
    fn run_slice_decodes_ui_seam_facets_rle() {
        let dir = std::env::temp_dir().join(format!(
            "ffi-seam-ui-rle-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");

        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.2,
            wall_loops: 2,
            sparse_infill_density_pct: 15.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: vec![],
            bbox_min: [0.0, 0.0, 0.0],
            bbox_max: [20.0, 20.0, 20.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: "fake.stl".to_string(),
                part_index: -1,
                object_transform: IDENTITY_OBJECT_TRANSFORM,
                // UI seam painter wire format:
                // [value: u8][run_length: u32 LE]...
                // 1 enforcer followed by 2 blockers.
                seam_facets_rle: "AQEAAAACAgAAAA==".to_string(),
                support_facets_rle: String::new(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: None,
            idex: None,
            context: SliceContext::default(),
        });
        assert!(r.success, "slice failed: {}", r.error_message);

        let body = std::fs::read_to_string(&out).expect("g-code file should exist");
        assert!(
            body.contains("; seam_facets_decoded: enforcers=1 blockers=2"),
            "UI RLE seam paint should be decoded before reaching slicing; header:\n{}",
            body.lines().take(20).collect::<Vec<_>>().join("\n")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_slice_decodes_ui_support_facets_rle() {
        let dir = std::env::temp_dir().join(format!(
            "ffi-support-ui-rle-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");

        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.2,
            wall_loops: 2,
            sparse_infill_density_pct: 15.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: vec![],
            bbox_min: [0.0, 0.0, 0.0],
            bbox_max: [20.0, 20.0, 20.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: "fake.stl".to_string(),
                part_index: -1,
                object_transform: IDENTITY_OBJECT_TRANSFORM,
                seam_facets_rle: String::new(),
                // UI support painter wire format mirrors seam:
                // [value: u8][run_length: u32 LE]... 1 enforcer, 2 blockers.
                support_facets_rle: "AQEAAAACAgAAAA==".to_string(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: None,
            idex: None,
            context: SliceContext::default(),
        });
        if r.success {
            let body = std::fs::read_to_string(&out).expect("g-code file should exist");
            assert!(
                body.contains("; support_facets_decoded: enforcers=1 blockers=2"),
                "expected support facet decode comment, got:\n{}",
                body.lines().take(30).collect::<Vec<_>>().join("\n"),
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Verify that `support_facets_rle` is a first-class field in the
    /// `SliceParams` API contract — passing a non-empty blob must be
    /// accepted at the type level (the bridge decodes it internally).
    #[test]
    fn run_slice_accepts_support_facets_rle() {
        let dir = std::env::temp_dir().join(format!(
            "ffi-support-rle-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");
        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.2,
            wall_loops: 2,
            sparse_infill_density_pct: 15.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: vec![],
            bbox_min: [0.0, 0.0, 0.0],
            bbox_max: [20.0, 20.0, 20.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: "fake.stl".to_string(),
                part_index: -1,
                object_transform: IDENTITY_OBJECT_TRANSFORM,
                seam_facets_rle: String::new(),
                support_facets_rle: "AQEC".to_string(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: Vec::new(),
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: None,
            idex: None,
            context: SliceContext::default(),
        });
        // The field is accepted at the type level; success/failure depends
        // on stub-vs-libslic3r build, not on this field.
        let _ = r;
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Variable-layer-height profile is a first-class field on
    /// `SliceParams`: a non-empty profile must be accepted by the bridge
    /// and the count echoed into the G-code footer so the daemon can
    /// confirm the value reached libslic3r.
    #[cfg(not(feature = "with-libslic3r"))]
    #[test]
    fn run_slice_accepts_a_layer_height_profile() {
        let dir = std::env::temp_dir().join(format!(
            "ffi-lhp-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");
        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.2,
            wall_loops: 2,
            sparse_infill_density_pct: 15.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: vec![],
            bbox_min: [0.0, 0.0, 0.0],
            bbox_max: [20.0, 20.0, 20.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![ObjectInputs {
                input_path: "fake.stl".to_string(),
                part_index: -1,
                object_transform: IDENTITY_OBJECT_TRANSFORM,
                seam_facets_rle: String::new(),
                support_facets_rle: String::new(),
                fan_facets_rle: None,
                filament_facets_rle: None,
                layer_height_profile: vec![0.0, 0.2, 5.0, 0.1, 10.0, 0.2],
                layer_ranges: Vec::new(),
                object_overrides: Vec::new(),
            }],
            cancel: None,
            idex: None,
            context: SliceContext::default(),
        });
        assert!(r.success, "expected slice to succeed");
        let body = std::fs::read_to_string(&out).expect("g-code file should exist");
        assert!(
            body.contains("; layer_height_profile_points: 3"),
            "expected layer-height profile echo with 3 control points, got:\n{}",
            body.lines().take(40).collect::<Vec<_>>().join("\n"),
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An object whose input file does not exist: the stub echoes the paint
    /// inputs it received, and a libslic3r build fails the slice instead.
    #[cfg(not(feature = "with-libslic3r"))]
    fn stub_object() -> ObjectInputs {
        ObjectInputs {
            input_path: "fake.stl".to_string(),
            part_index: -1,
            object_transform: IDENTITY_OBJECT_TRANSFORM,
            seam_facets_rle: String::new(),
            support_facets_rle: String::new(),
            fan_facets_rle: None,
            filament_facets_rle: None,
            layer_height_profile: Vec::new(),
            layer_ranges: Vec::new(),
            object_overrides: Vec::new(),
        }
    }

    /// The stub G-code `run_slice` writes for `object`.
    #[cfg(not(feature = "with-libslic3r"))]
    fn stub_slice_gcode(tag: &str, object: ObjectInputs) -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ffi-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.gcode");
        let r = run_slice(SliceParams {
            output_path: out.to_str().unwrap(),
            layer_height: 0.2,
            wall_loops: 2,
            sparse_infill_density_pct: 15.0,
            sparse_infill_pattern: String::new(),
            enable_support: false,
            print_speed_mm_s: 0.0,
            top_shell_thickness_mm: 0.0,
            wall_generator: String::new(),
            config_overrides: vec![],
            bbox_min: [0.0, 0.0, 0.0],
            bbox_max: [20.0, 20.0, 20.0],
            place_plate_on_bed: false,
            calibration_test: None,
            objects: vec![object],
            cancel: None,
            idex: None,
            context: SliceContext::default(),
        });
        assert!(r.success, "slice failed: {}", r.error_message);
        let body = std::fs::read_to_string(&out).expect("g-code file should exist");
        std::fs::remove_dir_all(&dir).ok();
        body
    }

    #[cfg(not(feature = "with-libslic3r"))]
    fn header(gcode: &str) -> String {
        gcode.lines().take(40).collect::<Vec<_>>().join("\n")
    }

    #[cfg(not(feature = "with-libslic3r"))]
    #[test]
    fn run_slice_decodes_ui_fan_facets_rle_up_to_147() {
        // One facet at 147 (every line type, 100 %), then two unpainted:
        // base64 of [147][1 u32 LE][0][2 u32 LE].
        let gcode = stub_slice_gcode(
            "fan-rle",
            ObjectInputs {
                fan_facets_rle: Some("kwEAAAAAAgAAAA==".to_string()),
                ..stub_object()
            },
        );
        assert!(
            gcode.contains("; fan_facets_decoded: painted=1 facets=3"),
            "{}",
            header(&gcode)
        );
    }

    #[cfg(not(feature = "with-libslic3r"))]
    #[test]
    fn run_slice_refuses_fan_facet_values_above_147() {
        // base64 of [148][1 u32 LE].
        let gcode = stub_slice_gcode(
            "fan-rle-148",
            ObjectInputs {
                fan_facets_rle: Some("lAEAAAA=".to_string()),
                ..stub_object()
            },
        );
        assert!(
            gcode.contains("; fan_facets_decode_error: facet value exceeds 147"),
            "{}",
            header(&gcode)
        );
    }

    #[cfg(not(feature = "with-libslic3r"))]
    #[test]
    fn run_slice_still_refuses_support_facet_values_above_2() {
        // base64 of [3][1 u32 LE]: the fan limit must not leak into support paint.
        let gcode = stub_slice_gcode(
            "support-rle-3",
            ObjectInputs {
                support_facets_rle: "AwEAAAA=".to_string(),
                ..stub_object()
            },
        );
        assert!(
            gcode.contains("; support_facets_decode_error: facet value exceeds 2"),
            "{}",
            header(&gcode)
        );
    }

    #[cfg(not(feature = "with-libslic3r"))]
    #[test]
    fn run_slice_passes_the_fan_value_of_a_layer_range() {
        let gcode = stub_slice_gcode(
            "range-fan",
            ObjectInputs {
                layer_ranges: vec![LayerConfigRangeInputs {
                    id: "band".to_string(),
                    z_min: 5.0,
                    z_max: 10.0,
                    overrides: Vec::new(),
                    // Lines 7 (all), 0 %.
                    fan_value: 127,
                }],
                ..stub_object()
            },
        );
        assert!(
            gcode.contains("; layer_range_fan: band z=[5, 10] value=127"),
            "{}",
            header(&gcode)
        );
    }
}
