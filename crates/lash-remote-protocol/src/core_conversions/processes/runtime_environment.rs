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
                        owner: identity.owner,
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
            lash_core::runtime::RuntimeSubject::SessionNode {
                session_id,
                node_id,
            } => Self::SessionNode {
                session_id,
                node_id,
            },
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
                                owner: identity.owner,
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
            RemoteRuntimeSubject::SessionNode {
                session_id,
                node_id,
            } => Self::SessionNode {
                session_id,
                node_id,
            },
        }
    }
}

impl From<lash_core::AdmittedPluginConfig> for RemoteProcessPluginConfig {
    fn from(value: lash_core::AdmittedPluginConfig) -> Self {
        let lash_core::AdmittedPluginConfig { revision, config } = value;
        let (protocol, namespaces) = std::sync::Arc::unwrap_or_clone(config).into_recorded_parts();
        Self {
            revision,
            protocol,
            namespaces,
        }
    }
}

impl From<RemoteProcessPluginConfig> for lash_core::AdmittedPluginConfig {
    fn from(value: RemoteProcessPluginConfig) -> Self {
        let RemoteProcessPluginConfig {
            revision,
            protocol,
            namespaces,
        } = value;
        Self::new(
            lash_core::PluginConfig::from_recorded_parts(protocol, namespaces),
            revision,
        )
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

impl From<lash_core::ModelMetadata> for RemoteModelMetadata {
    fn from(value: lash_core::ModelMetadata) -> Self {
        let lash_core::ModelMetadata {
            wire_model,
            extra_body,
            capability,
            limits,
            request_defaults,
        } = value;
        Self {
            wire_model,
            extra_body,
            capability: capability.into(),
            limits: limits.into(),
            request_defaults: request_defaults.into(),
        }
    }
}

impl TryFrom<RemoteModelMetadata> for lash_core::ModelMetadata {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteModelMetadata) -> Result<Self, Self::Error> {
        let RemoteModelMetadata {
            wire_model,
            extra_body,
            capability,
            limits,
            request_defaults,
        } = value;
        let model = lash_core::ModelMetadata::builder(wire_model)
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
            .with_capability(capability.into())
            .with_extra_body(extra_body)
            .with_request_defaults(request_defaults.into());
        Ok(model)
    }
}

impl From<lash_core::ModelConfig> for RemoteModelConfig {
    fn from(value: lash_core::ModelConfig) -> Self {
        let lash_core::ModelConfig { model, reasoning } = value;
        Self {
            key: model.key().as_str().to_string(),
            metadata: model.metadata().clone().into(),
            reasoning: reasoning.into(),
        }
    }
}

impl TryFrom<RemoteModelConfig> for lash_core::ModelConfig {
    type Error = RemoteProtocolError;

    /// The remote carrier conveys a binding the session already recorded;
    /// decoding it restores that recorded value, never a fresh lookup.
    fn try_from(value: RemoteModelConfig) -> Result<Self, Self::Error> {
        let RemoteModelConfig {
            key,
            metadata,
            reasoning,
        } = value;
        Ok(Self {
            model: lash_core::RecordedModel::mint(
                lash_core::ModelKey::new(key),
                metadata.try_into()?,
            ),
            reasoning: reasoning.into(),
        })
    }
}

impl From<lash_core::SessionPolicy> for RemoteProcessExecutionPolicy {
    fn from(value: lash_core::SessionPolicy) -> Self {
        let lash_core::SessionPolicy {
            model,
            attachment_acceptance,
            session_id,
            autonomous,
            turn_budget,
            no_progress_budget,
            charge_safety,
            prompt,
            generation,
        } = value;
        Self {
            model: model.map(Into::into),
            attachment_acceptance: std::sync::Arc::unwrap_or_clone(attachment_acceptance).into(),
            session_id,
            autonomous,
            turn_budget: turn_budget.into(),
            no_progress_budget: no_progress_budget.into(),
            charge_safety: charge_safety.into(),
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

impl From<lash_core::NoProgressBudget> for RemoteNoProgressBudget {
    fn from(value: lash_core::NoProgressBudget) -> Self {
        match value {
            lash_core::NoProgressBudget::Bounded(limit) => Self::Bounded(limit),
            lash_core::NoProgressBudget::Unbounded => Self::Unbounded,
        }
    }
}

impl From<RemoteNoProgressBudget> for lash_core::NoProgressBudget {
    fn from(value: RemoteNoProgressBudget) -> Self {
        match value {
            RemoteNoProgressBudget::Bounded(limit) => Self::Bounded(limit),
            RemoteNoProgressBudget::Unbounded => Self::Unbounded,
        }
    }
}

impl From<lash_core::ChargeSafetyPolicy> for RemoteChargeSafetyPolicy {
    fn from(value: lash_core::ChargeSafetyPolicy) -> Self {
        match value {
            lash_core::ChargeSafetyPolicy::RequireGuarantee => Self::RequireGuarantee,
            lash_core::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries,
                max_duplicate_cost_tokens,
            } => Self::AcceptDuplicateBilling {
                max_unsafe_retries,
                max_duplicate_cost_tokens,
            },
        }
    }
}

impl From<RemoteChargeSafetyPolicy> for lash_core::ChargeSafetyPolicy {
    fn from(value: RemoteChargeSafetyPolicy) -> Self {
        match value {
            RemoteChargeSafetyPolicy::RequireGuarantee => Self::RequireGuarantee,
            RemoteChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries,
                max_duplicate_cost_tokens,
            } => Self::AcceptDuplicateBilling {
                max_unsafe_retries,
                max_duplicate_cost_tokens,
            },
        }
    }
}

impl TryFrom<RemoteProcessExecutionPolicy> for lash_core::SessionPolicy {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessExecutionPolicy) -> Result<Self, Self::Error> {
        let RemoteProcessExecutionPolicy {
            model,
            attachment_acceptance,
            session_id,
            autonomous,
            turn_budget,
            no_progress_budget,
            charge_safety,
            prompt,
            generation,
        } = value;
        Ok(Self {
            model: model.map(TryInto::try_into).transpose()?,
            attachment_acceptance: std::sync::Arc::new(attachment_acceptance.into()),
            session_id,
            autonomous,
            turn_budget: turn_budget.into(),
            no_progress_budget: no_progress_budget.into(),
            charge_safety: charge_safety.into(),
            prompt: prompt.into(),
            generation: generation.try_into()?,
        })
    }
}

impl From<lash_core::ProcessExecutionEnvSpec> for RemoteProcessExecutionEnvSpec {
    fn from(value: lash_core::ProcessExecutionEnvSpec) -> Self {
        let lash_core::ProcessExecutionEnvSpec {
            plugin_config,
            policy,
            render,
        } = value;
        Self {
            plugin_config: plugin_config.into(),
            policy: policy.into(),
            render: render.map(|record| crate::processes::RemoteRecordedRender {
                renderer_id: record.renderer_id,
                params: record.params,
            }),
        }
    }
}

impl TryFrom<RemoteProcessExecutionEnvSpec> for lash_core::ProcessExecutionEnvSpec {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteProcessExecutionEnvSpec) -> Result<Self, Self::Error> {
        value.validate("RemoteProcessExecutionEnvSpec")?;
        let RemoteProcessExecutionEnvSpec {
            plugin_config,
            policy,
            render,
        } = value;
        Ok(Self {
            plugin_config: plugin_config.into(),
            policy: policy.try_into()?,
            render: render.map(|record| lash_core::RecordedRender {
                renderer_id: record.renderer_id,
                params: record.params,
            }),
        })
    }
}
