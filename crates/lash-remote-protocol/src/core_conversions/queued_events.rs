impl From<lash_core::MessageRole> for RemoteMessageRole {
    fn from(value: lash_core::MessageRole) -> Self {
        match value {
            lash_core::MessageRole::User => Self::User,
            lash_core::MessageRole::Assistant => Self::Assistant,
            lash_core::MessageRole::System => Self::System,
            lash_core::MessageRole::Event => Self::Event,
        }
    }
}

impl From<lash_core::PartKind> for RemotePartKind {
    fn from(value: lash_core::PartKind) -> Self {
        match value {
            lash_core::PartKind::Text => Self::Text,
            lash_core::PartKind::Attachment => Self::Attachment,
            lash_core::PartKind::Code => Self::Code,
            lash_core::PartKind::Output => Self::Output,
            lash_core::PartKind::Error => Self::Error,
            lash_core::PartKind::Prose => Self::Prose,
            lash_core::PartKind::ToolCall => Self::ToolCall,
            lash_core::PartKind::ToolResult => Self::ToolResult,
            lash_core::PartKind::Reasoning => Self::Reasoning,
        }
    }
}

impl From<lash_core::runtime::QueuedWorkClaimBoundary> for RemoteQueuedWorkClaimBoundary {
    fn from(value: lash_core::runtime::QueuedWorkClaimBoundary) -> Self {
        match value {
            lash_core::runtime::QueuedWorkClaimBoundary::ActiveTurnCheckpoint => {
                Self::ActiveTurnCheckpoint
            }
            lash_core::runtime::QueuedWorkClaimBoundary::Idle => Self::Idle,
        }
    }
}

impl From<lash_core::TurnOutputSource> for RemoteTurnOutputSource {
    fn from(value: lash_core::TurnOutputSource) -> Self {
        match value {
            lash_core::TurnOutputSource::Runtime => Self::Runtime,
            lash_core::TurnOutputSource::Plugin { plugin_id } => Self::Plugin { plugin_id },
        }
    }
}

impl From<lash_core::MessageOrigin> for RemoteMessageOrigin {
    fn from(value: lash_core::MessageOrigin) -> Self {
        match value {
            lash_core::MessageOrigin::Plugin {
                plugin_id,
                transient,
            } => Self::Plugin {
                plugin_id,
                transient,
            },
            lash_core::MessageOrigin::Process {
                process_id,
                event_type,
                sequence,
                wake_id,
                caused_by,
            } => Self::Process {
                process_id,
                event_type,
                sequence,
                wake_id,
                caused_by: caused_by.map(Into::into),
            },
            lash_core::MessageOrigin::TurnInput { turn_id, input_id } => {
                Self::TurnInput { turn_id, input_id }
            }
            lash_core::MessageOrigin::TurnOutput { turn_id, source } => Self::TurnOutput {
                turn_id,
                source: source.into(),
            },
        }
    }
}

impl From<lash_core::PruneState> for RemotePruneState {
    fn from(value: lash_core::PruneState) -> Self {
        match value {
            lash_core::PruneState::Intact => Self::Intact,
            lash_core::PruneState::Cleared => Self::Cleared,
            lash_core::PruneState::Deleted {
                breadcrumb,
                archive_hash,
            } => Self::Deleted {
                breadcrumb,
                archive_hash,
            },
            lash_core::PruneState::Summarized {
                summary,
                archive_hash,
            } => Self::Summarized {
                summary,
                archive_hash,
            },
        }
    }
}

impl From<lash_core::session_model::message::PartAttachment> for RemotePartAttachment {
    fn from(value: lash_core::session_model::message::PartAttachment) -> Self {
        let lash_core::session_model::message::PartAttachment { source } = value;
        Self {
            source: source.into(),
        }
    }
}

impl From<lash_core::TurnCause> for RemoteTurnCause {
    fn from(value: lash_core::TurnCause) -> Self {
        let lash_core::TurnCause {
            id,
            event_type,
            origin,
            text,
        } = value;
        Self {
            id,
            event_type,
            origin: origin.into(),
            text,
        }
    }
}

impl From<lash_core::PluginMessage> for RemotePluginMessage {
    fn from(value: lash_core::PluginMessage) -> Self {
        let lash_core::PluginMessage {
            id,
            role,
            content,
            origin,
            parts,
            attachments,
        } = value;
        Self {
            id,
            role: role.into(),
            content,
            origin: origin.map(Into::into),
            parts: parts.into_iter().map(Into::into).collect(),
            attachments: attachments.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<lash_core::Part> for RemotePart {
    fn from(value: lash_core::Part) -> Self {
        // Part is non-exhaustive outside its owning crate; project its public fields.
        Self {
            id: value.id,
            kind: value.kind.into(),
            content: value.content,
            attachment: value.attachment.map(Into::into),
            tool_call_id: value.tool_call_id,
            tool_name: value.tool_name,
            tool_replay: value.tool_replay.map(Into::into),
            prune_state: value.prune_state.into(),
            reasoning_meta: value.reasoning_meta.map(Into::into),
            response_meta: value.response_meta.map(Into::into),
        }
    }
}
