//! An [`ExecutionScope`] admitted for effect work, paired with the process
//! incarnation it was admitted under when it is a process.
//!
//! [`ExecutionScope::Process`](crate::ExecutionScope::Process) carries the
//! reusable process name and nothing else
//! (`crates/lash-sansio/src/effect_identity.rs`), while ADR 0099 §1 names the
//! pair — the name bound to one store-minted incarnation — as the opener.
//! Keeping the pin inside the same value as the scope makes the half-admitted
//! shape unconstructible: a `Process` scope cannot exist here without naming
//! its incarnation, and an incarnation cannot pin a scope that is not the
//! process it belongs to. Controller construction and opener derivation take
//! this value rather than the two halves as separate arguments, so the
//! forgetting is unrepresentable.

use crate::process_identity::ProcessRef;
use crate::{ExecutionScope, ProcessId, RuntimeError, RuntimeErrorCode, SessionId, TurnId};

/// An [`ExecutionScope`] admitted for effect work.
///
/// `scope` is the claim address a journal row fences on; `process` is the
/// [`ProcessRef`] the admission authority bound when `scope` is a process —
/// `Some` exactly then. The constructors keep the two halves consistent:
/// there is no way to hold a `Process` scope without its incarnation, a pin
/// that names a different process, or a pin at all on a scope kind that is
/// not a process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmittedScope {
    scope: ExecutionScope,
    process: Option<ProcessRef>,
}

impl AdmittedScope {
    /// Admits `scope` for effect work, pinned to `process` when the scope is a
    /// process.
    ///
    /// The one checked construction. `Process` requires `Some` naming its own
    /// incarnation; every other scope kind requires `None` — an incarnation
    /// has no owner to bind anywhere else.
    ///
    /// # Errors
    ///
    /// * [`AdmittedScopeError::ProcessIncarnationMissing`] — a `Process` scope
    ///   with no incarnation.
    /// * [`AdmittedScopeError::ProcessPinMismatch`] — a `Process` scope pinned
    ///   to an incarnation of another process.
    /// * [`AdmittedScopeError::NonProcessScopePinned`] — an incarnation pinned
    ///   onto a scope kind that is not a process.
    pub fn new(
        scope: ExecutionScope,
        process: Option<ProcessRef>,
    ) -> Result<Self, AdmittedScopeError> {
        match (&scope, &process) {
            (ExecutionScope::Process { process_id }, Some(process_ref)) => {
                if process_ref.process_id == *process_id {
                    Ok(Self { scope, process })
                } else {
                    Err(AdmittedScopeError::ProcessPinMismatch {
                        process_id: process_id.clone(),
                        pinned: process_ref.process_id.clone(),
                    })
                }
            }
            (ExecutionScope::Process { process_id }, None) => {
                Err(AdmittedScopeError::ProcessIncarnationMissing {
                    process_id: process_id.clone(),
                })
            }
            (_, Some(process_ref)) => Err(AdmittedScopeError::NonProcessScopePinned {
                scope_kind: scope_kind(&scope),
                pinned: process_ref.process_id.clone(),
            }),
            (_, None) => Ok(Self { scope, process }),
        }
    }

    /// Admits a scope that is not a process.
    ///
    /// The same check [`new`](Self::new) performs with no pin: a `Process`
    /// scope without its incarnation is refused.
    ///
    /// # Errors
    ///
    /// [`AdmittedScopeError::ProcessIncarnationMissing`] — the scope is a
    /// process.
    pub fn unpinned(scope: ExecutionScope) -> Result<Self, AdmittedScopeError> {
        Self::new(scope, None)
    }

    /// The admitted scope of one process incarnation: the pair an admission
    /// authority returns, which is an opener by itself (ADR 0099 §1).
    #[must_use]
    pub fn process(process_ref: ProcessRef) -> Self {
        Self {
            scope: ExecutionScope::process(process_ref.process_id.clone()),
            process: Some(process_ref),
        }
    }

    /// One admitted turn.
    #[must_use]
    pub fn turn(session_id: impl Into<SessionId>, turn_id: impl Into<TurnId>) -> Self {
        Self {
            scope: ExecutionScope::turn(session_id, turn_id),
            process: None,
        }
    }

    /// One admitted queued-work drain.
    #[must_use]
    pub fn queue_drain(session_id: impl Into<SessionId>, drain_id: impl Into<String>) -> Self {
        Self {
            scope: ExecutionScope::queue_drain(session_id, drain_id),
            process: None,
        }
    }

    /// One admitted session-delete scope.
    #[must_use]
    pub fn session_delete(session_id: impl Into<SessionId>) -> Self {
        Self {
            scope: ExecutionScope::session_delete(session_id),
            process: None,
        }
    }

    /// One admitted runtime operation.
    #[must_use]
    pub fn runtime_operation(operation_id: impl Into<String>) -> Self {
        Self {
            scope: ExecutionScope::runtime_operation(operation_id),
            process: None,
        }
    }

    /// The execution scope this pair was admitted under.
    #[must_use]
    pub fn scope(&self) -> &ExecutionScope {
        &self.scope
    }

    /// The incarnation a process scope was admitted under. `None` for every
    /// scope kind that is not a process — never "not yet bound".
    #[must_use]
    pub fn process_ref(&self) -> Option<&ProcessRef> {
        self.process.as_ref()
    }

    /// The bare claim address, discarding the admission pin.
    #[must_use]
    pub fn into_scope(self) -> ExecutionScope {
        self.scope
    }
}

