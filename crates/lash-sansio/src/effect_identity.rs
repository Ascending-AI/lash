use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{ProcessId, SessionId, TurnId};

/// Stable semantic identity for one effectful runtime operation.
///
/// This is the scope admitted by the host boundary before nondeterministic
/// work begins. Its journal encoding is an existing durable contract; effect
/// addresses compose it with a replay key without changing those bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionScope {
    Turn {
        session_id: SessionId,
        turn_id: TurnId,
    },
    Process {
        process_id: ProcessId,
    },
    QueueDrain {
        session_id: SessionId,
        drain_id: String,
    },
    SessionDelete {
        session_id: SessionId,
    },
    RuntimeOperation {
        operation_id: String,
    },
}

impl ExecutionScope {
    pub fn turn(session_id: impl Into<SessionId>, turn_id: impl Into<TurnId>) -> Self {
        Self::Turn {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
        }
    }

    pub fn process(process_id: impl Into<ProcessId>) -> Self {
        Self::Process {
            process_id: process_id.into(),
        }
    }

    pub fn queue_drain(session_id: impl Into<SessionId>, drain_id: impl Into<String>) -> Self {
        Self::QueueDrain {
            session_id: session_id.into(),
            drain_id: drain_id.into(),
        }
    }

    pub fn session_delete(session_id: impl Into<SessionId>) -> Self {
        Self::SessionDelete {
            session_id: session_id.into(),
        }
    }

    pub fn runtime_operation(operation_id: impl Into<String>) -> Self {
        Self::RuntimeOperation {
            operation_id: operation_id.into(),
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Turn { turn_id, .. } => turn_id,
            Self::Process { process_id } => process_id,
            Self::QueueDrain { drain_id, .. } => drain_id,
            Self::SessionDelete { session_id, .. } => session_id,
            Self::RuntimeOperation { operation_id } => operation_id,
        }
    }

    /// Canonical typed identity persisted by durable effect journals.
    pub fn journal_identity(&self) -> Result<EffectJournalIdentity, EffectIdentityError> {
        self.validate()?;
        Ok(EffectJournalIdentity::from_scope(self))
    }

    /// The scope named by a persisted journal key, or `None` when this build
    /// cannot safely interpret the key.
    #[must_use]
    pub fn from_journal_key(key: &str) -> Option<Self> {
        #[derive(Deserialize)]
        struct Wire {
            version: u8,
            kind: String,
            #[serde(default)]
            session_id: Option<SessionId>,
            #[serde(default)]
            execution_id: Option<String>,
        }

        let wire: Wire = serde_json::from_str(key).ok()?;
        if wire.version != JOURNAL_IDENTITY_VERSION {
            return None;
        }
        let scope = match wire.kind.as_str() {
            "turn" => Self::Turn {
                session_id: wire.session_id?,
                turn_id: TurnId::from(wire.execution_id?),
            },
            "drain" => Self::QueueDrain {
                session_id: wire.session_id?,
                drain_id: wire.execution_id?,
            },
            "delete" => Self::SessionDelete {
                session_id: wire.session_id?,
            },
            "process" => Self::Process {
                process_id: ProcessId::from(wire.execution_id?),
            },
            "op" => Self::RuntimeOperation {
                operation_id: wire.execution_id?,
            },
            _ => return None,
        };
        scope.validate().ok()?;
        Some(scope)
    }

    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            Self::Turn { session_id, .. }
            | Self::QueueDrain { session_id, .. }
            | Self::SessionDelete { session_id, .. } => Some(session_id),
            Self::Process { .. } | Self::RuntimeOperation { .. } => None,
        }
    }

    pub fn turn_id(&self) -> Option<&TurnId> {
        match self {
            Self::Turn { turn_id, .. } => Some(turn_id),
            _ => None,
        }
    }

    pub fn validates_turn_trace_id(&self) -> bool {
        matches!(self, Self::Turn { .. })
    }

    pub fn validate(&self) -> Result<(), EffectIdentityError> {
        let missing = match self {
            Self::Turn {
                session_id,
                turn_id,
            } => session_id.trim().is_empty() || turn_id.trim().is_empty(),
            Self::Process { process_id } => process_id.trim().is_empty(),
            Self::QueueDrain {
                session_id,
                drain_id,
            } => session_id.trim().is_empty() || drain_id.trim().is_empty(),
            Self::SessionDelete { session_id } => session_id.trim().is_empty(),
            Self::RuntimeOperation { operation_id } => operation_id.trim().is_empty(),
        };
        if missing {
            return Err(EffectIdentityError::MissingExecutionScopeId);
        }
        Ok(())
    }
}

