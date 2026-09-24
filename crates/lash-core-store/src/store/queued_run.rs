//! Durable admission of one queued logical run.

use super::{SessionExecutionLeaseAuthority, StoreError};
use crate::{BatchId, ExecutionScope, InputId, PersistedSessionConfig, SessionId, TurnId};

/// The selection named by the host. Automatic retries resume the pending run;
/// exact selections must match their original ordered request.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum QueuedRunRequest {
    Automatic,
    Selected { batch_ids: Vec<BatchId> },
}

/// Whether admission received a caller-supplied identity. A later automatic
/// wake can resume either origin, so the current request cannot infer it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum QueuedRunOrigin {
    Anonymous,
    Explicit,
}

/// Initial physical execution position, retained before any effect executes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueuedRunPosition {
    pub physical_ordinal: u64,
    pub turn_index: u64,
    pub turn_id: TurnId,
}

impl QueuedRunPosition {
    pub fn derive_turn_id(root: &TurnId, physical_ordinal: u64) -> TurnId {
        if physical_ordinal == 0 {
            root.clone()
        } else {
            TurnId::from(format!("{root}:agent-frame:{physical_ordinal}"))
        }
    }

    pub fn next(&self, scope: &ExecutionScope) -> Result<Self, StoreError> {
        let physical_ordinal = StoreError::checked_monotonic_increment(
            "queued_run_physical_ordinal",
            self.physical_ordinal,
        )?;
        Ok(Self {
            physical_ordinal,
            turn_index: StoreError::checked_monotonic_increment(
                "queued_run_turn_index",
                self.turn_index,
            )?,
            turn_id: Self::derive_turn_id(&TurnId::from(scope.id()), physical_ordinal),
        })
    }
}

/// Ordered references remain admission evidence after the queue rows settle.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum QueuedRunMember {
    Input(InputId),
    Batch(BatchId),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct QueuedRunAdmission {
    pub scope: ExecutionScope,
    pub request: QueuedRunRequest,
    pub origin: QueuedRunOrigin,
    pub configuration: PersistedSessionConfig,
    pub revision: u64,
    pub position: QueuedRunPosition,
    /// `None` means selection has not committed; an empty vector is a frozen
    /// empty selection and must never trigger another scan.
    pub members: Option<Vec<QueuedRunMember>>,
    pub initial_members: Option<Vec<QueuedRunMember>>,
    pub withheld_members: Vec<QueuedRunMember>,
    /// Work assigned by checkpoints, retained independently of physical claim generations.
    pub assigned_members: Vec<QueuedRunMember>,
    pub terminal: Option<QueuedRunTerminal>,
    pub last_commit: Option<QueuedRunCommit>,
}

/// Input to admission. Only an explicitly supplied identity is replayed after
/// settlement; an automatic wake starts distinct work after settlement.
#[derive(Clone, Debug)]
pub struct BeginQueuedRun {
    pub session_id: SessionId,
    pub identity: Option<ExecutionScope>,
    pub request: QueuedRunRequest,
    pub configuration: PersistedSessionConfig,
    pub expected_head_revision: u64,
    pub initial_turn_index: u64,
}

impl BeginQueuedRun {
    pub fn validate(&self, fence: &SessionExecutionLeaseAuthority) -> Result<(), StoreError> {
        if self.session_id != fence.session_id {
            return Err(StoreError::SessionExecutionLeaseExpired {
                session_id: self.session_id.clone(),
            });
        }
        if let Some(scope) = &self.identity {
            if !matches!(scope, ExecutionScope::QueueDrain { session_id, .. } if session_id == self.session_id)
            {
                return Err(StoreError::Backend(
                    "queued run identity must name this session's queue drain".into(),
                ));
            }
            scope
                .validate()
                .map_err(|error| StoreError::Backend(error.to_string()))?;
        }
        Ok(())
    }

    pub fn resume(&self, admission: &QueuedRunAdmission) -> Result<QueuedRunAdmission, StoreError> {
        if self
            .identity
            .as_ref()
            .is_some_and(|scope| scope != &admission.scope)
            || (self.request != admission.request
                && !(self.identity.is_none()
                    && matches!(self.request, QueuedRunRequest::Automatic)))
        {
            return Err(StoreError::QueuedRunConflict {
                session_id: self.session_id.clone(),
            });
        }
        if admission.terminal.is_none()
            && admission.members.is_some()
            && self.configuration != admission.configuration
        {
            return Err(StoreError::QueuedRunConfigurationChanged {
                session_id: self.session_id.clone(),
            });
        }
        let mut resumed = admission.clone();
        if self.identity.is_some() {
            resumed.origin = QueuedRunOrigin::Explicit;
        }
        Ok(resumed)
    }

    pub fn admit(self, drain_id: String) -> QueuedRunAdmission {
        let origin = if self.identity.is_some() {
            QueuedRunOrigin::Explicit
        } else {
            QueuedRunOrigin::Anonymous
        };
        let scope = self
            .identity
            .unwrap_or_else(|| ExecutionScope::queue_drain(&self.session_id, drain_id));
        let turn_id = TurnId::from(scope.id());
        QueuedRunAdmission {
            scope,
            request: self.request,
            origin,
            configuration: self.configuration,
            revision: 0,
            position: QueuedRunPosition {
                physical_ordinal: 0,
                turn_index: self.initial_turn_index,
                turn_id,
            },
            members: None,
            initial_members: None,
            withheld_members: Vec::new(),
            assigned_members: Vec::new(),
            terminal: None,
            last_commit: None,
        }
    }
}

