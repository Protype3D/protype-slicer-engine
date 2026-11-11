use serde::{Deserialize, Serialize};
use slicer_types::EventId;

pub const PROTOCOL_VERSION: &str = "1.0.0";

#[derive(Debug, Deserialize)]
pub struct HelloParams {
    pub token: String,
    pub client_id: String,
    pub protocol_version: String,
    #[serde(default)]
    pub resume_from_event: Option<EventId>,
}

#[derive(Debug, Serialize)]
pub struct HelloResult {
    pub session_id: String,
    pub daemon_version: String,
    pub protocol_version: String,
    pub current_event_id: EventId,
    pub capabilities: Vec<String>,
    /// Source this daemon was built from: its commit, the pinned libslic3r
    /// commit, and whether the build may differ from them.
    pub daemon_commit: String,
    pub libslic3r_commit: String,
    pub daemon_dirty: bool,
}

/// Every method `Dispatcher::handle` answers, alphabetical. This list is
/// the daemon's public RPC surface: the daemon links libslic3r and is
/// AGPL, so what it answers is open source, and nothing joins it by a new
/// arm alone.
///
/// `handle` refuses a method that is not here before it reaches its
/// `match`, so an arm missing from this list cannot be reached, and
/// `tests/method_routing.rs` fails for a name here that has no arm. The
/// daemon's `PUBLIC_SURFACE.toml` repeats the list and a test there fails
/// when the two differ: adding a method is a change to that file, and the
/// PR that makes it says why the method cannot live in the geometry worker
/// or the agent.
///
/// `hello` is the dispatcher's own arm, for direct callers and tests; the
/// socket handshake that clients meet lives in `server.rs` and answers
/// before any message reaches the dispatcher.
/// `object.duplicate` and `slice.get_layer_stream` are arms that only
/// answer "not yet implemented"; they are listed because they are routed.
pub const METHODS: &[&str] = &[
    "adaptive.list",
    "adaptive.remove",
    "adaptive.set",
    "events.subscribe",
    "events.unsubscribe",
    "gcode.annotate_layers",
    "gcode.plan_resume",
    "gcode.select_tool",
    "geometry.build",
    "geometry.engrave_serial",
    "hello",
    "machine.check_start_block",
    "machine.list",
    "machine.passport",
    "mesh.centre_bore",
    "mesh.free_circle",
    "mesh.inscribed",
    "mesh.measure",
    "mesh.profile",
    "mesh.section",
    "mesh.wall_thickness",
    "object.add",
    "object.cut_with_plane",
    "object.duplicate",
    "object.get_edit_mesh",
    "object.get_materialized_mesh",
    "object.import_local",
    "object.query_faces",
    "object.remove",
    "object.serial_candidates",
    "object.set_fan_facets",
    "object.set_filament_facets",
    "object.set_layer_height_profile",
    "object.set_layer_ranges",
    "object.set_operations",
    "object.set_override",
    "object.set_seam_facets",
    "object.set_serial_marking",
    "object.set_support_facets",
    "object.split_to_parts",
    "object.transform",
    "objects.list",
    "part.create",
    "part.create_from_object",
    "part.get",
    "part.remove",
    "part.set_document",
    "parts.list",
    "preset.activate",
    "preset.delete",
    "preset.list",
    "preset.save",
    "project.apply_external_edit",
    "project.discard_autosave",
    "project.discard_recovery",
    "project.export_gcode",
    "project.new",
    "project.open",
    "project.recover",
    "project.recover_list",
    "project.redo",
    "project.save",
    "project.undo",
    "setting.get",
    "setting.list",
    "setting.set",
    "settings.schema",
    "slice.cancel",
    "slice.estimate",
    "slice.get_layer_stream",
    "slice.request",
    "state.events_since",
    "state.snapshot",
    "verify.bore",
    "verify.fit",
    "verify.interference",
    "verify.overhangs",
    "verify.selftest",
    "verify.status",
];

/// The method a client sends to end its session. The socket loop in
/// `server.rs` compares against this name, and `SERVER_METHODS` is built
/// from it, so the name the daemon speaks and the name the public surface
/// lists are one string: renaming it moves both, and the surface test then
/// asks for the matching edit of `PUBLIC_SURFACE.toml`.
pub const GOODBYE: &str = "goodbye";

/// The notification that carries each project event to a client. Sent by
/// `server.rs` under this name; `NOTIFICATIONS` is built from it.
pub const EVENT_NOTIFICATION: &str = "events.event";

/// Methods the socket loop in `server.rs` answers itself, before the
/// dispatcher: the client says `goodbye` and the daemon closes the session.
/// An entry here is a promise `tests/method_routing.rs` holds the socket to.
pub const SERVER_METHODS: &[&str] = &[GOODBYE];

/// Messages the daemon sends to a client on its own, with no request: each
/// project event, as `events.event`.
pub const NOTIFICATIONS: &[&str] = &[EVENT_NOTIFICATION];

/// Capabilities advertised in `hello.capabilities` — the single source of
/// truth for both entry points that report them: the real client-facing
/// WS handshake (`server.rs`'s `handle_socket`, pre-auth, returns the
/// full `HelloResult`) and the `"hello"` arm of `Dispatcher::handle`
/// (direct calls, e.g. tests, with no auth/session semantics). Keeping one
/// function means the two can't drift apart. In advertised order, which is
/// not alphabetical; `PUBLIC_SURFACE.toml` lists the same set sorted.
pub fn capabilities() -> Vec<String> {
    vec![
        "events.subscribe".to_string(),
        "project.new".to_string(),
        "preset.activate".to_string(),
        "mesh_inspector".to_string(),
        "object.cut_with_plane".to_string(),
        "object.set_operations.hole_heads".to_string(),
        "geometry.build".to_string(),
        "geometry.surface_relief_v1".to_string(),
        "geometry.csg_import_v1".to_string(),
        "geometry.extrude_twist".to_string(),
        "geometry.engrave_serial".to_string(),
        "serial_marking_v1".to_string(),
        "serial_marking_short_v1".to_string(),
        "serial_marking_column_v1".to_string(),
        "machine.passport".to_string(),
        "mesh.measure".to_string(),
        "gcode.annotate_layers".to_string(),
        "verify.selftest".to_string(),
        "slice.estimate".to_string(),
        "filament_paint_v1".to_string(),
        "part_document".to_string(),
        "slice.calibration_test".to_string(),
        "slice.calibration_test.retraction_steps".to_string(),
    ]
}
