//! The daemon links libslic3r, so it is AGPL: whatever it exposes ships as
//! open source. `PUBLIC_SURFACE.toml` is the reviewed list of that
//! exposure — its crates, RPC methods, capabilities and CLI subcommands —
//! and these tests fail when the code and the list part ways in either
//! direction. A new method, crate, capability or subcommand therefore
//! cannot land without an edit to the file, which CODEOWNERS and the
//! `public-surface` label put in front of a reviewer.

use super::Cli;
use clap::CommandFactory;
use slicer_rpc::methods::{capabilities, METHODS, NOTIFICATIONS, SERVER_METHODS};
use std::collections::BTreeSet;
use std::path::PathBuf;
use toml::{Table, Value};

/// The repository root, two levels above this crate's manifest.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_toml(path: PathBuf) -> Table {
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.parse::<Table>()
        .unwrap_or_else(|e| panic!("{} is not valid TOML: {e}", path.display()))
}

fn surface() -> Table {
    read_toml(repo_root().join("PUBLIC_SURFACE.toml"))
}

fn section<'a>(surface: &'a Table, name: &str) -> &'a Table {
    surface
        .get(name)
        .and_then(Value::as_table)
        .unwrap_or_else(|| panic!("PUBLIC_SURFACE.toml has no [{name}] table"))
}

fn string_list(surface: &Table, table: &str, key: &str) -> Vec<String> {
    section(surface, table)
        .get(key)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("PUBLIC_SURFACE.toml has no `{table}.{key}` array"))
        .iter()
        .map(|item| {
            item.as_str()
                .unwrap_or_else(|| panic!("`{table}.{key}` holds a non-string: {item}"))
                .to_string()
        })
        .collect()
}

fn sorted(items: impl IntoIterator<Item = impl Into<String>>) -> Vec<String> {
    let mut items: Vec<String> = items.into_iter().map(Into::into).collect();
    items.sort();
    items
}

/// The disagreement between the code and the file, or `Ok` when they match.
/// It names the entries on each side, so the reviewer of a red CI run sees
/// what to add to the file or what to take out of the code instead of two
/// long lists to diff by eye. Pure, so a test can feed it a made-up set.
fn check_matches_surface(
    what: &str,
    in_code: Vec<String>,
    listed: Vec<String>,
) -> Result<(), String> {
    let in_code = sorted(in_code);
    let listed_sorted = sorted(listed.clone());
    if listed != listed_sorted {
        return Err(format!(
            "`{what}` in PUBLIC_SURFACE.toml must be sorted alphabetically"
        ));
    }
    if in_code == listed {
        return Ok(());
    }
    let code_set: BTreeSet<_> = in_code.iter().collect();
    let list_set: BTreeSet<_> = listed.iter().collect();
    let new_in_code: Vec<_> = code_set.difference(&list_set).collect();
    let missing_in_code: Vec<_> = list_set.difference(&code_set).collect();
    let detail = if new_in_code.is_empty() && missing_in_code.is_empty() {
        format!(
            "the same names, counted differently (a duplicate): code {in_code:?}, file {listed:?}"
        )
    } else {
        format!("in code, not listed: {new_in_code:?}\n  listed, not in code: {missing_in_code:?}")
    };
    Err(format!(
        "the {what} of the daemon and PUBLIC_SURFACE.toml disagree.\n  {detail}\n\
         Adding to the daemon's public surface needs a reason it cannot live in \
         the geometry worker or the agent; say it in the PR."
    ))
}

fn assert_matches_surface(what: &str, in_code: Vec<String>, listed: Vec<String>) {
    if let Err(disagreement) = check_matches_surface(what, in_code, listed) {
        panic!("{disagreement}");
    }
}

