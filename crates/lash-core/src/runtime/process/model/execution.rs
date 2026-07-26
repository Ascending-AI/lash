use serde::{Deserialize, Serialize};

use super::{ProcessLease, ProcessRecord};

/// Durable execution-attempt fact. The fold retains the latest attempt so the
/// sweep can apply the producer's recovery disposition and attempt budget.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessStarted {
    pub owner: crate::LeaseOwnerIdentity,
    #[serde(default)]
    pub fencing_token: u64,
    #[serde(default = "first_process_attempt")]
    pub attempt: u32,
    pub started_at_ms: u64,
}

const fn first_process_attempt() -> u32 {
    1
}

impl ProcessStarted {
    pub fn same_execution(&self, other: &Self) -> bool {
        self.owner.same_incarnation(&other.owner)
            && self.fencing_token == other.fencing_token
            && self.attempt == other.attempt
    }
}

/// Correctness fence presented by the process execution that writes runtime
/// lifecycle facts. Lash workers present their persisted lease; substrate
/// workflows present the workflow key whose single-writer discipline replaces
/// a Lash lease.
#[derive(Clone, Debug)]
pub enum ProcessExecutionWriteAuthority {
    Lease(ProcessLease),
    WorkflowKey { workflow_key: String },
}

impl ProcessExecutionWriteAuthority {
    pub fn lease(lease: ProcessLease) -> Self {
        Self::Lease(lease)
    }

    pub fn workflow_key(workflow_key: impl Into<String>) -> Self {
        Self::WorkflowKey {
            workflow_key: workflow_key.into(),
        }
    }

    pub fn lease_ref(&self) -> Option<&ProcessLease> {
        match self {
            Self::Lease(lease) => Some(lease),
            Self::WorkflowKey { .. } => None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum ProcessStartOutcome {
    Started(ProcessRecord),
    AlreadyApplied(ProcessRecord),
    AlreadyStarted {
        current: ProcessRecord,
        by: crate::LeaseOwnerIdentity,
    },
    AttemptsExhausted {
        current: ProcessRecord,
        attempts: u32,
        max_attempts: u32,
    },
}

impl ProcessStartOutcome {
    pub fn into_record(self) -> Result<ProcessRecord, crate::PluginError> {
        match self {
            Self::Started(record) | Self::AlreadyApplied(record) => Ok(record),
            Self::AlreadyStarted { current, by } => {
                Err(crate::PluginError::ProcessAlreadyStarted {
                    process_id: current.id,
                    by,
                })
            }
            Self::AttemptsExhausted {
                current,
                attempts,
                max_attempts,
            } => Err(crate::PluginError::ProcessAttemptsExhausted {
                process_id: current.id,
                attempts,
                max_attempts,
            }),
        }
    }
}

/// Execution-local context for process runners. Durable edges live on the
/// process record.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProcessExecutionContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causal_invocation: Option<crate::RuntimeInvocation>,
    /// Execution-local correctness fence. Substrate handlers reconstruct it
    /// from their workflow key, so it is deliberately not serialized.
    #[serde(skip)]
    pub execution_write_authority: Option<ProcessExecutionWriteAuthority>,
}

impl ProcessExecutionContext {
    pub fn with_causal_invocation(mut self, invocation: Option<crate::RuntimeInvocation>) -> Self {
        self.causal_invocation = invocation;
        self
    }

    pub fn with_execution_write_authority(
        mut self,
        authority: ProcessExecutionWriteAuthority,
    ) -> Self {
        self.execution_write_authority = Some(authority);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.causal_invocation.is_none()
    }
}
