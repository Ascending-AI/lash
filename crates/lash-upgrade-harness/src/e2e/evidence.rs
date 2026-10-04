//! Reconciled receipts, with explicit unavailable evidence.
use super::{
    case::ArtifactIdentity,
    control::{BarrierProof, CleanupReceipt, FaultReceipt, WorkIdentity},
    host::HostObservation,
};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DecodedRecord {
    Run(lash_core_store::tool_run::RunJournalEntry),
    Transfer(lash_core_store::tool_run::RunTransfer),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournalFact {
    pub work: WorkIdentity,
    pub invocation: String,
    pub index: u64,
    pub entry_type: String,
    pub name: Option<String>,
    pub value: serde_json::Value,
    pub decoded: Option<DecodedRecord>,
    pub admin_url: String,
    pub protocol: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evidence {
    pub case: String,
    pub artifacts: Vec<ArtifactIdentity>,
    pub journals: Vec<JournalFact>,
    pub barriers: Vec<BarrierProof>,
    pub faults: Vec<FaultReceipt>,
    pub stores: Vec<serde_json::Value>,
    pub effects: Vec<serde_json::Value>,
    pub outputs: Vec<HostObservation>,
    pub cleanup: Vec<CleanupReceipt>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Passed,
    Failed { reason: String },
    NotRun { reason: String },
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Counts {
    pub selected: usize,
    pub executed: usize,
    pub passed: usize,
    pub failed: usize,
    pub not_run: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaseReceipt {
    pub evidence: Evidence,
    pub verdict: Verdict,
}