/// The packages of `Cargo.lock` that are not downloaded: every `[[package]]`
/// without a `source` is a crate of this repository, whatever its place in
/// `[workspace].members`. Cargo makes a path dependency inside the workspace
/// directory a member on its own, so a crate added as
/// `slicer-secret = { path = "../slicer-secret" }` joins the daemon without
/// a line in `members`; `Cargo.lock` lists it, and it is also the one file
/// that sees a path dependency outside the workspace directory. It is in the
/// public mirror, so these tests run there too.
fn local_package_names(lock_text: &str) -> Vec<String> {
    let lock: Table = lock_text
        .parse()
        .unwrap_or_else(|e| panic!("Cargo.lock is not valid TOML: {e}"));
    lock.get("package")
        .and_then(Value::as_array)
        .expect("Cargo.lock has a [[package]] list")
        .iter()
        .filter_map(Value::as_table)
        .filter(|package| !package.contains_key("source"))
        .map(|package| {
            package
                .get("name")
                .and_then(Value::as_str)
                .expect("a [[package]] in Cargo.lock has a name")
                .to_string()
        })
        .collect()
}

#[test]
fn local_crates_match_public_surface() {
    let lock_path = repo_root().join("Cargo.lock");
    let lock_text = std::fs::read_to_string(&lock_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", lock_path.display()));

    let surface = surface();
    let listed: Vec<String> = section(&surface, "crates").keys().cloned().collect();
    assert_matches_surface("local crates", local_package_names(&lock_text), listed);

    for (name, why) in section(&surface, "crates") {
        let why = why
            .as_str()
            .unwrap_or_else(|| panic!("[crates].{name} must be a string saying why"));
        assert!(
            !why.trim().is_empty(),
            "[crates].{name} must say why the daemon needs this crate"
        );
    }
}

/// The hole the check on `[workspace].members` had: a crate that is only a
/// path dependency. The lock below is the real one plus such a crate, with
/// the `members` of the root `Cargo.toml` untouched, and it must be refused.
#[test]
fn a_path_dependency_outside_workspace_members_is_refused() {
    let real_lock = std::fs::read_to_string(repo_root().join("Cargo.lock")).unwrap();
    let with_extra =
        format!("{real_lock}\n[[package]]\nname = \"slicer-secret\"\nversion = \"0.1.0\"\n");
    let listed: Vec<String> = section(&surface(), "crates").keys().cloned().collect();

    let refused = check_matches_surface("local crates", local_package_names(&with_extra), listed)
        .expect_err("a local crate that PUBLIC_SURFACE.toml does not list must be refused");

    assert!(
        refused.contains("in code, not listed: [\"slicer-secret\"]"),
        "{refused}"
    );
}

#[test]
fn only_packages_without_a_source_are_local() {
    let lock = r#"
version = 4

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "00"

[[package]]
name = "slicer-rpc"
version = "0.1.0"
dependencies = ["serde"]

[[package]]
name = "from-git"
version = "0.2.0"
source = "git+https://example.com/x.git#abc"

[[package]]
name = "slicer-state"
version = "0.1.0"
"#;

    assert_eq!(
        local_package_names(lock),
        vec!["slicer-rpc".to_string(), "slicer-state".to_string()]
    );
}

#[test]
fn rpc_methods_match_public_surface() {
    let in_code: Vec<String> = METHODS.iter().map(|m| m.to_string()).collect();
    assert_eq!(
        in_code,
        sorted(METHODS.iter().copied()),
        "METHODS must be sorted alphabetically and hold no duplicate"
    );
    assert_matches_surface(
        "RPC methods",
        in_code,
        string_list(&surface(), "rpc", "methods"),
    );
}

#[test]
fn server_methods_and_notifications_match_public_surface() {
    let surface = surface();
    assert_matches_surface(
        "server methods",
        SERVER_METHODS.iter().map(|m| m.to_string()).collect(),
        string_list(&surface, "rpc", "server_methods"),
    );
    assert_matches_surface(
        "notifications",
        NOTIFICATIONS.iter().map(|m| m.to_string()).collect(),
        string_list(&surface, "rpc", "notifications"),
    );
}

#[test]
fn capabilities_match_public_surface() {
    assert_matches_surface(
        "capabilities",
        capabilities(),
        string_list(&surface(), "rpc", "capabilities"),
    );
}

#[test]
fn cli_subcommands_match_public_surface() {
    let in_code: Vec<String> = Cli::command()
        .get_subcommands()
        .map(|sub| sub.get_name().to_string())
        .collect();
    assert_matches_surface(
        "CLI subcommands",
        in_code,
        string_list(&surface(), "cli", "subcommands"),
    );
}