fn scope_kind(scope: &ExecutionScope) -> &'static str {
    match scope {
        ExecutionScope::Turn { .. } => "turn",
        ExecutionScope::Process { .. } => "process",
        ExecutionScope::QueueDrain { .. } => "queue-drain",
        ExecutionScope::SessionDelete { .. } => "session-delete",
        ExecutionScope::RuntimeOperation { .. } => "runtime-operation",
    }
}

/// A scope/admission pair the contract refuses.
///
/// Construction-time refusals: [`AdmittedScope`] is the only value controller
/// construction and opener derivation accept, so a pair that fails these
/// checks can never travel farther into the runtime.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AdmittedScopeError {
    /// A process scope with no admitted incarnation.
    ///
    /// `ExecutionScope::Process` carries the reusable process *name*, and the
    /// name alone is not an identity: it would alias every earlier
    /// incarnation's groups, closes and cancellation fences (ADR 0099 §1).
    /// The admission authority supplies the incarnation at construction, so
    /// this refusal means the scope arrived before the authority answered.
    #[error("process `{process_id}` was not admitted with an incarnation, so it has no opener")]
    ProcessIncarnationMissing {
        /// The reusable process name the scope carried.
        process_id: ProcessId,
    },
    /// A process scope pinned to an incarnation of another process.
    #[error(
        "process `{process_id}` cannot open work as process `{pinned}`: the pinned incarnation must be the scope's own"
    )]
    ProcessPinMismatch {
        /// The process the scope names.
        process_id: ProcessId,
        /// The process the pinned incarnation names.
        pinned: ProcessId,
    },
    /// An incarnation pinned onto a scope kind that is not a process.
    #[error(
        "process `{pinned}` cannot pin a {scope_kind} scope: only a process scope carries an admitted incarnation"
    )]
    NonProcessScopePinned {
        /// The scope kind the pin was attempted on.
        scope_kind: &'static str,
        /// The process the pinned incarnation names.
        pinned: ProcessId,
    },
}

impl From<AdmittedScopeError> for RuntimeError {
    fn from(error: AdmittedScopeError) -> Self {
        RuntimeError::new(
            RuntimeErrorCode::ExecutionScopeAdmissionRefused,
            error.to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_identity::ProcessIncarnation;

    fn process_ref(name: &str, incarnation: u64) -> ProcessRef {
        ProcessRef::new(
            name,
            ProcessIncarnation::from_registration_sequence(incarnation),
        )
    }

    /// The invariant the type exists for: the half-admitted shape cannot be
    /// built, so no consumer ever sees a process scope without its pin.
    #[test]
    fn a_process_scope_without_an_incarnation_is_unconstructible() {
        assert_eq!(
            AdmittedScope::unpinned(ExecutionScope::process("worker")),
            Err(AdmittedScopeError::ProcessIncarnationMissing {
                process_id: ProcessId::from("worker"),
            })
        );
        assert_eq!(
            AdmittedScope::new(ExecutionScope::process("worker"), None),
            Err(AdmittedScopeError::ProcessIncarnationMissing {
                process_id: ProcessId::from("worker"),
            })
        );
    }

    /// A pin for another process cannot open this one's work.
    #[test]
    fn a_process_scope_pinned_to_another_process_is_refused() {
        assert_eq!(
            AdmittedScope::new(
                ExecutionScope::process("worker"),
                Some(process_ref("indexer", 1)),
            ),
            Err(AdmittedScopeError::ProcessPinMismatch {
                process_id: ProcessId::from("worker"),
                pinned: ProcessId::from("indexer"),
            })
        );
    }

    /// An incarnation has no owner outside a process scope.
    #[test]
    fn a_non_process_scope_refuses_a_pin() {
        for scope in [
            ExecutionScope::turn("s", "t"),
            ExecutionScope::queue_drain("s", "d"),
            ExecutionScope::session_delete("s"),
            ExecutionScope::runtime_operation("op"),
        ] {
            assert!(
                matches!(
                    AdmittedScope::new(scope.clone(), Some(process_ref("worker", 1))),
                    Err(AdmittedScopeError::NonProcessScopePinned { .. })
                ),
                "{scope:?} must refuse a process pin"
            );
        }
    }

    /// `AdmittedScope::process` builds the pair an authority CAS hands back:
    /// the scope is the pin's own process, so the two halves cannot disagree.
    #[test]
    fn the_process_constructor_derives_the_scope_from_the_pin() {
        let admitted = AdmittedScope::process(process_ref("worker", 4));
        assert_eq!(
            admitted.scope(),
            &ExecutionScope::process("worker"),
            "the scope names the pinned process, not a paraphrase of it"
        );
        assert_eq!(admitted.process_ref(), Some(&process_ref("worker", 4)));
    }

    /// The non-process constructors carry no pin by construction.
    #[test]
    fn non_process_scopes_carry_no_pin() {
        for admitted in [
            AdmittedScope::turn("s", "t"),
            AdmittedScope::queue_drain("s", "d"),
            AdmittedScope::session_delete("s"),
            AdmittedScope::runtime_operation("op"),
            AdmittedScope::unpinned(ExecutionScope::turn("s", "t"))
                .expect("a turn admits unpinned"),
        ] {
            assert!(admitted.process_ref().is_none(), "{admitted:?}");
        }
    }

    /// A checked `new` with a matching pin admits the pair.
    #[test]
    fn a_process_scope_with_its_own_incarnation_admits() {
        let admitted = AdmittedScope::new(
            ExecutionScope::process("worker"),
            Some(process_ref("worker", 2)),
        )
        .expect("the pin names the scope's process");
        assert_eq!(admitted.process_ref(), Some(&process_ref("worker", 2)));
    }
}
