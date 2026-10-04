//! Real Restate processes and directed peer links; never a replacement substrate.
use super::{
    Step,
    case::{ArtifactIdentity, CaseLease},
    control::{BarrierProof, CleanupReceipt, FaultReceipt},
};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeReceipt {
    pub node: u32,
    pub ingress_url: String,
    pub admin_url: String,
    pub peer_address: String,
    pub data_directory: String,
    pub incarnation: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaderReceipt {
    pub partition: u32,
    pub node: u32,
    pub epoch: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterReceipt {
    pub binary: ArtifactIdentity,
    pub nodes: Vec<NodeReceipt>,
    pub leaders: Vec<LeaderReceipt>,
    pub provisioning: serde_json::Value,
}
pub trait ClusterControl {
    fn boot<'a>(
        &'a mut self,
        binary: &'a ArtifactIdentity,
        nodes: usize,
        lease: &'a mut CaseLease,
    ) -> Step<'a, ClusterReceipt>;
    fn leaders(&mut self) -> Step<'_, Vec<LeaderReceipt>>;
    fn kill<'a>(&'a mut self, node: u32, proof: &'a BarrierProof) -> Step<'a, FaultReceipt>;
    fn restart(&mut self, node: u32) -> Step<'_, NodeReceipt>;
    fn partition(&mut self, from: u32, to: u32) -> Step<'_, ()>;
    fn heal(&mut self, from: u32, to: u32) -> Step<'_, ()>;
    fn converge(&mut self) -> Step<'_, ClusterReceipt>;
    fn finish(&mut self) -> Step<'_, Vec<CleanupReceipt>>;
}

pub mod links;
mod runtime;
pub use runtime::LocalCluster;

mod metadata;
mod scanner;
