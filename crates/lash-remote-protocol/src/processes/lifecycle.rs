use super::*;

/// The wire form of the shared opener vocabulary inside an owned parent.
///
/// Mirrors `lash_core::EffectOpener` arm for arm so a remote peer names the
/// exact durable owner — a turn, a queued-work drain, or one process — and
/// never a rendered id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteEffectOpener {
    Turn {
        session_id: SessionId,
        turn_id: String,
    },
    QueueDrain {
        session_id: SessionId,
        drain_id: String,
    },
    Process {
        process_id: ProcessId,
    },
}

/// The wire form of a scope a process may live until: an opener or a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "scope", rename_all = "snake_case")]
pub enum RemoteScopeId {
    Opener(RemoteEffectOpener),
    Session(SessionId),
}

/// How a recorded lifetime's scope was granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteScopeGrant {
    Ancestor,
    HostSessionLookup,
}

/// The lifetime a remote start asks for. A remote start is a root: it has no
/// starter, so it is `detached` or lives until a session. This is data only:
/// the server boundary looks the session up and grants it (FIG-3607 R3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "lifetime", rename_all = "snake_case")]
pub enum RemoteStartLifetime {
    Detached,
    UntilSession { session_id: SessionId },
}

/// A process's recorded lifetime decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "lifetime", rename_all = "snake_case")]
pub enum RemoteLifetimeDecision {
    Detached,
    Until {
        scope: RemoteScopeId,
        grant: RemoteScopeGrant,
    },
}

impl RemoteScopeId {
    pub fn validate(
        &self,
        type_name: &'static str,
        field: &'static str,
    ) -> Result<(), RemoteProtocolError> {
        match self {
            Self::Opener(RemoteEffectOpener::Turn {
                session_id,
                turn_id,
            }) => {
                require_non_empty(type_name, field, session_id)?;
                require_non_empty(type_name, field, turn_id)
            }
            Self::Opener(RemoteEffectOpener::QueueDrain {
                session_id,
                drain_id,
            }) => {
                require_non_empty(type_name, field, session_id)?;
                require_non_empty(type_name, field, drain_id)
            }
            Self::Opener(RemoteEffectOpener::Process { process_id }) => {
                require_non_empty(type_name, field, process_id)
            }
            Self::Session(session_id) => require_non_empty(type_name, field, session_id),
        }
    }
}

impl RemoteStartLifetime {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        match self {
            Self::Detached => Ok(()),
            Self::UntilSession { session_id } => {
                require_non_empty(type_name, "lifetime.session_id", session_id)
            }
        }
    }
}

impl RemoteLifetimeDecision {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        match self {
            Self::Detached => Ok(()),
            Self::Until { scope, .. } => scope.validate(type_name, "lifetime.scope"),
        }
    }
}
