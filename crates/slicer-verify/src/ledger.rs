//! Which checks have been seen to fail, in this process.
//!
//! §3's requirement, and the reason this crate exists at all: *"ни одной
//! зелёной проверке нельзя верить, пока не видел, как она краснеет"*. A
//! check that has never produced a red result is not evidence of
//! anything — it may be measuring the wrong thing, or nothing.
//!
//! So a green [`Verdict`](crate::Verdict) carries `proven_falsifiable`,
//! and that flag is not a constructor argument. It is read from this
//! ledger, and the only way into the ledger is [`crate::selftest::run`]
//! actually running the check against a deliberately broken fixture and
//! actually getting `ok == false` back. There is no public way to set it,
//! which is the point — a flag a caller can pass in is a flag that will
//! eventually be passed in by someone in a hurry.
//!
//! Scope is the process, not the machine: a flag persisted to disk would
//! outlive the code it vouched for.

use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};

use crate::Check;

fn proven() -> &'static Mutex<BTreeSet<Check>> {
    static PROVEN: OnceLock<Mutex<BTreeSet<Check>>> = OnceLock::new();
    PROVEN.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Called only by the self-test runner, and only after it has watched the
/// check go red.
pub(crate) fn record(check: Check) {
    if let Ok(mut set) = proven().lock() {
        set.insert(check);
    }
}

/// Has this check been seen to fail in this session?
pub fn is_proven(check: Check) -> bool {
    proven()
        .lock()
        .map(|set| set.contains(&check))
        .unwrap_or(false)
}

/// Every check proven so far.
pub fn proven_checks() -> Vec<Check> {
    proven()
        .lock()
        .map(|set| set.iter().copied().collect())
        .unwrap_or_default()
}

/// Checks that have *not* been proven, for a caller that wants to refuse
/// to act on them.
pub fn unproven_checks() -> Vec<Check> {
    Check::ALL
        .iter()
        .copied()
        .filter(|c| !is_proven(*c))
        .collect()
}
