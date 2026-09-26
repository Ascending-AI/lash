//! An [`ExecutionScope`] admitted for effect work.
//!
//! Controller construction and opener derivation take this value rather than a
//! bare scope, so the one place a scope becomes admitted stays visible. A
//! process scope names its process by the minted, never-reused process id
//! (ADR 0107), which is an opener by itself (ADR 0099 §1): no incarnation is
//! pinned beside it, because no successor can ever share the id.

use crate::{ExecutionScope, ProcessId, SessionId, TurnId};

/// An [`ExecutionScope`] admitted for effect work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmittedScope {
    scope: ExecutionScope,
}

impl AdmittedScope {
    /// Admits `scope` for effect work.
    #[must_use]
    pub fn new(scope: ExecutionScope) -> Self {
        Self { scope }
    }

    /// The admitted scope of one process.
    #[must_use]
    pub fn process(process_id: ProcessId) -> Self {
        Self::new(ExecutionScope::process(process_id))
    }

    /// One admitted turn.
    #[must_use]
    pub fn turn(session_id: impl Into<SessionId>, turn_id: impl Into<TurnId>) -> Self {
        Self::new(ExecutionScope::turn(session_id, turn_id))
    }

    /// One admitted queued-work drain.
    #[must_use]
    pub fn queue_drain(session_id: impl Into<SessionId>, drain_id: impl Into<String>) -> Self {
        Self::new(ExecutionScope::queue_drain(session_id, drain_id))
    }

    /// One admitted session-delete scope.
    #[must_use]
    pub fn session_delete(session_id: impl Into<SessionId>) -> Self {
        Self::new(ExecutionScope::session_delete(session_id))
    }

    /// One admitted runtime operation.
    #[must_use]
    pub fn runtime_operation(operation_id: impl Into<String>) -> Self {
        Self::new(ExecutionScope::runtime_operation(operation_id))
    }

    /// The execution scope that was admitted.
    #[must_use]
    pub fn scope(&self) -> &ExecutionScope {
        &self.scope
    }

    /// The process this scope is, when it is one.
    #[must_use]
    pub fn process_id(&self) -> Option<&ProcessId> {
        match &self.scope {
            ExecutionScope::Process { process_id } => Some(process_id),
            _ => None,
        }
    }

    /// The bare claim address.
    #[must_use]
    pub fn into_scope(self) -> ExecutionScope {
        self.scope
    }
}

/// The wire shape of an admitted scope, for `#[serde(with = ...)]`: the scope
/// alone.
///
/// A process scope names its minted process id (ADR 0107), so there is no
/// second half to carry. The retired shape that pinned an incarnation beside
/// the scope is refused at decode (`deny_unknown_fields`), never
/// reinterpreted.
pub mod wire {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::AdmittedScope;
    use crate::ExecutionScope;

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct AdmittedScopeWire {
        scope: ExecutionScope,
    }

    /// Serializes `admitted` as its scope.
    ///
    /// # Errors
    ///
    /// Whatever `serializer` reports.
    pub fn serialize<S: Serializer>(
        admitted: &AdmittedScope,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        AdmittedScopeWire {
            scope: admitted.scope().clone(),
        }
        .serialize(serializer)
    }

    /// Decodes the scope and admits it.
    ///
    /// # Errors
    ///
    /// A malformed shape, including the retired incarnation-pinned one.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<AdmittedScope, D::Error> {
        let wire = AdmittedScopeWire::deserialize(deserializer)?;
        Ok(AdmittedScope::new(wire.scope))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_process_constructor_is_the_process_scope() {
        let process = crate::process_identity::process_id_for_test("worker");
        let admitted = AdmittedScope::process(process.clone());
        assert_eq!(admitted.scope(), &ExecutionScope::process(process.clone()));
        assert_eq!(admitted.process_id(), Some(&process));
    }

    #[test]
    fn non_process_scopes_name_no_process() {
        for admitted in [
            AdmittedScope::turn("s", "t"),
            AdmittedScope::queue_drain("s", "d"),
            AdmittedScope::session_delete("s"),
            AdmittedScope::runtime_operation("op"),
        ] {
            assert_eq!(admitted.process_id(), None, "{admitted:?}");
        }
    }
}
