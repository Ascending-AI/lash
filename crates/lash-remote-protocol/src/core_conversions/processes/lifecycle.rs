use super::*;

impl From<lash_core::ProcessLifecyclePolicy> for RemoteProcessLifecyclePolicy {
    fn from(value: lash_core::ProcessLifecyclePolicy) -> Self {
        Self {
            parent: match value.parent {
                lash_core::ParentScope::Host => RemoteParentScope::Host,
                lash_core::ParentScope::Turn {
                    session_id,
                    turn_id,
                } => RemoteParentScope::Turn {
                    session_id,
                    turn_id: turn_id.to_string(),
                },
                lash_core::ParentScope::Process {
                    process_id,
                    incarnation,
                } => RemoteParentScope::Process {
                    process_id,
                    incarnation: incarnation.registration_sequence(),
                },
            },
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
            parent: match value.parent {
                RemoteParentScope::Host => lash_core::ParentScope::Host,
                RemoteParentScope::Turn {
                    session_id,
                    turn_id,
                } => lash_core::ParentScope::Turn {
                    session_id,
                    turn_id: lash_core::TurnId::from(turn_id),
                },
                RemoteParentScope::Process {
                    process_id,
                    incarnation,
                } => lash_core::ParentScope::Process {
                    process_id,
                    incarnation: lash_core::ProcessIncarnation::from_registration_sequence(
                        incarnation,
                    ),
                },
            },
            on_parent_end: match value.on_parent_end {
                RemoteOnParentEnd::Abandon => lash_core::OnParentEnd::Abandon,
                RemoteOnParentEnd::Cancel => lash_core::OnParentEnd::Cancel,
            },
        })
    }
}
