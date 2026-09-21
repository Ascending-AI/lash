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
            lash_core::EffectOpener::Process { process_ref } => Self::Process {
                process_id: process_ref.process_id,
                incarnation: process_ref.incarnation.registration_sequence(),
            },
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
            RemoteEffectOpener::Process {
                process_id,
                incarnation,
            } => Self::process(lash_core::ProcessRef::new(
                process_id,
                lash_core::ProcessIncarnation::from_registration_sequence(incarnation),
            )),
        }
    }
}

impl From<lash_core::ParentScope> for RemoteParentScope {
    fn from(value: lash_core::ParentScope) -> Self {
        match value {
            lash_core::ParentScope::Owned(opener) => Self::Owned(opener.into()),
            lash_core::ParentScope::Host => Self::Host,
        }
    }
}

impl From<RemoteParentScope> for lash_core::ParentScope {
    fn from(value: RemoteParentScope) -> Self {
        match value {
            RemoteParentScope::Owned(opener) => Self::Owned(opener.into()),
            RemoteParentScope::Host => Self::Host,
        }
    }
}

impl From<lash_core::ProcessLifecyclePolicy> for RemoteProcessLifecyclePolicy {
    fn from(value: lash_core::ProcessLifecyclePolicy) -> Self {
        Self {
            parent: value.parent.into(),
            on_parent_end: match value.on_parent_end {
                lash_core::OnParentEnd::Abandon => RemoteOnParentEnd::Abandon,
                lash_core::OnParentEnd::Cancel => RemoteOnParentEnd::Cancel,
            },
        }
    }
}

impl TryFrom<RemoteProcessLifecyclePolicy> for lash_core::ProcessLifecyclePolicy {
    type Error = RemoteProtocolError;
    fn try_from(value: RemoteProcessLifecyclePolicy) -> Result<Self, Self::Error> {
        Ok(Self {
            parent: value.parent.into(),
            on_parent_end: match value.on_parent_end {
                RemoteOnParentEnd::Abandon => lash_core::OnParentEnd::Abandon,
                RemoteOnParentEnd::Cancel => lash_core::OnParentEnd::Cancel,
            },
        })
    }
}
