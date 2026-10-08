//! Durable await-event wait identity.

use crate::{ExecutionScope, RuntimeError};
use lash_sansio::ToolCallId;
use serde::{Deserialize, Serialize};
/// The wait a Deferred source resolves: the deferring tool call it parked
/// on, by its stable call id (ADR 0117).
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AwaitEventWaitIdentity {
    ToolCall { call_id: ToolCallId },
}
impl AwaitEventWaitIdentity {
    pub fn tool_call(call_id: ToolCallId) -> Self {
        Self::ToolCall { call_id }
    }

    pub fn validate(&self) -> Result<(), RuntimeError> {
        let invalid = match self {
            Self::ToolCall { call_id } => call_id.as_str().trim().is_empty(),
        };
        if invalid {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidAwaitEventWaitIdentity,
                "await-event wait identity requires non-empty stable ids",
            ));
        }
        Ok(())
    }
}
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct AwaitEventKey {
    pub scope: ExecutionScope,
    pub wait: AwaitEventWaitIdentity,
    pub key_id: String,
    pub signature: String,
}
impl AwaitEventKey {
    /// Derives the deterministic promise key effect-host implementors use to rendezvous durable
    /// wait resolution with its execution scope and wait identity.
    pub fn promise_key(&self) -> String {
        format!("lash-await-event:{}", self.key_id)
    }
}

impl crate::store::DurableRecord for AwaitEventKey {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::artifact_referrer::ARTIFACT_REFERRER_KINDS_VERSION);
}
