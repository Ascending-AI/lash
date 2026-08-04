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
    /// Compares owner incarnation, fencing token, and attempt for process-store implementors
    /// deciding whether two facts belong to the same execution generation.
    pub fn same_execution(&self, other: &Self) -> bool {
        self.owner.same_incarnation(&other.owner)
            && self.fencing_token == other.fencing_token
            && self.attempt == other.attempt
    }
}

/// Correctness fence presented by the process execution that writes runtime
/// lifecycle facts. Lash workers present their persisted lease; durable
/// substrates present a replay-stable execution id bound to one attempt.
/// Restate successors within that attempt share the root execution id, so this
/// authority fences attempt generations rather than individual segments.
#[derive(Clone, Debug)]
pub enum ProcessExecutionWriteAuthority {
    Lease(ProcessLease),
    Invocation {
        process_id: String,
        execution_id: String,
        attempt: Option<u32>,
        resume_from: Option<ProcessStarted>,
    },
}

impl ProcessExecutionWriteAuthority {
    /// Constructs owner-bound resume authority for durable-substrate implementors, pinning the
    /// exact retained execution that may hand over.
    pub fn invocation_resume(
        process_id: impl Into<String>,
        execution_id: impl Into<String>,
        resume_from: ProcessStarted,
    ) -> Self {
        Self::Invocation {
            process_id: process_id.into(),
            execution_id: execution_id.into(),
            attempt: None,
            resume_from: Some(resume_from),
        }
    }

    /// Constructs a `ProcessExecutionWriteAuthority` using lease semantics for store and
    /// durable-substrate implementors while persisting and coordinating durable process execution.
    pub fn lease(lease: ProcessLease) -> Self {
        Self::Lease(lease)
    }

    /// Constructs a `ProcessExecutionWriteAuthority` using invocation semantics for store and
    /// durable-substrate implementors while persisting and coordinating durable process execution.
    pub fn invocation(process_id: impl Into<String>, execution_id: impl Into<String>) -> Self {
        Self::Invocation {
            process_id: process_id.into(),
            execution_id: execution_id.into(),
            attempt: None,
            resume_from: None,
        }
    }

    /// Binds invocation authority to one attempt for durable-substrate implementors; lease
    /// authority already carries its generation and is unchanged.
    pub fn bind_attempt(&self, attempt: u32) -> Self {
        match self {
            Self::Lease(lease) => Self::Lease(lease.clone()),
            Self::Invocation {
                process_id,
                execution_id,
                resume_from,
                ..
            } => Self::Invocation {
                process_id: process_id.clone(),
                execution_id: execution_id.clone(),
                attempt: Some(attempt),
                resume_from: resume_from.clone(),
            },
        }
    }

    /// Projects a replay-stable started fact only after invocation authority is attempt-bound,
    /// returning `None` for leases and unbound invocations.
    pub fn invocation_started(&self) -> Option<ProcessStarted> {
        match self {
            Self::Lease(_) => None,
            Self::Invocation {
                process_id,
                execution_id,
                attempt: Some(attempt),
                ..
            } => Some(ProcessStarted {
                owner: crate::LeaseOwnerIdentity::restate_process_execution(
                    process_id,
                    execution_id.clone(),
                ),
                fencing_token: 0,
                attempt: *attempt,
                started_at_ms: 0,
            }),
            Self::Invocation { attempt: None, .. } => None,
        }
    }

    /// Permits durable handover only when the retained owner incarnation, fencing token, and
    /// attempt exactly match the predecessor captured by invocation authority.
    pub fn permits_owner_bound_resume(&self, retained: &ProcessStarted) -> bool {
        matches!(
            self,
            Self::Invocation {
                resume_from: Some(expected),
                ..
            } if expected.same_execution(retained)
        )
    }

    pub(crate) fn validate_resume_predecessor(
        &self,
        process_id: &str,
        retained: Option<&ProcessStarted>,
    ) -> Result<(), crate::PluginError> {
        let Self::Invocation {
            resume_from: Some(expected),
            ..
        } = self
        else {
            return Ok(());
        };
        if retained.is_some_and(|retained| retained.same_execution(expected)) {
            return Ok(());
        }
        self.trace_invocation_denial(
            process_id,
            None,
            retained,
            "durable handover predecessor does not match retained execution",
        );
        Err(crate::PluginError::ProcessLeaseSuperseded {
            process_id: process_id.to_string(),
        })
    }

