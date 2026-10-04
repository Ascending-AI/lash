//! Strict recorded providers preserve request occurrence and transport evidence.
use super::{Step, case::CaseLease, control::WorkIdentity};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProviderKind {
    Scripted,
    RecordedHttp,
    Live,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderRequest {
    pub occurrence: String,
    pub work: WorkIdentity,
    pub request: serde_json::Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderResponse {
    pub status: u16,
    pub headers: std::collections::BTreeMap<String, String>,
    pub frames: Vec<Vec<u8>>,
    pub usage: serde_json::Value,
    pub typed_failure: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderReceipt {
    pub request: ProviderRequest,
    pub response: ProviderResponse,
}
pub trait ProviderFixture {
    fn boot<'a>(&'a mut self, lease: &'a mut CaseLease) -> Step<'a, String>;
    fn receipts(&self) -> anyhow::Result<Vec<ProviderReceipt>>;
    /// Refuse missing, extra and out-of-order requests, and propagate task panics.
    fn finish(&mut self) -> Step<'_, Vec<super::control::CleanupReceipt>>;
}
