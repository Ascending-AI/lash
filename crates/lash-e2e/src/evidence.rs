//! What a case reports: the receipt `scripts/e2e-gate.py` reads from
//! `<case dir>/receipt.json` and splits into the runner's role files.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

/// One resource the case owned, and whether it closed.
#[derive(Clone, Debug, Serialize)]
pub struct CleanupReceipt {
    pub resource: String,
    pub closed: bool,
    pub detail: String,
}

/// One lash node the case booted.
#[derive(Clone, Debug, Serialize)]
pub struct NodeEvidence {
    /// The node's name: its lease owner id.
    pub node: String,
    /// Every boot of the name, by process id.
    pub boots: Vec<u32>,
    /// Whether the case killed a boot of it.
    pub killed: bool,
}

/// Body entries per admitted identity, counted from the hosts' body
/// ledgers: what hidden replay would make a host run again (ADR 0132 §2).
#[derive(Clone, Debug, Default, Serialize)]
pub struct Tripwire {
    /// Body entries per `tool/call/attempt`.
    pub bodies: BTreeMap<String, usize>,
    /// Model calls the hosts' providers answered, in total.
    pub model_calls: usize,
}

impl Tripwire {
    /// The identities entered more than once.
    #[must_use]
    pub fn repeated(&self) -> Vec<&str> {
        self.bodies
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(identity, _)| identity.as_str())
            .collect()
    }
}

/// Everything a case observed.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Evidence {
    pub scenario: String,
    /// Every labelled commit the hosts' ledgers recorded, in order per node.
    pub commits: Vec<Value>,
    pub tripwire: Tripwire,
    pub nodes: Vec<NodeEvidence>,
    /// The store and the facts the case read back from it.
    pub stores: Vec<Value>,
    /// The terminal outcomes the hosts answered.
    pub outputs: Vec<Value>,
    /// The host binaries, with their digests.
    pub artifacts: Vec<Value>,
    pub barriers: Vec<Value>,
    pub faults: Vec<Value>,
    pub effects: Vec<Value>,
    pub cleanup: Vec<CleanupReceipt>,
}

/// The receipt: the evidence and the case's verdict.
#[derive(Debug, Serialize)]
pub struct CaseReceipt {
    pub case: CaseBody,
    pub verdict: Verdict,
}

#[derive(Debug, Serialize)]
pub struct CaseBody {
    pub evidence: Evidence,
}

#[derive(Debug, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    Passed,
    Failed { reason: String },
}
