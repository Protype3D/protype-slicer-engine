//! Which source this binary was built from.
//!
//! `std` only: `build.rs` compiles this very file through `#[path]`, so the
//! rules that pick the revision are the ones the unit tests below exercise.
//!
//! So that a user can ask for exactly the source a running daemon was built
//! from, it reports two commits: the daemon tree and the pinned
//! `upstream/libslic3r`.

/// What a field holds when nothing could tell it.
pub const UNKNOWN: &str = "unknown";

const SHA_LEN: usize = 40;

/// Revisions a binary was built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildRevision {
    pub daemon: String,
    pub libslic3r: String,
    /// The build may differ from `daemon` + `libslic3r`: edited tree,
    /// submodule off its pin, or a revision that could not be read at all.
    pub dirty: bool,
}

impl BuildRevision {
    /// `<cargo version> (daemon <sha>, libslic3r <sha>[, dirty])` — the
    /// `--version` line. Release tooling parses it, keep the shape.
    pub fn version_line(&self, cargo_version: &str) -> String {
        let dirty = if self.dirty { ", dirty" } else { "" };
        format!(
            "{cargo_version} (daemon {}, libslic3r {}{dirty})",
            self.daemon, self.libslic3r
        )
    }
}

fn is_full_sha(value: &str) -> bool {
    value.len() == SHA_LEN
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn checked_sha(key: &str, value: &str) -> Result<String, String> {
    if is_full_sha(value) {
        Ok(value.to_string())
    } else {
        Err(format!(
            "{key}: expected {SHA_LEN} lowercase hex digits, got {value:?}"
        ))
    }
}

/// Reads the `REVISION` file the release script writes into the source tree:
/// `daemon=<40 hex>` and `libslic3r=<40 hex>`, one per line. A tree without a
/// `.git` (the published mirror) has nothing else to learn its revision from.
pub fn parse_revision_file(text: &str) -> Result<BuildRevision, String> {
    let mut daemon = None;
    let mut libslic3r = None;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line without `=`: {line:?}"))?;
        let slot = match key {
            "daemon" => &mut daemon,
            "libslic3r" => &mut libslic3r,
            other => return Err(format!("unknown key {other:?}")),
        };
        if slot.is_some() {
            return Err(format!("{key} is given twice"));
        }
        *slot = Some(checked_sha(key, value)?);
    }
    Ok(BuildRevision {
        daemon: daemon.ok_or("daemon is missing")?,
        libslic3r: libslic3r.ok_or("libslic3r is missing")?,
        dirty: false,
    })
}

/// What `git` said about the tree being built.
#[derive(Debug, Clone)]
pub struct GitFacts {
    /// `git rev-parse HEAD`.
    pub head: String,
    /// `git rev-parse HEAD:upstream/libslic3r` — the pin in the commit, not
    /// whatever the submodule has checked out.
    pub gitlink: String,
    /// `HEAD` of the checked-out submodule; `None` when it is not checked out.
    pub submodule_head: Option<String>,
    /// `git status --porcelain --ignore-submodules=none` printed nothing.
    pub porcelain_empty: bool,
}

