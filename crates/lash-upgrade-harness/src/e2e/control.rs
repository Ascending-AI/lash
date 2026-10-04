//! Out-of-journal controls. A proposed record is never a durable barrier.
use super::Step;
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkIdentity {
    pub ingress: String,
    pub run: String,
    pub segment: String,
    pub call: Option<String>,
    pub ordinal: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BarrierKind {
    BodyEntered,
    SideEffectAccepted,
    XProposed,
    XDurable,
    DProposed,
    DDurable,
    DeclarationIssued,
    VProposed,
    VDurable,
    ContinuationPublished,
    SuccessorAdmitted,
    PredecessorDischarged,
    SourceSealed,
    Suspended,
    HostReady,
    TransportConnected,
    BeforeAck,
    WorkerReaped,
    ParkCommitted,
    TelemetryFlushed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Barrier {
    pub work: WorkIdentity,
    pub kind: BarrierKind,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BarrierProof {
    pub barrier: Barrier,
    /// Real admin/journal artifact or independently durable outside-effect receipt.
    pub artifact: String,
    pub journal_index: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Fault {
    KillHost { target: String },
    KillVm { target: String },
    RestartRestate { node: u32 },
    DropConnection { target: String },
    PartitionLink { from: u32, to: u32 },
    HealLink { from: u32, to: u32 },
    Redeploy { target: String, artifact: String },
    DrainAndRetire { generation: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FaultReceipt {
    pub fault: Fault,
    pub proof: BarrierProof,
    pub target_incarnation: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessReceipt {
    pub role: String,
    pub pid: u32,
    pub incarnation: u32,
    pub log: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupReceipt {
    pub resource: String,
    pub closed: bool,
    pub detail: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ToolControl {
    Hold(Barrier),
    Release(Barrier),
    Resolve {
        work: WorkIdentity,
        value: serde_json::Value,
    },
}
pub trait Control {
    fn await_barrier<'a>(&'a mut self, barrier: &'a Barrier) -> Step<'a, BarrierProof>;
    fn inject<'a>(&'a mut self, fault: Fault, proof: &'a BarrierProof) -> Step<'a, FaultReceipt>;
    fn tool<'a>(&'a mut self, command: ToolControl) -> Step<'a, ()>;
}
