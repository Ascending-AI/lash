use super::*;

impl TryFrom<lash_core::facade_support::ProcessTerminalSemantics>
    for RemoteProcessTerminalSemantics
{
    type Error = RemoteProtocolError;

    fn try_from(
        value: lash_core::facade_support::ProcessTerminalSemantics,
    ) -> Result<Self, Self::Error> {
        let lash_core::facade_support::ProcessTerminalSemantics { status, outcome } = value;
        Ok(Self {
            status: status.into(),
            outcome: outcome.try_into()?,
        })
    }
}

impl TryFrom<RemoteProcessTerminalSemantics>
    for lash_core::facade_support::ProcessTerminalSemantics
{
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessTerminalSemantics) -> Result<Self, Self::Error> {
        let RemoteProcessTerminalSemantics { status, outcome } = value;
        Ok(Self {
            status: status.into(),
            outcome: outcome.try_into()?,
        })
    }
}

impl From<lash_core::facade_support::ProcessWake> for RemoteProcessWake {
    fn from(value: lash_core::facade_support::ProcessWake) -> Self {
        let lash_core::facade_support::ProcessWake { input } = value;
        Self { input }
    }
}

impl From<RemoteProcessWake> for lash_core::facade_support::ProcessWake {
    fn from(value: RemoteProcessWake) -> Self {
        let RemoteProcessWake { input } = value;
        Self { input }
    }
}

impl From<lash_core::ProcessValueSelector> for RemoteProcessValueSelector {
    fn from(value: lash_core::ProcessValueSelector) -> Self {
        match value {
            lash_core::ProcessValueSelector::Payload => Self::Payload,
            lash_core::ProcessValueSelector::Pointer(value) => Self::Pointer(value),
            lash_core::ProcessValueSelector::Const(value) => Self::Const(value),
            lash_core::ProcessValueSelector::Template { template, fields } => Self::Template {
                template,
                fields: fields
                    .into_iter()
                    .map(|(name, selector)| (name, selector.into()))
                    .collect(),
            },
            lash_core::ProcessValueSelector::Present(value) => Self::Present(value),
        }
    }
}

impl From<RemoteProcessValueSelector> for lash_core::ProcessValueSelector {
    fn from(value: RemoteProcessValueSelector) -> Self {
        match value {
            RemoteProcessValueSelector::Payload => Self::Payload,
            RemoteProcessValueSelector::Pointer(value) => Self::Pointer(value),
            RemoteProcessValueSelector::Const(value) => Self::Const(value),
            RemoteProcessValueSelector::Template { template, fields } => Self::Template {
                template,
                fields: fields
                    .into_iter()
                    .map(|(name, selector)| (name, selector.into()))
                    .collect(),
            },
            RemoteProcessValueSelector::Present(value) => Self::Present(value),
        }
    }
}

impl From<lash_core::RuntimeInvocation> for RemoteRuntimeInvocation {
    fn from(value: lash_core::RuntimeInvocation) -> Self {
        let lash_core::RuntimeInvocation {
            attribution,
            subject,
            caused_by,
            replay,
        } = value;
        Self {
            attribution: attribution.into(),
            subject: subject.into(),
            caused_by: caused_by.map(Into::into),
            replay: replay.map(Into::into),
        }
    }
}

impl From<RemoteRuntimeInvocation> for lash_core::RuntimeInvocation {
    fn from(value: RemoteRuntimeInvocation) -> Self {
        let RemoteRuntimeInvocation {
            attribution,
            subject,
            caused_by,
            replay,
        } = value;
        Self {
            attribution: attribution.into(),
            subject: subject.into(),
            caused_by: caused_by.map(Into::into),
            replay: replay.map(Into::into),
        }
    }
}

impl From<lash_core::runtime::RuntimeAttribution> for RemoteRuntimeAttribution {
    fn from(value: lash_core::runtime::RuntimeAttribution) -> Self {
        let lash_core::runtime::RuntimeAttribution {
            session_id,
            turn_id,
            turn_index,
            protocol_iteration,
        } = value;
        Self {
            session_id,
            turn_id,
            turn_index,
            protocol_iteration,
        }
    }
}

