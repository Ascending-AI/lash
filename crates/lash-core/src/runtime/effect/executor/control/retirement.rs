//! Scope-retirement gates and the external-completion resolution vocabulary.
//!
//! Split out of `control.rs` verbatim to keep every file in this module under
//! the production file-size budget; no item, signature or path changed.

use super::*;

// =============================================================================
// Effect host + controller trait + scope + error
// =============================================================================

pub use lash_sansio::{EffectJournalIdentity, ExecutionScope};

/// Who proves that a scope-exact retirement can no longer be reached.
///
/// Retirement deletes journal rows and fences the scope forever, so it must
/// rest on proven unreachability (ADR 0049 reclaim model). There are exactly
/// two proofs, and the caller names which one it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectRetirementGate {
    /// The scope's owner is terminal by the owner's own record: the process
    /// registry pruned the row, so no redrive can ever run under the scope
    /// again. The store deletes whatever is journaled, in-flight rows
    /// included — a finalizer that arrives later is refused by the fence.
    OwnerTerminal,
    /// The store itself must witness that nothing is live: no effect row is
    /// still `in_progress` under the scope (grouped children draining after a
    /// run-to-completion close count). A live scope is left untouched and the
    /// retirement reports `effect_scope_not_quiescent`, so the caller retries
    /// once the work settles.
    WhenQuiescent,
}

/// One retirement request against the durable effect journal.
///
/// `Session` names a family of scopes (every turn, drain, and delete scope the
/// session owns); `Process` and `RuntimeOperation` each name one exact
/// non-session scope. Retiring an exact scope deletes its effect children, its
/// groups, and its await-event promise rows in one transaction and leaves a
/// scope-retirement fence behind, so the scope can never be re-admitted — not
/// by a late redrive, not after a restart. A process fence lasts until the
/// same process id is registered again (ADR 0049); a runtime-operation fence
/// is permanent.
///
/// Every scope-exact retirement carries an [`EffectRetirementGate`]: the
/// constructors build the owner-terminal form, and
/// [`when_quiescent`](Self::when_quiescent) asks the store to prove
/// unreachability instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectJournalRetirement {
    Session {
        session_id: SessionId,
    },
    Process {
        process_id: ProcessId,
        gate: EffectRetirementGate,
    },
    RuntimeOperation {
        operation_id: String,
        gate: EffectRetirementGate,
    },
}

impl EffectJournalRetirement {
    /// Constructs a session-wide retirement request for effect-host implementors removing every
    /// durable effect journal entry owned by a deleted session.
    pub fn session(session_id: impl Into<SessionId>) -> Self {
        Self::Session {
            session_id: session_id.into(),
        }
    }

    /// Constructs a process-wide retirement request for effect-host implementors removing every
    /// durable effect journal entry owned by a terminal process. The gate is
    /// [`EffectRetirementGate::OwnerTerminal`]: the process registry's prune is
    /// the proof, so in-flight rows go too.
    pub fn process(process_id: impl Into<ProcessId>) -> Self {
        Self::Process {
            process_id: process_id.into(),
            gate: EffectRetirementGate::OwnerTerminal,
        }
    }

    /// Constructs a runtime-operation retirement request for effect-host implementors removing
    /// every durable effect journal entry and await-event promise a terminal runtime operation
    /// owns. The gate is [`EffectRetirementGate::OwnerTerminal`]; a caller that
    /// holds no such proof asks for [`when_quiescent`](Self::when_quiescent).
    pub fn runtime_operation(operation_id: impl Into<String>) -> Self {
        Self::RuntimeOperation {
            operation_id: operation_id.into(),
            gate: EffectRetirementGate::OwnerTerminal,
        }
    }

    /// Gate this scope-exact retirement on the store's own quiescence proof:
    /// it succeeds only when no effect is still in progress under the scope,
    /// and otherwise fails with `effect_scope_not_quiescent` without deleting
    /// or fencing anything. A session-wide retirement is returned unchanged.
    #[must_use]
    pub fn when_quiescent(self) -> Self {
        match self {
            Self::Session { .. } => self,
            Self::Process { process_id, .. } => Self::Process {
                process_id,
                gate: EffectRetirementGate::WhenQuiescent,
            },
            Self::RuntimeOperation { operation_id, .. } => Self::RuntimeOperation {
                operation_id,
                gate: EffectRetirementGate::WhenQuiescent,
            },
        }
    }

    /// The proof this scope-exact retirement rests on, or `None` for a
    /// session-wide family, which is always owner-terminal by construction.
    pub fn gate(&self) -> Option<EffectRetirementGate> {
        match self {
            Self::Session { .. } => None,
            Self::Process { gate, .. } | Self::RuntimeOperation { gate, .. } => Some(*gate),
        }
    }

    /// The exact scope this retirement fences, or `None` for a session-wide
    /// family. The scope-retirement fence is keyed by this scope's journal
    /// identity, which is why the two non-session variants and their
    /// [`ExecutionScope`] twins must never drift apart.
    pub fn retired_scope(&self) -> Option<ExecutionScope> {
        match self {
            Self::Session { .. } => None,
            Self::Process { process_id, .. } => Some(ExecutionScope::process(process_id.clone())),
            Self::RuntimeOperation { operation_id, .. } => {
                Some(ExecutionScope::runtime_operation(operation_id.clone()))
            }
        }
    }

    /// The retirement that fences exactly `scope`, or `None` for a
    /// session-bearing scope, which is retired as a family through
    /// [`EffectJournalRetirement::session`].
    pub fn for_scope(scope: &ExecutionScope) -> Option<Self> {
        match scope {
            ExecutionScope::Process { process_id } => Some(Self::process(process_id.clone())),
            ExecutionScope::RuntimeOperation { operation_id } => {
                Some(Self::runtime_operation(operation_id.clone()))
            }
            ExecutionScope::Turn { .. }
            | ExecutionScope::QueueDrain { .. }
            | ExecutionScope::SessionDelete { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalCompletionError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
pub enum Resolution {
    Ok(serde_json::Value),
    Err(ExternalCompletionError),
    Timeout,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResolveOutcome {
    Accepted,
    AlreadyResolved { terminal: Resolution },
    UnknownOrRevoked,
}
