use super::*;

impl From<lash_core::CausalRef> for RemoteCausalRef {
    fn from(value: lash_core::CausalRef) -> Self {
        match value {
            lash_core::CausalRef::Turn {
                session_id,
                turn_id,
            } => Self::Turn {
                session_id,
                turn_id,
            },
            lash_core::CausalRef::Effect { address } => Self::Effect { address },
            lash_core::CausalRef::ToolCall {
                session_id,
                call_id,
            } => Self::ToolCall {
                session_id,
                call_id,
            },
            lash_core::CausalRef::Process { process_id } => Self::Process { process_id },
            lash_core::CausalRef::ProcessEvent {
                process_id,
                sequence,
            } => Self::ProcessEvent {
                process_id,
                sequence,
            },
            lash_core::CausalRef::TriggerOccurrence {
                occurrence_id,
                subscription_id,
                subscription_incarnation,
                subscription_revision,
            } => Self::TriggerOccurrence {
                occurrence_id,
                subscription_id,
                subscription_incarnation,
                subscription_revision,
            },
            lash_core::CausalRef::SessionNode {
                session_id,
                node_id,
            } => Self::SessionNode {
                session_id,
                node_id,
            },
        }
    }
}

impl From<RemoteCausalRef> for lash_core::CausalRef {
    fn from(value: RemoteCausalRef) -> Self {
        match value {
            RemoteCausalRef::Turn {
                session_id,
                turn_id,
            } => Self::Turn {
                session_id,
                turn_id,
            },
            RemoteCausalRef::Effect { address } => Self::Effect { address },
            RemoteCausalRef::ToolCall {
                session_id,
                call_id,
            } => Self::ToolCall {
                session_id,
                call_id,
            },
            RemoteCausalRef::Process { process_id } => Self::Process { process_id },
            RemoteCausalRef::ProcessEvent {
                process_id,
                sequence,
            } => Self::ProcessEvent {
                process_id,
                sequence,
            },
            RemoteCausalRef::TriggerOccurrence {
                occurrence_id,
                subscription_id,
                subscription_incarnation,
                subscription_revision,
            } => Self::TriggerOccurrence {
                occurrence_id,
                subscription_id,
                subscription_incarnation,
                subscription_revision,
            },
            RemoteCausalRef::SessionNode {
                session_id,
                node_id,
            } => Self::SessionNode {
                session_id,
                node_id,
            },
        }
    }
}

impl TryFrom<RemoteTurnInput> for lash_core::TurnInput {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteTurnInput) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteTurnInput {
            items,
            trace_turn_id,
            ..
        } = value;
        let mut input = lash_core::TurnInput::items(
            items
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
        );
        input.trace_turn_id = trace_turn_id;
        Ok(input)
    }
}

impl TryFrom<RemoteTurnRequest> for lash_core::TurnInput {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteTurnRequest) -> Result<Self, Self::Error> {
        value.validate()?;
        // Identity/routing fields are consumed by the transport layer, not the
        // core turn input; tool grants are applied separately. `turn_id` is
        // the send's id, and the protocol turn options its run spec, which the
        // transport passes beside the input ([`RemoteTurnRequest::run_spec`]).
        let RemoteTurnRequest {
            session_id: _,
            turn_id: _,
            input,
            protocol_turn_options: _,
            tool_grants: _,
            metadata: _,
        } = value;
        input.try_into()
    }
}

impl RemoteTurnRequest {
    /// The run spec this request's input is sent under: its protocol turn
    /// options as one-shot overrides, or the default spec.
    pub fn run_spec(&self) -> lash_core::RunSpec {
        lash_core::RunSpec::overrides(lash_core::RunOverrides {
            protocol_turn_options: self.protocol_turn_options.clone().map(Into::into),
            ..lash_core::RunOverrides::default()
        })
    }
}

impl TryFrom<lash_core::TurnInput> for RemoteTurnInput {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::TurnInput) -> Result<Self, Self::Error> {
        let lash_core::TurnInput {
            items,
            trace_turn_id,
            turn_context: _,
        } = value;
        Ok(Self {
            items: items
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            trace_turn_id,
            #[cfg(feature = "synthetic-next")]
            synthetic_next_note: None,
        })
    }
}

impl TryFrom<RemoteInputItem> for lash_core::InputItem {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteInputItem) -> Result<Self, Self::Error> {
        match value {
            RemoteInputItem::Text { text } => Ok(Self::Text { text }),
            RemoteInputItem::Attachment { source } => Ok(Self::Attachment {
                source: source.try_into()?,
            }),
        }
    }
}

impl TryFrom<lash_core::InputItem> for RemoteInputItem {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::InputItem) -> Result<Self, Self::Error> {
        match value {
            lash_core::InputItem::Text { text } => Ok(Self::Text { text }),
            lash_core::InputItem::Attachment { source } => Ok(Self::Attachment {
                source: source.into(),
            }),
        }
    }
}