impl From<RemoteRuntimeAttribution> for lash_core::runtime::RuntimeAttribution {
    fn from(value: RemoteRuntimeAttribution) -> Self {
        let RemoteRuntimeAttribution {
            session_id,
            turn_id,
            turn_index,
            protocol_iteration,
        } = value;
        Self {
            session_id,
            turn_id,
            turn_index,
            protocol_iteration,
        }
    }
}

impl From<lash_core::runtime::RuntimeReplay> for RemoteRuntimeReplay {
    fn from(value: lash_core::runtime::RuntimeReplay) -> Self {
        let lash_core::runtime::RuntimeReplay { key, attribution } = value;
        Self {
            key,
            attribution: attribution.map(|attribution| match attribution {
                lash_core::RuntimeReplayAttribution::ToolIntent(identity) => {
                    RemoteRuntimeReplayAttribution::ToolIntent(identity.into())
                }
            }),
        }
    }
}

impl From<RemoteRuntimeReplay> for lash_core::runtime::RuntimeReplay {
    fn from(value: RemoteRuntimeReplay) -> Self {
        let RemoteRuntimeReplay { key, attribution } = value;
        Self {
            key,
            attribution: attribution.map(|attribution| match attribution {
                RemoteRuntimeReplayAttribution::ToolIntent(identity) => {
                    lash_core::RuntimeReplayAttribution::ToolIntent(lash_core::ToolIntentIdentity {
                        session_id: identity.session_id,
                        execution_scope_id: identity.execution_scope_id,
                        tool_call_id: identity.tool_call_id,
                        intent_index: identity.intent_index,
                        replay_key: identity.replay_key,
                        minting_emission_replay_key: identity.minting_emission_replay_key,
                    })
                }
            }),
        }
    }
}

