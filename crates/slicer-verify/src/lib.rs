//! Checks that have to prove they can fail before anyone may cite them.
//!
//! §3 of the print-pipeline spec, whose rule is the one everything else
//! here hangs off: *no green check is worth anything until it has been
//! seen to go red.* The bore check that mattered was written after a
//! render showed a crossbar inside a hole, and its first run was on the
//! **broken** model — it reported 26 mm against a required 53.7. Only
//! after that did a green result on the fixed model mean anything.
//!
//! So every [`Verdict`] carries [`Verdict::proven_falsifiable`], and that
//! flag cannot be set by a caller. It is read from [`ledger`], and the
//! only way into the ledger is [`selftest::run`] actually running the
//! check against a deliberately broken fixture and actually getting a red
//! result back.
//!
//! ```
//! use slicer_verify::{selftest, Check, ledger};
//!
//! // Nothing is citable before its self-test has run.
//! assert!(!ledger::is_proven(Check::Overhangs));
//!
//! let report = selftest::run(Check::Overhangs);
//! assert!(report.passed, "the check must report the defect: {}", report.observed);
//! assert!(ledger::is_proven(Check::Overhangs));
//! ```

pub mod checks;
pub mod ledger;
pub mod selftest;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    Bore,
    Fit,
    Interference,
    Overhangs,
}

impl Check {
    pub const ALL: [Check; 4] = [
        Check::Bore,
        Check::Fit,
        Check::Interference,
        Check::Overhangs,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            Check::Bore => "bore",
            Check::Fit => "fit",
            Check::Interference => "interference",
            Check::Overhangs => "overhangs",
        }
    }
}

impl std::fmt::Display for Check {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub check: Check,
    pub ok: bool,
    /// Whether this check has been watched failing in this session.
    ///
    /// Not a constructor argument. A green verdict with this `false` is
    /// not evidence — it is a check nobody has established measures
    /// anything at all.
    pub proven_falsifiable: bool,
    pub summary: String,
    /// The numbers behind the verdict.
    pub detail: serde_json::Value,
}

impl Verdict {
    pub(crate) fn new<T: Serialize>(check: Check, ok: bool, summary: String, detail: &T) -> Self {
        Self {
            check,
            ok,
            proven_falsifiable: ledger::is_proven(check),
            summary,
            detail: serde_json::to_value(detail).unwrap_or(serde_json::Value::Null),
        }
    }

    /// The only form worth acting on: it passed, **and** the check has
    /// been shown capable of failing.
    ///
    /// A caller should branch on this rather than on `ok`. The two differ
    /// exactly when someone is about to trust a check that has never been
    /// tested.
    pub fn is_credible_pass(&self) -> bool {
        self.ok && self.proven_falsifiable
    }

    /// Why this verdict may not be cited, if it may not be.
    pub fn caveat(&self) -> Option<String> {
        if self.proven_falsifiable {
            return None;
        }
        Some(format!(
            "the `{}` check has not been seen to fail in this session, so a pass from it is not \
             evidence — run its self-test first",
            self.check
        ))
    }
}
