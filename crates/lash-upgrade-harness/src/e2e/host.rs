//! Prebuilt production hosts implement these controls through their public transport.
use super::{
    Step,
    case::{ArtifactIdentity, CaseLease},
    control::WorkIdentity,
};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HostKind {
    UpgradeNode,
    Workbench,
    ExternalConsumer,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostReady {
    pub endpoint: String,
    pub process: super::control::ProcessReceipt,
    pub protocol: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HostCommand {
    Submit {
        session: String,
        idempotency_key: String,
        input: serde_json::Value,
    },
    Attach {
        run: String,
    },
    Cancel {
        run: String,
    },
    Operation {
        session: String,
        input: serde_json::Value,
    },
    Process {
        action: String,
        input: serde_json::Value,
    },
    Park {
        run: String,
    },
    Transfer {
        run: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostObservation {
    pub work: WorkIdentity,
    pub output: serde_json::Value,
}
pub trait HostAdapter {
    fn boot<'a>(
        &'a mut self,
        artifact: &'a ArtifactIdentity,
        lease: &'a mut CaseLease,
    ) -> Step<'a, HostReady>;
    fn command<'a>(&'a mut self, command: HostCommand) -> Step<'a, HostObservation>;
    fn transcript(&self) -> anyhow::Result<Vec<HostObservation>>;
    /// Graceful stop must flush acknowledged telemetry before returning.
    fn stop(&mut self) -> Step<'_, Vec<super::control::CleanupReceipt>>;
}