impl From<lash_core::runtime::RuntimeSubject> for RemoteRuntimeSubject {
    fn from(value: lash_core::runtime::RuntimeSubject) -> Self {
        match value {
            lash_core::runtime::RuntimeSubject::Effect {
                address,
                effect_id,
                replay_attribution,
            } => Self::Effect {
                address,
                effect_id,
                replay_attribution: replay_attribution.map(|attribution| match attribution {
                    lash_core::RuntimeReplayAttribution::ToolIntent(identity) => {
                        RemoteRuntimeReplayAttribution::ToolIntent(identity.into())
                    }
                }),
            },
            lash_core::runtime::RuntimeSubject::Process { process_id } => {
                Self::Process { process_id }
            }
            lash_core::runtime::RuntimeSubject::ProcessEvent {
                process_id,
                sequence,
                event_type,
            } => Self::ProcessEvent {
                process_id,
                sequence,
                event_type,
            },
            lash_core::runtime::RuntimeSubject::TriggerOccurrence {
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
            lash_core::runtime::RuntimeSubject::SessionNode { node_id } => {
                Self::SessionNode { node_id }
            }
        }
    }
}

impl From<RemoteRuntimeSubject> for lash_core::runtime::RuntimeSubject {
    fn from(value: RemoteRuntimeSubject) -> Self {
        match value {
            RemoteRuntimeSubject::Effect {
                address,
                effect_id,
                replay_attribution,
            } => Self::Effect {
                address,
                effect_id,
                replay_attribution: replay_attribution.map(|attribution| match attribution {
                    RemoteRuntimeReplayAttribution::ToolIntent(identity) => {
                        lash_core::RuntimeReplayAttribution::ToolIntent(
                            lash_core::ToolIntentIdentity {
                                session_id: identity.session_id,
                                execution_scope_id: identity.execution_scope_id,
                                tool_call_id: identity.tool_call_id,
                                intent_index: identity.intent_index,
                                replay_key: identity.replay_key,
                                minting_emission_replay_key: identity.minting_emission_replay_key,
                            },
                        )
                    }
                }),
            },
            RemoteRuntimeSubject::Process { process_id } => Self::Process { process_id },
            RemoteRuntimeSubject::ProcessEvent {
                process_id,
                sequence,
                event_type,
            } => Self::ProcessEvent {
                process_id,
                sequence,
                event_type,
            },
            RemoteRuntimeSubject::TriggerOccurrence {
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
            RemoteRuntimeSubject::SessionNode { node_id } => Self::SessionNode { node_id },
        }
    }
}

impl From<lash_core::RuntimeEffectKind> for RemoteRuntimeEffectKind {
    fn from(value: lash_core::RuntimeEffectKind) -> Self {
        match value {
            lash_core::RuntimeEffectKind::LlmCall => Self::LlmCall,
            lash_core::RuntimeEffectKind::AssistantResponseHooks => Self::AssistantResponseHooks,
            lash_core::RuntimeEffectKind::Direct => Self::Direct,
            lash_core::RuntimeEffectKind::ToolAttempt => Self::ToolAttempt,
            lash_core::RuntimeEffectKind::ToolBatch => Self::ToolBatch,
            lash_core::RuntimeEffectKind::ToolParentEnd => Self::ToolParentEnd,
            lash_core::RuntimeEffectKind::Process => Self::Process,
            lash_core::RuntimeEffectKind::Trigger => Self::Trigger,
            lash_core::RuntimeEffectKind::ExecCode => Self::ExecCode,
            lash_core::RuntimeEffectKind::AcceptTurnInput => Self::AcceptTurnInput,
            lash_core::RuntimeEffectKind::Checkpoint => Self::Checkpoint,
            lash_core::RuntimeEffectKind::SyncExecutionEnvironment => {
                Self::SyncExecutionEnvironment
            }
            lash_core::RuntimeEffectKind::Sleep => Self::Sleep,
            lash_core::RuntimeEffectKind::AwaitEvent => Self::AwaitEvent,
            lash_core::RuntimeEffectKind::PeekAwaitEvent => Self::PeekAwaitEvent,
            lash_core::RuntimeEffectKind::LanguageRuntimeValue => Self::LanguageRuntimeValue,
        }
    }
}

impl From<RemoteRuntimeEffectKind> for lash_core::RuntimeEffectKind {
    fn from(value: RemoteRuntimeEffectKind) -> Self {
        match value {
            RemoteRuntimeEffectKind::LlmCall => Self::LlmCall,
            RemoteRuntimeEffectKind::AssistantResponseHooks => Self::AssistantResponseHooks,
            RemoteRuntimeEffectKind::Direct => Self::Direct,
            RemoteRuntimeEffectKind::ToolAttempt => Self::ToolAttempt,
            RemoteRuntimeEffectKind::ToolBatch => Self::ToolBatch,
            RemoteRuntimeEffectKind::ToolParentEnd => Self::ToolParentEnd,
            RemoteRuntimeEffectKind::Process => Self::Process,
            RemoteRuntimeEffectKind::Trigger => Self::Trigger,
            RemoteRuntimeEffectKind::ExecCode => Self::ExecCode,
            RemoteRuntimeEffectKind::AcceptTurnInput => Self::AcceptTurnInput,
            RemoteRuntimeEffectKind::Checkpoint => Self::Checkpoint,
            RemoteRuntimeEffectKind::SyncExecutionEnvironment => Self::SyncExecutionEnvironment,
            RemoteRuntimeEffectKind::Sleep => Self::Sleep,
            RemoteRuntimeEffectKind::AwaitEvent => Self::AwaitEvent,
            RemoteRuntimeEffectKind::PeekAwaitEvent => Self::PeekAwaitEvent,
            RemoteRuntimeEffectKind::LanguageRuntimeValue => Self::LanguageRuntimeValue,
        }
    }
}

impl From<lash_core::PluginOptions> for RemoteProcessPluginOptions {
    fn from(value: lash_core::PluginOptions) -> Self {
        let lash_core::PluginOptions { plugins } = value;
        Self { plugins }
    }
}

impl From<RemoteProcessPluginOptions> for lash_core::PluginOptions {
    fn from(value: RemoteProcessPluginOptions) -> Self {
        let RemoteProcessPluginOptions { plugins } = value;
        Self { plugins }
    }
}

impl From<lash_core::ModelLimits> for RemoteProcessModelLimits {
    fn from(value: lash_core::ModelLimits) -> Self {
        Self {
            context_window_tokens: value.context_window_tokens.get(),
            output_token_capacity: value.output_token_capacity.map(|value| value.get()),
        }
    }
}

impl From<lash_core::ModelSpec> for RemoteProcessModelSpec {
    fn from(value: lash_core::ModelSpec) -> Self {
        let lash_core::ModelSpec {
            id,
            variant,
            capability,
            limits,
        } = value;
        Self {
            id,
            variant: variant.into(),
            capability: capability.into(),
            limits: limits.into(),
        }
    }
}

impl TryFrom<RemoteProcessModelSpec> for lash_core::ModelSpec {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessModelSpec) -> Result<Self, Self::Error> {
        let RemoteProcessModelSpec {
            id,
            variant,
            capability,
            limits,
        } = value;
        let model = lash_core::ModelSpec::builder(id)
            .variant(variant.into())
            .context_window_tokens(limits.context_window_tokens);
        let model = match limits.output_token_capacity {
            Some(capacity) => model.output_token_capacity(capacity),
            None => model,
        };
        let model = model
            .build()
            .map_err(|err| RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessExecutionPolicy",
                message: err.to_string(),
            })?
            .with_capability(capability.into());
        Ok(model)
    }
}

