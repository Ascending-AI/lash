use crate::{ProcessId, RuntimeError, RuntimeErrorCode, SessionId, TurnId};
use serde::{Deserialize, Serialize};

/// Stable semantic identity for one effectful runtime operation.
///
/// The scope is chosen by the host boundary before any nondeterministic work is
/// planned. It is intentionally generic: Restate, an native test host, or a
/// future durable effect host all receive the same Lash scope vocabulary.
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
    /// Constructs the stable session-and-turn scope effect-host implementors use to key one turn's
    /// durable effects.
    pub fn turn(session_id: impl Into<SessionId>, turn_id: impl Into<TurnId>) -> Self {
        Self::Turn {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
        }
    }

    /// Constructs the stable process scope effect-host implementors use to key effects that outlive
    /// any one session turn.
    pub fn process(process_id: impl Into<ProcessId>) -> Self {
        Self::Process {
            process_id: process_id.into(),
        }
    }

    /// Constructs the stable session-and-drain scope effect-host implementors use to key
    /// queued-work effects outside a turn.
    pub fn queue_drain(session_id: impl Into<SessionId>, drain_id: impl Into<String>) -> Self {
        Self::QueueDrain {
            session_id: session_id.into(),
            drain_id: drain_id.into(),
        }
    }

    /// Constructs the stable session-delete scope effect-host implementors use to journal deletion
    /// work outside a turn.
    pub fn session_delete(session_id: impl Into<SessionId>) -> Self {
        Self::SessionDelete {
            session_id: session_id.into(),
        }
    }

    /// Constructs a runtime-operation scope effect-host implementors can journal when no session or
    /// process owns the work.
    pub fn runtime_operation(operation_id: impl Into<String>) -> Self {
        Self::RuntimeOperation {
            operation_id: operation_id.into(),
        }
    }

    /// Exposes id to store and durable-substrate implementors and effect-host implementors while
    /// snapshotting or restoring durable session state.
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
    pub fn journal_identity(&self) -> Result<EffectJournalIdentity, RuntimeError> {
        self.validate()?;
        Ok(EffectJournalIdentity::from_scope(self))
    }

    /// The scope a persisted `scope_id` names, or `None` when no version of this
    /// runtime wrote that key.
    ///
    /// The inverse of [`journal_identity`](Self::journal_identity), and
    /// deliberately beside it: a durable effect row records its scope as the
    /// journal key alone, so a reader that did not open the effect — the group
    /// drain, which takes its queue from the journal rather than from a caller —
    /// has the key and needs the scope. Re-deriving it here rather than at that
    /// reader keeps one mapping in both directions instead of two that can
    /// drift.
    ///
    /// `None` rather than a guessed scope, for the same reason
    /// [`EffectGroupColumn::from_column`](super::super::group_journal::EffectGroupColumn::from_column)
    /// answers `None`: a key this build cannot read is a row it must refuse, not
    /// one it may re-execute under an invented identity.
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
        // A key that decodes to a scope this runtime would refuse to journal is
        // not a scope: the round trip has to land on a value the forward
        // direction could have produced.
        scope.validate().ok()?;
        Some(scope)
    }

    /// Exposes session id to store and durable-substrate implementors and effect-host implementors
    /// while snapshotting or restoring durable session state. Returns `None` when no session id is
    /// present.
    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            Self::Turn { session_id, .. }
            | Self::QueueDrain { session_id, .. }
            | Self::SessionDelete { session_id, .. } => Some(session_id),
            Self::Process { .. } | Self::RuntimeOperation { .. } => None,
        }
    }

    /// Exposes turn id to store and durable-substrate implementors and effect-host implementors
    /// while snapshotting or restoring durable session state. Returns `None` when no turn id is
    /// present.
    pub fn turn_id(&self) -> Option<&TurnId> {
        match self {
            Self::Turn { turn_id, .. } => Some(turn_id),
            _ => None,
        }
    }

    /// Reports whether effect-host implementors may validate a trace turn ID against this scope;
    /// only a turn scope carries that identity.
    pub fn validates_turn_trace_id(&self) -> bool {
        matches!(self, Self::Turn { .. })
    }

    /// Rejects empty stable identifiers before store or effect-host implementors persist a scope;
    /// turn and queue-drain scopes require both component IDs.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        let missing = match self {
            Self::Turn {
                session_id,
                turn_id,
                ..
            } => session_id.trim().is_empty() || turn_id.trim().is_empty(),
            Self::Process { process_id } => process_id.trim().is_empty(),
            Self::QueueDrain {
                session_id,
                drain_id,
                ..
            } => session_id.trim().is_empty() || drain_id.trim().is_empty(),
            Self::SessionDelete { session_id, .. } => session_id.trim().is_empty(),
            Self::RuntimeOperation { operation_id } => operation_id.trim().is_empty(),
        };
        if missing {
            return Err(RuntimeError::new(
                RuntimeErrorCode::MissingExecutionScopeId,
                "execution scopes require non-empty stable ids",
            ));
        }
        Ok(())
    }
}

/// Durable effect-journal key plus indexed lifecycle join columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectJournalIdentity {
    key: String,
    session_id: Option<SessionId>,
}

/// The `version` field every journal key this build writes carries, and the
/// only one [`ExecutionScope::from_journal_key`] reads back.
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
