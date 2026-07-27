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
/// lifecycle facts. Lash workers present their persisted lease; durable
/// substrates present a replay-stable execution id bound to one attempt.
#[derive(Clone, Debug)]
pub enum ProcessExecutionWriteAuthority {
    Lease(ProcessLease),
    Invocation {
        process_id: String,
        execution_id: String,
        attempt: Option<u32>,
    },
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    Testing {
        process_id: String,
    },
}

impl ProcessExecutionWriteAuthority {
    pub fn lease(lease: ProcessLease) -> Self {
        Self::Lease(lease)
    }

    pub fn invocation(process_id: impl Into<String>, execution_id: impl Into<String>) -> Self {
        Self::Invocation {
            process_id: process_id.into(),
            execution_id: execution_id.into(),
            attempt: None,
        }
    }

    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub fn testing(process_id: impl Into<String>) -> Self {
        Self::Testing {
            process_id: process_id.into(),
        }
    }

    pub fn bind_attempt(&self, attempt: u32) -> Self {
        match self {
            Self::Lease(lease) => Self::Lease(lease.clone()),
            Self::Invocation {
                process_id,
                execution_id,
                ..
            } => Self::Invocation {
                process_id: process_id.clone(),
                execution_id: execution_id.clone(),
                attempt: Some(attempt),
            },
            #[cfg(any(test, feature = "testing"))]
            Self::Testing { process_id } => Self::Testing {
                process_id: process_id.clone(),
            },
        }
    }

    pub fn lease_ref(&self) -> Option<&ProcessLease> {
        match self {
            Self::Lease(lease) => Some(lease),
            Self::Invocation { .. } => None,
            #[cfg(any(test, feature = "testing"))]
            Self::Testing { .. } => None,
        }
    }

    pub fn invocation_started(&self) -> Option<ProcessStarted> {
        match self {
            Self::Lease(_) => None,
            Self::Invocation {
                process_id,
                execution_id,
                attempt: Some(attempt),
            } => Some(ProcessStarted {
                owner: crate::LeaseOwnerIdentity::opaque(
                    format!("restate:{process_id}"),
                    execution_id.clone(),
                ),
                fencing_token: 0,
                attempt: *attempt,
                started_at_ms: 0,
            }),
            Self::Invocation { attempt: None, .. } => None,
            #[cfg(any(test, feature = "testing"))]
            Self::Testing { .. } => None,
        }
    }

    pub fn validate_invocation_for_start(
        &self,
        process_id: &str,
        started: &ProcessStarted,
    ) -> Result<(), crate::PluginError> {
        #[cfg(any(test, feature = "testing"))]
        if let Self::Testing {
            process_id: authority_process_id,
        } = self
        {
            return if authority_process_id == process_id {
                Ok(())
            } else {
                Err(crate::PluginError::ProcessLeaseSuperseded {
                    process_id: process_id.to_string(),
                })
            };
        }
        let Self::Invocation {
            process_id: authority_process_id,
            ..
        } = self
        else {
            return Ok(());
        };
        if authority_process_id != process_id
            || self
                .invocation_started()
                .is_none_or(|authority| !authority.same_execution(started))
        {
            return Err(crate::PluginError::ProcessLeaseSuperseded {
                process_id: process_id.to_string(),
            });
        }
        Ok(())
    }

    pub fn validate_invocation_for_write(
        &self,
        process_id: &str,
        record: &ProcessRecord,
    ) -> Result<(), crate::PluginError> {
        #[cfg(any(test, feature = "testing"))]
        if let Self::Testing {
            process_id: authority_process_id,
        } = self
        {
            return if authority_process_id == process_id {
                Ok(())
            } else {
                Err(crate::PluginError::ProcessLeaseSuperseded {
                    process_id: process_id.to_string(),
                })
            };
        }
        let Self::Invocation {
            process_id: authority_process_id,
            ..
        } = self
        else {
            return Ok(());
        };
        let current = record.first_started.as_deref();
        if authority_process_id != process_id
            || self.invocation_started().is_none_or(|authority| {
                current.is_none_or(|current| !current.same_execution(&authority))
            })
        {
            return Err(crate::PluginError::ProcessLeaseSuperseded {
                process_id: process_id.to_string(),
            });
        }
        Ok(())
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

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ProcessCompletionOutcome {
    Committed(ProcessRecord),
    AlreadyApplied { stored: ProcessRecord },
    Superseded { stored: ProcessRecord },
}

impl ProcessCompletionOutcome {
    pub fn from_stored(record: ProcessRecord, proposed: &super::ProcessAwaitOutput) -> Self {
        if record.status.await_output() == Some(proposed) {
            Self::AlreadyApplied { stored: record }
        } else {
            Self::Superseded { stored: record }
        }
    }

    pub fn stored(&self) -> &ProcessRecord {
        match self {
            Self::Committed(record)
            | Self::AlreadyApplied { stored: record }
            | Self::Superseded { stored: record } => record,
        }
    }

    pub fn into_record(self) -> ProcessRecord {
        match self {
            Self::Committed(record)
            | Self::AlreadyApplied { stored: record }
            | Self::Superseded { stored: record } => record,
        }
    }
}

impl std::ops::Deref for ProcessCompletionOutcome {
    type Target = ProcessRecord;

    fn deref(&self) -> &Self::Target {
        self.stored()
    }
}

impl ProcessStartOutcome {
    pub fn into_record(self) -> Result<ProcessRecord, crate::PluginError> {
        match self {
            Self::Started(record) | Self::AlreadyApplied(record) => Ok(record),
            Self::AlreadyStarted { current, by } => {
                Err(crate::PluginError::ProcessAlreadyStarted {
                    process_id: current.id,
                    by: Box::new(by),
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
