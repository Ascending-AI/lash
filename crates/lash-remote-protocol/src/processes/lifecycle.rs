use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteOnParentEnd {
    Abandon,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteParentScope {
    Turn {
        session_id: SessionId,
        turn_id: String,
    },
    Process {
        process_id: ProcessId,
        incarnation: u64,
    },
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
            RemoteParentScope::Turn {
                session_id,
                turn_id,
            } => {
                require_non_empty(type_name, "parent.session_id", session_id)?;
                require_non_empty(type_name, "parent.turn_id", turn_id)?;
                if !matches!(originator, RemoteProcessOriginator::Session { session_id: originating_session, .. } if originating_session == session_id)
                {
                    return Err(RemoteProtocolError::InvalidEnvelope {
                        type_name,
                        message: "turn parent must belong to the originating session".to_string(),
                    });
                }
            }
            RemoteParentScope::Process {
                process_id,
                incarnation,
            } => RemoteProcessRef {
                process_id: process_id.clone(),
                incarnation: *incarnation,
            }
            .validate(type_name)?,
            RemoteParentScope::Host => {}
        }
        Ok(())
    }
}