/// `REVISION` wins; without a valid one the git facts decide; without those
/// the answer is `unknown` and dirty, because nothing vouches for the build.
pub fn pick_revision(revision_file: Option<&str>, git: Option<GitFacts>) -> BuildRevision {
    if let Some(from_file) = revision_file.and_then(|text| parse_revision_file(text).ok()) {
        return from_file;
    }
    match git {
        Some(facts) => {
            // A submodule that is not checked out is absent, not modified.
            let off_pin = facts
                .submodule_head
                .as_deref()
                .is_some_and(|head| head != facts.gitlink);
            BuildRevision {
                daemon: facts.head,
                libslic3r: facts.gitlink,
                dirty: !facts.porcelain_empty || off_pin,
            }
        }
        None => BuildRevision {
            daemon: UNKNOWN.to_string(),
            libslic3r: UNKNOWN.to_string(),
            dirty: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn revision_file_with_two_full_shas_is_clean() {
        let r = parse_revision_file(&format!("daemon={A}\nlibslic3r={B}\n")).unwrap();
        assert_eq!(
            (r.daemon.as_str(), r.libslic3r.as_str(), r.dirty),
            (A, B, false)
        );
    }
    #[test]
    fn revision_file_rejects_short_sha_and_missing_key() {
        assert!(parse_revision_file(&format!("daemon=abc\nlibslic3r={B}\n")).is_err());
        assert!(parse_revision_file(&format!("daemon={A}\n")).is_err());
    }
    #[test]
    fn revision_file_wins_over_git() {
        let git = GitFacts {
            head: B.into(),
            gitlink: B.into(),
            submodule_head: Some(B.into()),
            porcelain_empty: true,
        };
        assert_eq!(
            pick_revision(Some(&format!("daemon={A}\nlibslic3r={A}\n")), Some(git)).daemon,
            A
        );
    }
    #[test]
    fn git_dirty_when_tree_changed_or_submodule_off_pin() {
        let base = GitFacts {
            head: A.into(),
            gitlink: B.into(),
            submodule_head: Some(B.into()),
            porcelain_empty: true,
        };
        assert!(!pick_revision(None, Some(base.clone())).dirty);
        assert!(
            pick_revision(
                None,
                Some(GitFacts {
                    porcelain_empty: false,
                    ..base.clone()
                })
            )
            .dirty
        );
        assert!(
            pick_revision(
                None,
                Some(GitFacts {
                    submodule_head: Some(A.into()),
                    ..base
                })
            )
            .dirty
        );
    }
    #[test]
    fn nothing_known_gives_unknown_and_dirty() {
        let r = pick_revision(None, None);
        assert_eq!(
            (r.daemon.as_str(), r.libslic3r.as_str(), r.dirty),
            ("unknown", "unknown", true)
        );
    }
    #[test]
    fn revision_file_rejects_uppercase_unknown_and_repeated_keys() {
        let upper = "A".repeat(40);
        assert!(parse_revision_file(&format!("daemon={upper}\nlibslic3r={B}\n")).is_err());
        assert!(parse_revision_file(&format!("daemon={A}\nlibslic3r={B}\nextra={A}\n")).is_err());
        assert!(parse_revision_file(&format!("daemon={A}\ndaemon={A}\nlibslic3r={B}\n")).is_err());
    }
    #[test]
    fn invalid_revision_file_falls_back_to_git() {
        let git = GitFacts {
            head: A.into(),
            gitlink: B.into(),
            submodule_head: Some(B.into()),
            porcelain_empty: true,
        };
        let r = pick_revision(Some("daemon=abc\n"), Some(git));
        assert_eq!(
            (r.daemon.as_str(), r.libslic3r.as_str(), r.dirty),
            (A, B, false)
        );
    }
    #[test]
    fn submodule_not_checked_out_is_not_dirty() {
        let git = GitFacts {
            head: A.into(),
            gitlink: B.into(),
            submodule_head: None,
            porcelain_empty: true,
        };
        let r = pick_revision(None, Some(git));
        assert_eq!(
            (r.daemon.as_str(), r.libslic3r.as_str(), r.dirty),
            (A, B, false)
        );
    }
    #[test]
    fn version_line_names_both_commits_and_marks_dirty() {
        let clean = BuildRevision {
            daemon: A.into(),
            libslic3r: B.into(),
            dirty: false,
        };
        assert_eq!(
            clean.version_line("0.1.0"),
            format!("0.1.0 (daemon {A}, libslic3r {B})")
        );
        let dirty = BuildRevision {
            dirty: true,
            ..clean
        };
        assert_eq!(
            dirty.version_line("0.1.0"),
            format!("0.1.0 (daemon {A}, libslic3r {B}, dirty)")
        );
    }
}
