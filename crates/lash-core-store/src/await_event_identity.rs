//! Durable await-event wait identity.

use crate::{ExecutionScope, ProcessId, RuntimeError};
use serde::{Deserialize, Serialize};
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AwaitEventWaitIdentity {
    ProcessSignal {
        process_id: ProcessId,
        signal_name: String,
        ordinal: u64,
    },
}
impl AwaitEventWaitIdentity {
    pub fn process_signal(
        process_id: impl Into<ProcessId>,
        signal_name: impl Into<String>,
        ordinal: u64,
    ) -> Self {
        Self::ProcessSignal {
            process_id: process_id.into(),
            signal_name: signal_name.into(),
            ordinal,
        }
    }

    pub fn validate(&self) -> Result<(), RuntimeError> {
        let invalid = match self {
            Self::ProcessSignal {
                signal_name,
                ordinal,
                ..
            } => signal_name.trim().is_empty() || *ordinal == 0,
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