    fn trace_invocation_denial(
        &self,
        process_id: &str,
        proposed_start: Option<&ProcessStarted>,
        retained: Option<&ProcessStarted>,
        reason: &'static str,
    ) {
        let Self::Invocation {
            process_id: authority_process_id,
            execution_id,
            attempt,
            ..
        } = self
        else {
            return;
        };
        let presented_owner = crate::LeaseOwnerIdentity::restate_process_execution(
            authority_process_id,
            execution_id,
        );
        tracing::warn!(
            process_id,
            presented_process_id = authority_process_id,
            presented_owner_id = presented_owner.owner_id,
            presented_invocation_id = execution_id,
            presented_attempt = ?attempt,
            presented_fencing_token = 0_u64,
            proposed_owner_id = proposed_start.map(|started| started.owner.owner_id.as_str()),
            proposed_invocation_id =
                proposed_start.map(|started| started.owner.incarnation_id.as_str()),
            proposed_attempt = proposed_start.map(|started| started.attempt),
            proposed_fencing_token = proposed_start.map(|started| started.fencing_token),
            retained_owner_id = retained.map(|started| started.owner.owner_id.as_str()),
            retained_invocation_id =
                retained.map(|started| started.owner.incarnation_id.as_str()),
            retained_attempt = retained.map(|started| started.attempt),
            retained_fencing_token = retained.map(|started| started.fencing_token),
            verdict = "denied",
            reason,
            "process invocation fence decision"
        );
    }

    /// Rejects a durable invocation start as superseded unless its process ID and complete
    /// execution identity match the presented authority.
    pub fn validate_invocation_for_start(
        &self,
        process_id: &str,
        started: &ProcessStarted,
        retained: Option<&ProcessStarted>,
    ) -> Result<(), crate::PluginError> {
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
            self.trace_invocation_denial(
                process_id,
                Some(started),
                retained,
                "presented start identity does not match authority",
            );
            return Err(crate::PluginError::ProcessLeaseSuperseded {
                process_id: process_id.to_string(),
            });
        }
        Ok(())
    }

    /// Rejects a durable invocation write as superseded unless its process ID and complete
    /// execution identity match the record's retained current execution.
    pub fn validate_invocation_for_write(
        &self,
        process_id: &str,
        record: &ProcessRecord,
    ) -> Result<(), crate::PluginError> {
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
            self.trace_invocation_denial(
                process_id,
                None,
                current,
                "presented write identity does not match retained execution",
            );
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
    /// Classifies a repeated completion as already applied only when the stored terminal outcome
    /// exactly equals the proposal; a different retained outcome is superseding evidence.
    pub fn from_stored(record: ProcessRecord, proposed: &super::ProcessAwaitOutput) -> Self {
        if record.outcome.as_ref() == Some(proposed) {
            Self::AlreadyApplied { stored: record }
        } else {
            Self::Superseded { stored: record }
        }
    }

    pub(crate) fn stored(&self) -> &ProcessRecord {
        match self {
            Self::Committed(record)
            | Self::AlreadyApplied { stored: record }
            | Self::Superseded { stored: record } => record,
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn into_record(self) -> ProcessRecord {
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
    pub(crate) fn into_record(self) -> Result<ProcessRecord, crate::PluginError> {
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
    /// from their replay-stable invocation identity, so it is deliberately not
    /// serialized.
    #[serde(skip)]
    pub execution_write_authority: Option<ProcessExecutionWriteAuthority>,
}

impl ProcessExecutionContext {
    /// Sets the causal invocation carried by a `ProcessExecutionContext` for store and
    /// process-engine implementors while persisting and coordinating durable process execution.
    pub fn with_causal_invocation(mut self, invocation: Option<crate::RuntimeInvocation>) -> Self {
        self.causal_invocation = invocation;
        self
    }

    /// Sets the execution write authority carried by a `ProcessExecutionContext` for store and
    /// process-engine implementors while persisting and coordinating durable process execution.
    pub fn with_execution_write_authority(
        mut self,
        authority: ProcessExecutionWriteAuthority,
    ) -> Self {
        self.execution_write_authority = Some(authority);
        self
    }

    /// Lets store and process-engine implementors test whether this `ProcessExecutionContext` is
    /// empty while persisting and coordinating durable process execution.
    pub fn is_empty(&self) -> bool {
        self.causal_invocation.is_none()
    }
}
