//! The source revision this binary was built from; `build.rs` fills it in.

pub mod revision;

pub const DAEMON_COMMIT: &str = env!("PROTYPE_DAEMON_COMMIT");
pub const LIBSLIC3R_COMMIT: &str = env!("PROTYPE_LIBSLIC3R_COMMIT");
/// The build may differ from the commits above (see `BuildRevision::dirty`).
pub const DIRTY: bool = matches!(env!("PROTYPE_DAEMON_DIRTY").as_bytes(), b"true");
/// `<cargo version> (daemon <sha>, libslic3r <sha>[, dirty])`.
pub const VERSION_LINE: &str = env!("PROTYPE_VERSION_LINE");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_is_built_from_the_constants() {
        let revision = revision::BuildRevision {
            daemon: DAEMON_COMMIT.to_string(),
            libslic3r: LIBSLIC3R_COMMIT.to_string(),
            dirty: DIRTY,
        };
        assert_eq!(
            VERSION_LINE,
            revision.version_line(env!("CARGO_PKG_VERSION"))
        );
    }
}