/// Atomic selection returns payloads only through the existing claim records.
#[derive(Clone, Debug)]
pub struct SelectedQueuedRun {
    pub admission: QueuedRunAdmission,
    pub inputs: Vec<crate::turn_input_vocabulary::TurnInputClaim>,
    pub queued: Vec<crate::QueuedWorkClaim>,
    pub already_satisfied: Vec<BatchId>,
    pub refusal: Option<super::QueuedWorkClaimRefusal>,
    /// On resume, the still-open rows the run's checkpoints were assigned,
    /// retaken under the resuming generation (FIG-3552). They are not the
    /// run's input: a replayed checkpoint that delivered them settles them
    /// under these claims, so ownership moves only through the claim CAS.
    pub reacquired_inputs: Vec<crate::turn_input_vocabulary::TurnInputClaim>,
    /// The queued-work counterpart of [`Self::reacquired_inputs`].
    pub reacquired_queued: Vec<crate::QueuedWorkClaim>,
}

impl QueuedRunAdmission {
    /// Checkpoint-assigned rows that are not current members: the rows a
    /// resume retakes beside its members (FIG-3552).
    pub fn assigned_non_members(&self) -> impl Iterator<Item = &QueuedRunMember> {
        self.assigned_members.iter().filter(|member| {
            !self
                .members
                .iter()
                .flatten()
                .any(|current| current == *member)
        })
    }
}

/// Durable terminal evidence returned by an explicit-id replay.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum QueuedRunTerminal {
    Completed {
        turn_id: TurnId,
        outcome: lash_sansio::TurnOutcome,
    },
    Empty,
    Failed {
        code: crate::RuntimeErrorCode,
        message: String,
    },
}

/// Admission progress carried by the physical commit. New outbox batch IDs
/// are resolved by the store in the same transaction that creates them.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum QueuedRunProgress {
    Advance {
        position: QueuedRunPosition,
        members: Vec<QueuedRunMember>,
        withheld_members: Vec<QueuedRunMember>,
        include_outbox: bool,
    },
    Settle {
        terminal: QueuedRunTerminal,
    },
    /// Forget an unnamed run that started no physical work, atomically with
    /// its terminal disposition. No replay identity is exposed to its caller.
    ForgetUnworked,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QueuedRunCommit {
    pub scope: ExecutionScope,
    pub expected_revision: u64,
    pub progress: QueuedRunProgress,
}

impl QueuedRunAdmission {
    pub fn can_forget_unworked(&self) -> bool {
        self.origin == QueuedRunOrigin::Anonymous
            && self.terminal.is_none()
            && self.position.physical_ordinal == 0
            && self.initial_members.as_ref().is_none_or(Vec::is_empty)
            && self.members.as_ref().is_none_or(Vec::is_empty)
            && self.withheld_members.is_empty()
            && self.assigned_members.is_empty()
            && self.last_commit.is_none()
    }

    pub fn owns_member(&self, member: &QueuedRunMember) -> bool {
        self.initial_members
            .iter()
            .flatten()
            .chain(self.members.iter().flatten())
            .chain(&self.withheld_members)
            .chain(&self.assigned_members)
            .any(|owned| owned == member)
    }

    pub fn already_satisfied_batch_ids(&self) -> Vec<BatchId> {
        let (QueuedRunRequest::Selected { batch_ids }, Some(initial_members)) =
            (&self.request, &self.initial_members)
        else {
            return Vec::new();
        };
        batch_ids
            .iter()
            .filter(|id| {
                !initial_members.iter().any(
                    |member| matches!(member, QueuedRunMember::Batch(selected) if selected == *id),
                )
            })
            .cloned()
            .collect()
    }

    pub fn advance(
        &self,
        commit: &QueuedRunCommit,
        outbox: &[crate::QueuedWorkBatch],
    ) -> Result<Self, StoreError> {
        let session_id = self.scope.session_id().ok_or_else(|| {
            StoreError::Backend("queued admission has no session identity".into())
        })?;
        if self.scope != commit.scope {
            return Err(StoreError::QueuedRunConflict {
                session_id: session_id.clone(),
            });
        }
        if self.last_commit.as_ref().is_some_and(|previous| {
            previous.scope == commit.scope && previous.progress == commit.progress
        }) {
            return Ok(self.clone());
        }
        if self.terminal.is_some() || self.revision != commit.expected_revision {
            return Err(StoreError::QueuedRunConflict {
                session_id: session_id.clone(),
            });
        }
        let mut next = self.clone();
        next.revision =
            StoreError::checked_monotonic_increment("queued_run_revision", self.revision)?;
        match &commit.progress {
            QueuedRunProgress::Advance {
                position,
                members,
                withheld_members,
                include_outbox,
            } => {
                if position != &self.position.next(&self.scope)? {
                    return Err(StoreError::QueuedRunConflict {
                        session_id: session_id.clone(),
                    });
                }
                next.position = position.clone();
                let mut members = members.clone();
                if *include_outbox {
                    members.extend(
                        outbox
                            .iter()
                            .map(|batch| QueuedRunMember::Batch(batch.batch_id.clone())),
                    );
                }
                next.members = Some(members);
                next.withheld_members = withheld_members.clone();
            }
            QueuedRunProgress::Settle { terminal } => {
                if let QueuedRunTerminal::Completed { turn_id, .. } = terminal
                    && turn_id != self.position.turn_id
                {
                    return Err(StoreError::QueuedRunConflict {
                        session_id: session_id.clone(),
                    });
                }
                next.terminal = Some(terminal.clone());
            }
            QueuedRunProgress::ForgetUnworked => {
                if !self.can_forget_unworked() {
                    return Err(StoreError::QueuedRunConflict {
                        session_id: session_id.clone(),
                    });
                }
                next.terminal = Some(QueuedRunTerminal::Empty);
            }
        }
        next.last_commit = Some(commit.clone());
        Ok(next)
    }
}