impl From<lash_core::SessionPolicy> for RemoteProcessExecutionPolicy {
    fn from(value: lash_core::SessionPolicy) -> Self {
        let lash_core::SessionPolicy {
            model,
            provider_id,
            session_id,
            autonomous,
            turn_budget,
            // The no-progress budget is host-owned live policy and is
            // deliberately absent from the remote process wire: its default is
            // bounded, so a peer that never hears the value resolves to the
            // bound. Omission can therefore only fail safe — it can drop an
            // explicit `Unbounded` opt-out, never an explicit bound. Carrying
            // it would be a `REMOTE_PROTOCOL_VERSION` shape change for a knob
            // the peer's own host configuration already supplies.
            no_progress_budget: _,
            // Charge appetite is likewise host-owned live policy. A remote
            // peer resolves its own safe default unless its host opts in.
            charge_safety: _,
            prompt,
            generation,
        } = value;
        Self {
            model: model.into(),
            provider_id,
            session_id,
            autonomous,
            turn_budget: turn_budget.into(),
            prompt: prompt.into(),
            generation: generation.into(),
        }
    }
}

impl From<lash_core::TurnBudget> for RemoteTurnBudget {
    fn from(value: lash_core::TurnBudget) -> Self {
        match value {
            lash_core::TurnBudget::Bounded(limit) => Self::Bounded(limit),
            lash_core::TurnBudget::Unbounded => Self::Unbounded,
        }
    }
}

impl From<RemoteTurnBudget> for lash_core::TurnBudget {
    fn from(value: RemoteTurnBudget) -> Self {
        match value {
            RemoteTurnBudget::Bounded(limit) => Self::Bounded(limit),
            RemoteTurnBudget::Unbounded => Self::Unbounded,
        }
    }
}

impl TryFrom<RemoteProcessExecutionPolicy> for lash_core::SessionPolicy {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessExecutionPolicy) -> Result<Self, Self::Error> {
        let RemoteProcessExecutionPolicy {
            model,
            provider_id,
            session_id,
            autonomous,
            turn_budget,
            prompt,
            generation,
        } = value;
        Ok(Self {
            model: model.try_into()?,
            provider_id,
            session_id,
            autonomous,
            turn_budget: turn_budget.into(),
            no_progress_budget: lash_core::NoProgressBudget::default(),
            charge_safety: lash_core::ChargeSafetyPolicy::default(),
            prompt: prompt.into(),
            generation: generation.try_into()?,
        })
    }
}

impl From<lash_core::ProcessExecutionEnvSpec> for RemoteProcessExecutionEnvSpec {
    fn from(value: lash_core::ProcessExecutionEnvSpec) -> Self {
        let lash_core::ProcessExecutionEnvSpec {
            plugin_options,
            policy,
        } = value;
        Self {
            plugin_options: plugin_options.into(),
            policy: policy.into(),
        }
    }
}

impl TryFrom<RemoteProcessExecutionEnvSpec> for lash_core::ProcessExecutionEnvSpec {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessExecutionEnvSpec) -> Result<Self, Self::Error> {
        value.validate("RemoteProcessExecutionEnvSpec")?;
        let RemoteProcessExecutionEnvSpec {
            plugin_options,
            policy,
        } = value;
        Ok(Self {
            plugin_options: plugin_options.into(),
            policy: policy.try_into()?,
        })
    }
}
