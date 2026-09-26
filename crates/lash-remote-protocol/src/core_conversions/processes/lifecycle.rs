use super::*;

impl From<lash_core::EffectOpener> for RemoteEffectOpener {
    fn from(value: lash_core::EffectOpener) -> Self {
        match value {
            lash_core::EffectOpener::Turn {
                session_id,
                turn_id,
            } => Self::Turn {
                session_id,
                turn_id: turn_id.to_string(),
            },
            lash_core::EffectOpener::QueueDrain {
                session_id,
                drain_id,
            } => Self::QueueDrain {
                session_id,
                drain_id,
            },
            lash_core::EffectOpener::Process { process_id } => Self::Process { process_id },
        }
    }
}

impl From<RemoteEffectOpener> for lash_core::EffectOpener {
    fn from(value: RemoteEffectOpener) -> Self {
        match value {
            RemoteEffectOpener::Turn {
                session_id,
                turn_id,
            } => Self::turn(session_id, turn_id),
            RemoteEffectOpener::QueueDrain {
                session_id,
                drain_id,
            } => Self::queue_drain(session_id, drain_id),
            RemoteEffectOpener::Process { process_id } => Self::process(process_id),
        }
    }
}

impl From<lash_core::ScopeId> for RemoteScopeId {
    fn from(value: lash_core::ScopeId) -> Self {
        match value {
            lash_core::ScopeId::Opener(opener) => Self::Opener(opener.into()),
            lash_core::ScopeId::Session(session_id) => Self::Session(session_id),
        }
    }
}

impl From<RemoteScopeId> for lash_core::ScopeId {
    fn from(value: RemoteScopeId) -> Self {
        match value {
            RemoteScopeId::Opener(opener) => Self::Opener(opener.into()),
            RemoteScopeId::Session(session_id) => Self::Session(session_id),
        }
    }
}

impl From<lash_core::ScopeGrant> for RemoteScopeGrant {
    fn from(value: lash_core::ScopeGrant) -> Self {
        match value {
            lash_core::ScopeGrant::Ancestor => Self::Ancestor,
            lash_core::ScopeGrant::HostSessionLookup => Self::HostSessionLookup,
        }
    }
}

impl From<RemoteScopeGrant> for lash_core::ScopeGrant {
    fn from(value: RemoteScopeGrant) -> Self {
        match value {
            RemoteScopeGrant::Ancestor => Self::Ancestor,
            RemoteScopeGrant::HostSessionLookup => Self::HostSessionLookup,
        }
    }
}

impl From<lash_core::LifetimeDecision> for RemoteLifetimeDecision {
    fn from(value: lash_core::LifetimeDecision) -> Self {
        match value {
            lash_core::LifetimeDecision::Detached => Self::Detached,
            lash_core::LifetimeDecision::Until { scope, grant } => Self::Until {
                scope: scope.into(),
                grant: grant.into(),
            },
        }
    }
}

impl From<RemoteLifetimeDecision> for lash_core::LifetimeDecision {
    fn from(value: RemoteLifetimeDecision) -> Self {
        match value {
            RemoteLifetimeDecision::Detached => Self::Detached,
            RemoteLifetimeDecision::Until { scope, grant } => Self::Until {
                scope: scope.into(),
                grant: grant.into(),
            },
        }
    }
}

/// A remote start's lifetime is data: `until_session` becomes the host
/// session-lookup grant here, and the server boundary that realizes the start
/// looks the session up before anything registers (FIG-3607 R3).
impl From<RemoteStartLifetime> for lash_core::LifetimeDecision {
    fn from(value: RemoteStartLifetime) -> Self {
        match value {
            RemoteStartLifetime::Detached => Self::Detached,
            RemoteStartLifetime::UntilSession { session_id } => Self::Until {
                scope: lash_core::ScopeId::Session(session_id),
                grant: lash_core::ScopeGrant::HostSessionLookup,
            },
        }
    }
}

/// Only a root's lifetime crosses as a start: a runtime start's ancestor
/// grant cannot be requested by a remote peer.
impl TryFrom<lash_core::LifetimeDecision> for RemoteStartLifetime {
    type Error = RemoteProtocolError;
    fn try_from(value: lash_core::LifetimeDecision) -> Result<Self, Self::Error> {
        match value {
            lash_core::LifetimeDecision::Detached => Ok(Self::Detached),
            lash_core::LifetimeDecision::Until {
                scope: lash_core::ScopeId::Session(session_id),
                grant: lash_core::ScopeGrant::HostSessionLookup,
            } => Ok(Self::UntilSession { session_id }),
            lash_core::LifetimeDecision::Until { scope, .. } => {
                Err(RemoteProtocolError::InvalidEnvelope {
                    type_name: "RemoteProcessStartRequest",
                    message: format!(
                        "a start's lifetime `until {scope}` is a runtime grant and cannot cross the wire"
                    ),
                })
            }
        }
    }
}
