//! Tells `build_info` which source this binary is built from.
//!
//! The revision comes from the `REVISION` file at the workspace root when
//! there is one (release builds, and the published mirror, which has no
//! `.git`), and from git otherwise. The rules live in
//! `src/build_info/revision.rs`, compiled here as well as into the library.
//! A git failure never fails the build: the revision is `unknown` then.

#[path = "src/build_info/revision.rs"]
mod revision;

use revision::{parse_revision_file, pick_revision, GitFacts};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

const REVISION_FILE: &str = "REVISION";
const LIBSLIC3R_PATH: &str = "upstream/libslic3r";

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR for build scripts"),
    );
    let root = manifest_dir
        .ancestors()
        .nth(2)
        .expect("slicer-rpc lives at <workspace>/crates/slicer-rpc");

    let revision_file = read_revision_file(&root.join(REVISION_FILE));
    watch_git_state(root);
    let revision = pick_revision(revision_file.as_deref(), git_facts(root));

    let cargo_version = env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    println!("cargo:rustc-env=PROTYPE_DAEMON_COMMIT={}", revision.daemon);
    println!(
        "cargo:rustc-env=PROTYPE_LIBSLIC3R_COMMIT={}",
        revision.libslic3r
    );
    println!("cargo:rustc-env=PROTYPE_DAEMON_DIRTY={}", revision.dirty);
    println!(
        "cargo:rustc-env=PROTYPE_VERSION_LINE={}",
        revision.version_line(&cargo_version)
    );
}

/// The file's text, if there is a file. One that cannot be used is reported
/// but does not stop the build: git is asked instead. A `REVISION` written
/// after the build script last ran is not noticed until something else makes
/// it run again, so write it before building.
fn read_revision_file(path: &Path) -> Option<String> {
    watch_if_exists(path);
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return None,
        Err(err) => {
            println!("cargo:warning={REVISION_FILE} is unreadable and ignored: {err}");
            return None;
        }
    };
    if let Err(reason) = parse_revision_file(&text) {
        println!("cargo:warning={REVISION_FILE} is invalid and ignored: {reason}");
    }
    Some(text)
}

/// Stdout of `git <args>` run in `dir`, trimmed; `None` if git is missing or
/// the command fails.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        // `status` must not rewrite the index this script watches.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    Some(stdout.trim().to_string())
}

fn git_facts(root: &Path) -> Option<GitFacts> {
    // A source tree unpacked inside somebody else's checkout must not report
    // that checkout's commit: only a repository rooted at the workspace counts.
    if !git(root, &["rev-parse", "--show-prefix"])?.is_empty() {
        return None;
    }
    let head = git(root, &["rev-parse", "HEAD"])?;
    let gitlink = git(root, &["rev-parse", &format!("HEAD:{LIBSLIC3R_PATH}")])?;
    let porcelain = git(root, &["status", "--porcelain", "--ignore-submodules=none"])?;
    Some(GitFacts {
        head,
        gitlink,
        submodule_head: submodule_head(root),
        porcelain_empty: porcelain.is_empty(),
    })
}

/// The submodule directory, if it is checked out. An empty directory is not:
/// git run inside it would answer for the parent repository.
fn checked_out_submodule(root: &Path) -> Option<PathBuf> {
    let dir = root.join(LIBSLIC3R_PATH);
    dir.join(".git").exists().then_some(dir)
}

fn submodule_head(root: &Path) -> Option<String> {
    git(&checked_out_submodule(root)?, &["rev-parse", "HEAD"])
}

fn submodule_git_dir(root: &Path) -> Option<PathBuf> {
    let dir = checked_out_submodule(root)?;
    git(&dir, &["rev-parse", "--absolute-git-dir"]).map(PathBuf::from)
}

/// Re-run when a commit, a checkout or a staged change moves what git says.
/// In a linked worktree the git dir is not `<root>/.git`, so ask git for it.
/// Edits of tracked files alone do not re-run the script, and `dirty` reflects
/// the tree as it was when the script last ran.
fn watch_git_state(root: &Path) {
    if let Some(git_dir) = git(root, &["rev-parse", "--absolute-git-dir"]) {
        let git_dir = PathBuf::from(git_dir);
        watch_if_exists(&git_dir.join("HEAD"));
        watch_if_exists(&git_dir.join("index"));
    }
    // Commits move the branch file, not `HEAD`; it lives in the common dir.
    if let (Some(common), Some(branch)) = (
        git(root, &["rev-parse", "--git-common-dir"]),
        git(root, &["symbolic-ref", "--quiet", "HEAD"]),
    ) {
        let common = root.join(common);
        watch_if_exists(&common.join(branch));
        watch_if_exists(&common.join("packed-refs"));
    }
    if let Some(git_dir) = submodule_git_dir(root) {
        watch_if_exists(&git_dir.join("HEAD"));
    }
}

/// A watched path that is missing counts as changed on every build, so only
/// existing ones are named.
fn watch_if_exists(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
