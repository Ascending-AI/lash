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
            lash_core::CausalRef::Effect {
                session_id,
                turn_id,
                effect_id,
            } => Self::Effect {
                session_id,
                turn_id,
                effect_id,
            },
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
            RemoteCausalRef::Effect {
                session_id,
                turn_id,
                effect_id,
            } => Self::Effect {
                session_id,
                turn_id,
                effect_id,
            },
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
            protocol_turn_options,
            trace_turn_id,
            prompt_layer,
        } = value;
        let mut input = lash_core::TurnInput::items(
            items
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
        );
        input.protocol_turn_options = protocol_turn_options.map(Into::into);
        input.trace_turn_id = trace_turn_id;
        if let Some(prompt_layer) = prompt_layer {
            input.turn_context.set_prompt_layer(prompt_layer.into());
        }
        Ok(input)
    }
}

impl TryFrom<RemoteTurnRequest> for lash_core::TurnInput {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteTurnRequest) -> Result<Self, Self::Error> {
        value.validate()?;
        // Identity/routing fields are consumed by the transport layer, not the
        // core turn input; tool grants are applied separately.
        let RemoteTurnRequest {
            session_id: _,
            turn_id: _,
            idempotency_key: _,
            input,
            tool_grants: _,
            metadata: _,
        } = value;
        input.try_into()
    }
}

impl TryFrom<lash_core::TurnInput> for RemoteTurnInput {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::TurnInput) -> Result<Self, Self::Error> {
        // `turn_context` has private internals and is inspected through
        // accessors below; new TurnContext fields are not guarded here.
        let lash_core::TurnInput {
            items,
            protocol_turn_options,
            trace_turn_id,
            protocol_extension,
            turn_context,
        } = value;
        if protocol_extension.is_some() {
            return Err(RemoteProtocolError::NonRemoteSafeTurnInput(
                "live protocol turn extensions cannot cross a remote boundary".to_string(),
            ));
        }
        if turn_context.has_live_plugin_inputs() {
            return Err(RemoteProtocolError::NonRemoteSafeTurnInput(format!(
                "live plugin turn inputs cannot cross a remote boundary: {:?}",
                turn_context.live_plugin_input_ids()
            )));
        }
        if turn_context.provider().is_some() {
            return Err(RemoteProtocolError::NonRemoteSafeTurnInput(
                "per-turn provider overrides cannot cross a remote boundary".to_string(),
            ));
        }
        let prompt_layer = (!turn_context.prompt_layer().is_empty())
            .then(|| RemotePromptLayer::from(turn_context.prompt_layer().clone()));
        Ok(Self {
            items: items
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            protocol_turn_options: protocol_turn_options.map(Into::into),
            trace_turn_id,
            prompt_layer,
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