/// Canonical address of one admitted runtime effect.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EffectAddress {
    pub execution_scope: ExecutionScope,
    pub replay_key: String,
}

impl EffectAddress {
    pub fn new(
        execution_scope: ExecutionScope,
        replay_key: impl Into<String>,
    ) -> Result<Self, EffectIdentityError> {
        let address = Self {
            execution_scope,
            replay_key: replay_key.into(),
        };
        address.validate()?;
        Ok(address)
    }

    pub fn validate(&self) -> Result<(), EffectIdentityError> {
        self.execution_scope.validate()?;
        if self.replay_key.is_empty() {
            return Err(EffectIdentityError::MissingReplayKey);
        }
        Ok(())
    }

    /// Collision-free, human-inspectable graph identity shared by all trace
    /// projections and causal references.
    pub fn graph_key(&self) -> String {
        let scope = self
            .execution_scope
            .journal_identity()
            .expect("validated effect address contains a valid execution scope");
        let replay_key = serde_json::to_string(&self.replay_key)
            .expect("effect replay key is an infallible JSON string");
        format!("effect:{}:{replay_key}", scope.key())
    }
}

/// Durable effect-journal key plus indexed lifecycle join columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectJournalIdentity {
    key: String,
    session_id: Option<SessionId>,
}

/// The exact existing generation of `ExecutionScope` journal keys.
const JOURNAL_IDENTITY_VERSION: u8 = 2;

impl EffectJournalIdentity {
    fn from_scope(scope: &ExecutionScope) -> Self {
        #[derive(Serialize)]
        struct Wire<'a> {
            version: u8,
            kind: &'static str,
            #[serde(skip_serializing_if = "Option::is_none")]
            session_id: Option<&'a SessionId>,
            #[serde(skip_serializing_if = "Option::is_none")]
            execution_id: Option<&'a str>,
        }

        let (kind, session_id, execution_id) = match scope {
            ExecutionScope::Turn {
                session_id,
                turn_id,
            } => ("turn", Some(session_id), Some(turn_id.as_str())),
            ExecutionScope::QueueDrain {
                session_id,
                drain_id,
            } => ("drain", Some(session_id), Some(drain_id.as_str())),
            ExecutionScope::SessionDelete { session_id } => ("delete", Some(session_id), None),
            ExecutionScope::Process { process_id } => ("process", None, Some(process_id.as_str())),
            ExecutionScope::RuntimeOperation { operation_id } => {
                ("op", None, Some(operation_id.as_str()))
            }
        };
        let key = serde_json::to_string(&Wire {
            version: JOURNAL_IDENTITY_VERSION,
            kind,
            session_id,
            execution_id,
        })
        .expect("effect journal identity contains only infallible string fields");
        Self {
            key,
            session_id: session_id.cloned(),
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectIdentityError {
    MissingExecutionScopeId,
    MissingReplayKey,
}

impl fmt::Display for EffectIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MissingExecutionScopeId => "execution scopes require non-empty stable ids",
            Self::MissingReplayKey => "effect addresses require a replay key",
        })
    }
}

impl std::error::Error for EffectIdentityError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_identity_v2_bytes_remain_unchanged_for_all_scope_variants() {
        let fixtures = [
            (
                ExecutionScope::turn("session", "turn"),
                r#"{"version":2,"kind":"turn","session_id":"session","execution_id":"turn"}"#,
            ),
            (
                ExecutionScope::queue_drain("session", "drain"),
                r#"{"version":2,"kind":"drain","session_id":"session","execution_id":"drain"}"#,
            ),
            (
                ExecutionScope::session_delete("session"),
                r#"{"version":2,"kind":"delete","session_id":"session"}"#,
            ),
            (
                ExecutionScope::process("process"),
                r#"{"version":2,"kind":"process","execution_id":"process"}"#,
            ),
            (
                ExecutionScope::runtime_operation("operation"),
                r#"{"version":2,"kind":"op","execution_id":"operation"}"#,
            ),
        ];
        for (scope, expected) in fixtures {
            assert_eq!(scope.journal_identity().unwrap().key(), expected);
            assert_eq!(ExecutionScope::from_journal_key(expected), Some(scope));
        }
    }

    #[test]
    fn same_replay_key_in_distinct_scopes_has_distinct_graph_identity() {
        let turn = EffectAddress::new(ExecutionScope::turn("session", "turn"), "same").unwrap();
        let process = EffectAddress::new(ExecutionScope::process("process"), "same").unwrap();
        assert_ne!(turn, process);
        assert_ne!(turn.graph_key(), process.graph_key());
    }
}
