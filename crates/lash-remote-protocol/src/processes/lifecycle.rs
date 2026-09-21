use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteOnParentEnd {
    Abandon,
    Cancel,
}

/// The wire form of the shared opener vocabulary inside an owned parent.
///
/// Mirrors `lash_core::EffectOpener` arm for arm so a remote peer names the
/// exact durable owner — a turn, a queued-work drain, or one process
/// incarnation — and never a rendered id.
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
        incarnation: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "opener", rename_all = "snake_case")]
pub enum RemoteParentScope {
    Owned(RemoteEffectOpener),
    Host,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessLifecyclePolicy {
    pub parent: RemoteParentScope,
    pub on_parent_end: RemoteOnParentEnd,
}

impl RemoteProcessLifecyclePolicy {
    pub fn validate(
        &self,
        type_name: &'static str,
        originator: &RemoteProcessOriginator,
    ) -> Result<(), RemoteProtocolError> {
        match &self.parent {
            RemoteParentScope::Host if self.on_parent_end == RemoteOnParentEnd::Cancel => {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name,
                    message: "Host parent cannot declare Cancel on parent end".to_string(),
                });
            }
            RemoteParentScope::Owned(RemoteEffectOpener::Turn {
                session_id,
                turn_id,
            }) => {
                require_non_empty(type_name, "parent.opener.session_id", session_id)?;
                require_non_empty(type_name, "parent.opener.turn_id", turn_id)?;
                require_originating_session(type_name, session_id, originator)?;
            }
            RemoteParentScope::Owned(RemoteEffectOpener::QueueDrain {
                session_id,
                drain_id,
            }) => {
                require_non_empty(type_name, "parent.opener.session_id", session_id)?;
                require_non_empty(type_name, "parent.opener.drain_id", drain_id)?;
                require_originating_session(type_name, session_id, originator)?;
            }
            RemoteParentScope::Owned(RemoteEffectOpener::Process {
                process_id,
                incarnation,
            }) => RemoteProcessRef {
                process_id: process_id.clone(),
                incarnation: *incarnation,
            }
            .validate(type_name)?,
            RemoteParentScope::Host => {}
        }
        Ok(())
    }
}

fn require_originating_session(
    type_name: &'static str,
    session_id: &SessionId,
    originator: &RemoteProcessOriginator,
) -> Result<(), RemoteProtocolError> {
    if !matches!(originator, RemoteProcessOriginator::Session { session_id: originating_session, .. } if originating_session == session_id)
    {
        return Err(RemoteProtocolError::InvalidEnvelope {
            type_name,
            message: "turn or drain parent must belong to the originating session".to_string(),
        });
    }
    Ok(())
}
