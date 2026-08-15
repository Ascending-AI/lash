impl RemoteLlmRequest {
    pub fn from_core(request_id: impl Into<String>, value: core_llm::LlmRequest) -> Self {
        let core_llm::LlmRequest {
            model,
            messages,
            attachments,
            tools,
            tool_choice,
            model_variant,
            model_capability,
            generation,
            scope,
            output_spec,
            stream_events: _,
            provider_trace: _,
            resolved_stored: _,
        } = value;
        Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            request_id: request_id.into(),
            scope: scope.into(),
            model_intent: RemoteModelIntent {
                model,
                variant: model_variant.into(),
                capability: model_capability.into(),
                provider: None,
                metadata: HashMap::new(),
            },
            messages: messages.into_iter().map(Into::into).collect(),
            attachments: attachments.into_iter().map(Into::into).collect(),
            tools: tools.iter().cloned().map(Into::into).collect(),
            tool_choice: tool_choice.into(),
            output_spec: output_spec.map(Into::into),
            generation: generation.into(),
            metadata: HashMap::new(),
        }
    }
}

impl TryFrom<RemoteLlmRequest> for core_llm::LlmRequest {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteLlmRequest) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteLlmRequest {
            protocol_version: _,
            request_id: _,
            model_intent,
            messages,
            attachments,
            tools,
            tool_choice,
            output_spec,
            generation,
            scope,
            metadata: _,
        } = value;
        let RemoteModelIntent {
            model,
            variant,
            capability,
            provider: _,
            metadata: _,
        } = model_intent;
        Ok(Self {
            model,
            messages: messages.into_iter().map(Into::into).collect(),
            attachments: attachments
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            resolved_stored: Default::default(),
            tools: Arc::new(tools.into_iter().map(Into::into).collect()),
            tool_choice: tool_choice.into(),
            model_variant: variant.into(),
            model_capability: capability.into(),
            generation: generation.try_into()?,
            scope: scope.into(),
            output_spec: output_spec.map(Into::into),
            stream_events: None,
            provider_trace: None,
        })
    }
}

impl From<core_llm::ModelCapability> for RemoteModelCapability {
    fn from(value: core_llm::ModelCapability) -> Self {
        let core_llm::ModelCapability {
            reasoning,
            cache_control,
            stream_termination,
            sampling,
        } = value;
        Self {
            reasoning: reasoning.map(Into::into),
            cache_control: cache_control.map(Into::into),
            stream_termination: stream_termination.map(Into::into),
            sampling: sampling.into(),
        }
    }
}

impl From<RemoteModelCapability> for core_llm::ModelCapability {
    fn from(value: RemoteModelCapability) -> Self {
        let RemoteModelCapability {
            reasoning,
            cache_control,
            stream_termination,
            sampling,
        } = value;
        Self {
            reasoning: reasoning.map(Into::into),
            cache_control: cache_control.map(Into::into),
            stream_termination: stream_termination.map(Into::into),
            sampling: sampling.into(),
        }
    }
}

impl From<core_llm::SamplingCapability> for RemoteSamplingCapability {
    fn from(value: core_llm::SamplingCapability) -> Self {
        match value {
            core_llm::SamplingCapability::Configurable => Self::Configurable,
            core_llm::SamplingCapability::Pinned => Self::Pinned,
        }
    }
}

impl From<RemoteSamplingCapability> for core_llm::SamplingCapability {
    fn from(value: RemoteSamplingCapability) -> Self {
        match value {
            RemoteSamplingCapability::Configurable => Self::Configurable,
            RemoteSamplingCapability::Pinned => Self::Pinned,
        }
    }
}

impl From<core_llm::CacheControlDialect> for RemoteCacheControlDialect {
    fn from(value: core_llm::CacheControlDialect) -> Self {
        match value {
            core_llm::CacheControlDialect::Anthropic => Self::Anthropic,
            core_llm::CacheControlDialect::Gemini => Self::Gemini,
        }
    }
}

impl From<RemoteCacheControlDialect> for core_llm::CacheControlDialect {
    fn from(value: RemoteCacheControlDialect) -> Self {
        match value {
            RemoteCacheControlDialect::Anthropic => Self::Anthropic,
            RemoteCacheControlDialect::Gemini => Self::Gemini,
        }
    }
}

impl From<core_llm::StreamTermination> for RemoteStreamTermination {
    fn from(value: core_llm::StreamTermination) -> Self {
        match value {
            core_llm::StreamTermination::RequireTerminalEvidence => {
                Self::RequireTerminalEvidence
            }
            core_llm::StreamTermination::EofTolerated => Self::EofTolerated,
        }
    }
}

impl From<RemoteStreamTermination> for core_llm::StreamTermination {
    fn from(value: RemoteStreamTermination) -> Self {
        match value {
            RemoteStreamTermination::RequireTerminalEvidence => {
                Self::RequireTerminalEvidence
            }
            RemoteStreamTermination::EofTolerated => Self::EofTolerated,
        }
    }
}

impl From<core_llm::ReasoningCapability> for RemoteReasoningCapability {
    fn from(value: core_llm::ReasoningCapability) -> Self {
        let core_llm::ReasoningCapability {
            efforts,
            default_effort,
            aliases,
            encoding,
            disable,
            mandatory,
        } = value;
        Self {
            efforts,
            default_effort,
            aliases,
            encoding: encoding.into(),
            disable: disable.map(Into::into),
            mandatory,
        }
    }
}

impl From<RemoteReasoningCapability> for core_llm::ReasoningCapability {
    fn from(value: RemoteReasoningCapability) -> Self {
        let RemoteReasoningCapability {
            efforts,
            default_effort,
            aliases,
            encoding,
            disable,
            mandatory,
        } = value;
        Self {
            efforts,
            default_effort,
            aliases,
            encoding: encoding.into(),
            disable: disable.map(Into::into),
            mandatory,
        }
    }
}

impl From<core_llm::ReasoningSelection> for RemoteReasoningSelection {
    fn from(value: core_llm::ReasoningSelection) -> Self {
        match value {
            core_llm::ReasoningSelection::ProviderDefault => Self::ProviderDefault,
            core_llm::ReasoningSelection::Disabled => Self::Disabled,
            core_llm::ReasoningSelection::Effort(effort) => Self::Effort(effort),
        }
    }
}

impl From<RemoteReasoningSelection> for core_llm::ReasoningSelection {
    fn from(value: RemoteReasoningSelection) -> Self {
        match value {
            RemoteReasoningSelection::ProviderDefault => Self::ProviderDefault,
            RemoteReasoningSelection::Disabled => Self::Disabled,
            RemoteReasoningSelection::Effort(effort) => Self::Effort(effort),
        }
    }
}

impl From<core_llm::ReasoningDisableEncoding> for RemoteReasoningDisableEncoding {
    fn from(value: core_llm::ReasoningDisableEncoding) -> Self {
        match value {
            core_llm::ReasoningDisableEncoding::Native => Self::Native,
            core_llm::ReasoningDisableEncoding::Omit => Self::Omit,
            core_llm::ReasoningDisableEncoding::Effort(effort) => Self::Effort(effort),
            core_llm::ReasoningDisableEncoding::Budget(budget) => Self::Budget(budget),
            core_llm::ReasoningDisableEncoding::ToggleFalse => Self::ToggleFalse,
        }
    }
}

impl From<RemoteReasoningDisableEncoding> for core_llm::ReasoningDisableEncoding {
    fn from(value: RemoteReasoningDisableEncoding) -> Self {
        match value {
            RemoteReasoningDisableEncoding::Native => Self::Native,
            RemoteReasoningDisableEncoding::Omit => Self::Omit,
            RemoteReasoningDisableEncoding::Effort(effort) => Self::Effort(effort),
            RemoteReasoningDisableEncoding::Budget(budget) => Self::Budget(budget),
            RemoteReasoningDisableEncoding::ToggleFalse => Self::ToggleFalse,
        }
    }
}

impl From<core_llm::ReasoningEncoding> for RemoteReasoningEncoding {
    fn from(value: core_llm::ReasoningEncoding) -> Self {
        match value {
            core_llm::ReasoningEncoding::Effort => Self::Effort,
            core_llm::ReasoningEncoding::Budget(map) => Self::Budget(map),
        }
    }
}

impl From<RemoteReasoningEncoding> for core_llm::ReasoningEncoding {
    fn from(value: RemoteReasoningEncoding) -> Self {
        match value {
            RemoteReasoningEncoding::Effort => Self::Effort,
            RemoteReasoningEncoding::Budget(map) => Self::Budget(map),
        }
    }
}

impl From<core_llm::LlmRequestScope> for RemoteLlmRequestScope {
    fn from(value: core_llm::LlmRequestScope) -> Self {
        Self {
            session_id: value.session_id,
            agent_frame_id: value.agent_frame_id,
            request_id: value.request_id,
        }
    }
}

impl From<RemoteLlmRequestScope> for core_llm::LlmRequestScope {
    fn from(value: RemoteLlmRequestScope) -> Self {
        Self::new(value.session_id, value.agent_frame_id, value.request_id)
    }
}

impl RemoteLlmResponse {
    pub fn from_core(request_id: impl Into<String>, value: core_llm::LlmResponse) -> Self {
        let core_llm::LlmResponse {
            full_text,
            parts,
            usage,
            terminal_reason,
            terminal_diagnostic,
            provider_usage,
            request_body,
            http_summary,
            execution_evidence,
            generation_disposition,
            response_metadata,
        } = value;
        let mut diagnostics = Vec::new();
        if let Some(message) = terminal_diagnostic {
            diagnostics.push(RemoteDiagnostic {
                kind: "terminal".to_string(),
                code: None,
                message,
                data: None,
            });
        }
        Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            request_id: request_id.into(),
            full_text,
            output_parts: parts.into_iter().map(Into::into).collect(),
            usage: usage.into(),
            terminal_reason: terminal_reason.into(),
            diagnostics,
            provider_metadata: RemoteProviderMetadata {
                usage: provider_usage,
                request_body,
                http_summary,
                data: response_metadata,
            },
            execution_evidence: execution_evidence.map(Into::into),
            generation_disposition: generation_disposition.map(Into::into),
        }
    }
}

impl From<RemoteLlmResponse> for core_llm::LlmResponse {
    fn from(value: RemoteLlmResponse) -> Self {
        let RemoteLlmResponse {
            protocol_version: _,
            request_id: _,
            full_text,
            output_parts,
            usage,
            terminal_reason,
            diagnostics,
            provider_metadata,
            execution_evidence,
            generation_disposition,
        } = value;
        let RemoteProviderMetadata {
            usage: provider_usage,
            request_body,
            http_summary,
            data: response_metadata,
        } = provider_metadata;
        Self {
            full_text,
            parts: output_parts.into_iter().map(Into::into).collect(),
            usage: usage.into(),
            terminal_reason: terminal_reason.into(),
            terminal_diagnostic: diagnostics.first().map(|diag| diag.message.clone()),
            provider_usage,
            request_body,
            http_summary,
            execution_evidence: execution_evidence.map(Into::into),
            generation_disposition: generation_disposition.map(Into::into),
            response_metadata,
        }
    }
}

impl From<core_llm::GenerationDisposition> for RemoteGenerationDisposition {
    fn from(value: core_llm::GenerationDisposition) -> Self {
        let core_llm::GenerationDisposition {
            output_token_cap,
            temperature,
            seed,
            stop_sequences,
            cache,
        } = value;
        Self {
            output_token_cap: output_token_cap.into(),
            temperature: temperature.into(),
            seed: seed.into(),
            stop_sequences: stop_sequences.into(),
            cache: cache.into(),
        }
    }
}

impl From<RemoteGenerationDisposition> for core_llm::GenerationDisposition {
    fn from(value: RemoteGenerationDisposition) -> Self {
        let RemoteGenerationDisposition {
            output_token_cap,
            temperature,
            seed,
            stop_sequences,
            cache,
        } = value;
        Self {
            output_token_cap: output_token_cap.into(),
            temperature: temperature.into(),
            seed: seed.into(),
            stop_sequences: stop_sequences.into(),
            cache: cache.into(),
        }
    }
}

impl From<core_llm::GenerationOptionDisposition> for RemoteGenerationOptionDisposition {
    fn from(value: core_llm::GenerationOptionDisposition) -> Self {
        match value {
            core_llm::GenerationOptionDisposition::NotRequested => Self::NotRequested,
            core_llm::GenerationOptionDisposition::Applied => Self::Applied,
            core_llm::GenerationOptionDisposition::SuppressedProtocolOwned => {
                Self::SuppressedProtocolOwned
            }
            core_llm::GenerationOptionDisposition::OmittedUnsupported => Self::OmittedUnsupported,
            core_llm::GenerationOptionDisposition::OmittedSamplingPinned => {
                Self::OmittedSamplingPinned
            }
            core_llm::GenerationOptionDisposition::ClampedToCapacity => Self::ClampedToCapacity,
        }
    }
}

impl From<RemoteGenerationOptionDisposition> for core_llm::GenerationOptionDisposition {
    fn from(value: RemoteGenerationOptionDisposition) -> Self {
        match value {
            RemoteGenerationOptionDisposition::NotRequested => Self::NotRequested,
            RemoteGenerationOptionDisposition::Applied => Self::Applied,
            RemoteGenerationOptionDisposition::SuppressedProtocolOwned => {
                Self::SuppressedProtocolOwned
            }
            RemoteGenerationOptionDisposition::OmittedUnsupported => Self::OmittedUnsupported,
            RemoteGenerationOptionDisposition::OmittedSamplingPinned => {
                Self::OmittedSamplingPinned
            }
            RemoteGenerationOptionDisposition::ClampedToCapacity => Self::ClampedToCapacity,
        }
    }
}

impl From<core_llm::ExecutionEvidence> for RemoteExecutionEvidence {
    fn from(value: core_llm::ExecutionEvidence) -> Self {
        let core_llm::ExecutionEvidence {
            served_model,
            provider_response_id,
            provider_request_id,
            reasoning_output_tokens,
            provider_finish_reason,
            collection_interruption,
        } = value;
        Self {
            served_model,
            provider_response_id,
            provider_request_id,
            reasoning_output_tokens,
            provider_finish_reason,
            collection_interruption: collection_interruption.map(Into::into),
        }
    }
}

impl From<RemoteExecutionEvidence> for core_llm::ExecutionEvidence {
    fn from(value: RemoteExecutionEvidence) -> Self {
        let RemoteExecutionEvidence {
            served_model,
            provider_response_id,
            provider_request_id,
            reasoning_output_tokens,
            provider_finish_reason,
            collection_interruption,
        } = value;
        Self {
            served_model,
            provider_response_id,
            provider_request_id,
            reasoning_output_tokens,
            provider_finish_reason,
            collection_interruption: collection_interruption.map(Into::into),
        }
    }
}

impl From<core_llm::ExecutionEvidenceCollectionInterruption>
    for RemoteExecutionEvidenceCollectionInterruption
{
    fn from(value: core_llm::ExecutionEvidenceCollectionInterruption) -> Self {
        match value {
            core_llm::ExecutionEvidenceCollectionInterruption::ProtocolAbort => {
                Self::ProtocolAbort
            }
        }
    }
}

impl From<RemoteExecutionEvidenceCollectionInterruption>
    for core_llm::ExecutionEvidenceCollectionInterruption
{
    fn from(value: RemoteExecutionEvidenceCollectionInterruption) -> Self {
        match value {
            RemoteExecutionEvidenceCollectionInterruption::ProtocolAbort => Self::ProtocolAbort,
        }
    }
}

impl From<core_llm::LlmCallRecord> for RemoteLlmCallRecord {
    fn from(value: core_llm::LlmCallRecord) -> Self {
        let core_llm::LlmCallRecord {
            call_id: core_llm::LlmCallId(call_id),
            label,
            replay_drops,
            attempts,
        } = value;
        Self {
            call_id,
            label,
            replay_drops: replay_drops.into_iter().map(Into::into).collect(),
            attempts: attempts.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<core_llm::ProviderReplayDrop> for RemoteProviderReplayDrop {
    fn from(value: core_llm::ProviderReplayDrop) -> Self {
        Self {
            kind: match value.kind {
                core_llm::ProviderReplayKind::ResponseText => RemoteProviderReplayKind::ResponseText,
                core_llm::ProviderReplayKind::Reasoning => RemoteProviderReplayKind::Reasoning,
                core_llm::ProviderReplayKind::ToolCall => RemoteProviderReplayKind::ToolCall,
            },
            reason: match value.reason {
                core_llm::ProviderReplayDropReason::Unstamped => RemoteProviderReplayDropReason::Unstamped,
                core_llm::ProviderReplayDropReason::ForeignRoute => RemoteProviderReplayDropReason::ForeignRoute,
            },
            minting_route: value.minting_route.map(Into::into),
            serving_route: value.serving_route.into(),
        }
    }
}

impl From<core_llm::AttemptRecord> for RemoteAttemptRecord {
    fn from(value: core_llm::AttemptRecord) -> Self {
        let core_llm::AttemptRecord {
            ordinal,
            started_at,
            duration,
            outcome,
            protocol_position,
            retry_budget_consumed,
            retry_decision,
            error,
            evidence,
            generation_disposition,
            usage,
        } = value;
        Self {
            ordinal,
            started_at_ms: started_at,
            duration_ms: duration.as_millis().try_into().unwrap_or(u64::MAX),
            outcome: outcome.into(),
            protocol_position: protocol_position.into(),
            retry_budget_consumed,
            retry_decision: retry_decision.map(Into::into),
            error: error.map(Into::into),
            evidence: evidence.map(Into::into),
            generation_disposition: generation_disposition.map(Into::into),
            usage: usage.map(Into::into),
        }
    }
}

impl From<core_llm::AttemptOutcome> for RemoteAttemptOutcome {
    fn from(value: core_llm::AttemptOutcome) -> Self {
        match value {
            core_llm::AttemptOutcome::Completed => Self::Completed,
            core_llm::AttemptOutcome::Failed => Self::Failed,
            core_llm::AttemptOutcome::Aborted => Self::Aborted,
            core_llm::AttemptOutcome::Interrupted => Self::Interrupted,
        }
    }
}

impl From<core_llm::ProtocolPosition> for RemoteProtocolPosition {
    fn from(value: core_llm::ProtocolPosition) -> Self {
        match value {
            core_llm::ProtocolPosition::NoResponse => Self::NoResponse,
            core_llm::ProtocolPosition::ResponseObserved => Self::ResponseObserved,
            core_llm::ProtocolPosition::OutputStarted => Self::OutputStarted,
            core_llm::ProtocolPosition::TerminalObserved => Self::TerminalObserved,
        }
    }
}

impl From<core_llm::RetryDecision> for RemoteRetryDecision {
    fn from(value: core_llm::RetryDecision) -> Self {
        let core_llm::RetryDecision {
            scheduled,
            delay,
            reason,
        } = value;
        Self {
            scheduled,
            delay_ms: delay.map(|value| value.as_millis().try_into().unwrap_or(u64::MAX)),
            reason,
        }
    }
}

impl From<core_llm::NormalizedError> for RemoteNormalizedError {
    fn from(value: core_llm::NormalizedError) -> Self {
        let core_llm::NormalizedError {
            class,
            provider_code,
            http_status,
            provider_request_id,
            retry_after,
            diagnostic: _,
        } = value;
        Self {
            class,
            provider_code,
            http_status,
            provider_request_id,
            retry_after_ms: retry_after
                .map(|value| value.as_millis().try_into().unwrap_or(u64::MAX)),
        }
    }
}

impl From<core_llm::GenerationOptions> for RemoteGenerationOptions {
    fn from(value: core_llm::GenerationOptions) -> Self {
        let core_llm::GenerationOptions {
            output_token_cap,
            temperature,
            seed,
            stop_sequences,
            projection_provenance: _,
        } = value;
        Self {
            output_token_cap: output_token_cap.map(|cap| cap.get() as u64),
            temperature: temperature.map(Into::into),
            seed,
            stop_sequences,
        }
    }
}

impl TryFrom<RemoteGenerationOptions> for core_llm::GenerationOptions {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteGenerationOptions) -> Result<Self, Self::Error> {
        value.validate("RemoteGenerationOptions")?;
        let RemoteGenerationOptions {
            output_token_cap,
            temperature,
            seed,
            stop_sequences,
        } = value;
        let temperature = temperature
            .map(core_llm::NonNegativeFiniteF64::try_from)
            .transpose()
            .map_err(|error| RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteGenerationOptions",
                message: format!("generation.temperature is invalid: {error}"),
            })?;
        Ok(Self {
            output_token_cap: output_token_cap
                .and_then(|cap| usize::try_from(cap).ok())
                .and_then(NonZeroUsize::new),
            temperature,
            seed,
            stop_sequences,
            projection_provenance: Default::default(),
        })
    }
}

impl From<core_llm::LlmMessage> for RemoteLlmMessage {
    fn from(value: core_llm::LlmMessage) -> Self {
        let core_llm::LlmMessage { role, blocks } = value;
        Self {
            role: role.into(),
            content: blocks.iter().cloned().map(Into::into).collect(),
        }
    }
}

impl From<RemoteLlmMessage> for core_llm::LlmMessage {
    fn from(value: RemoteLlmMessage) -> Self {
        let RemoteLlmMessage { role, content } = value;
        Self::new(role.into(), content.into_iter().map(Into::into).collect())
    }
}

impl From<core_llm::LlmRole> for RemoteLlmRole {
    fn from(value: core_llm::LlmRole) -> Self {
        match value {
            core_llm::LlmRole::User => Self::User,
            core_llm::LlmRole::Assistant => Self::Assistant,
            core_llm::LlmRole::System => Self::System,
        }
    }
}

impl From<RemoteLlmRole> for core_llm::LlmRole {
    fn from(value: RemoteLlmRole) -> Self {
        match value {
            RemoteLlmRole::User => Self::User,
            RemoteLlmRole::Assistant => Self::Assistant,
            RemoteLlmRole::System => Self::System,
        }
    }
}

impl From<core_llm::LlmContentBlock> for RemoteLlmContentBlock {
    fn from(value: core_llm::LlmContentBlock) -> Self {
        match value {
            core_llm::LlmContentBlock::Text {
                text,
                response_meta,
                cache_breakpoint,
            } => Self::Text {
                text: text.to_string(),
                response_meta: response_meta.map(Into::into),
                cache_breakpoint,
            },
            core_llm::LlmContentBlock::Attachment { attachment_idx } => Self::Attachment {
                attachment_index: attachment_idx,
            },
            core_llm::LlmContentBlock::ToolCall {
                call_id,
                tool_name,
                input_json,
                replay,
            } => Self::ToolCall {
                call_id,
                tool_name,
                input_json,
                replay: replay.map(Into::into),
            },
            core_llm::LlmContentBlock::ToolResult {
                call_id,
                content,
                tool_name,
            } => Self::ToolResult {
                call_id,
                content,
                tool_name,
            },
            core_llm::LlmContentBlock::Reasoning { text, replay } => Self::Reasoning {
                text,
                replay: replay.map(Into::into),
            },
        }
    }
}

impl From<RemoteLlmContentBlock> for core_llm::LlmContentBlock {
    fn from(value: RemoteLlmContentBlock) -> Self {
        match value {
            RemoteLlmContentBlock::Text {
                text,
                response_meta,
                cache_breakpoint,
            } => Self::Text {
                text: Arc::<str>::from(text),
                response_meta: response_meta.map(Into::into),
                cache_breakpoint,
            },
            RemoteLlmContentBlock::Attachment { attachment_index } => Self::Attachment {
                attachment_idx: attachment_index,
            },
            RemoteLlmContentBlock::ToolCall {
                call_id,
                tool_name,
                input_json,
                replay,
            } => Self::ToolCall {
                call_id,
                tool_name,
                input_json,
                replay: replay.map(Into::into),
            },
            RemoteLlmContentBlock::ToolResult {
                call_id,
                content,
                tool_name,
            } => Self::ToolResult {
                call_id,
                content,
                tool_name,
            },
            RemoteLlmContentBlock::Reasoning { text, replay } => Self::Reasoning {
                text,
                replay: replay.map(Into::into),
            },
        }
    }
}

impl From<core_llm::ProviderRouteIdentity> for RemoteProviderRouteIdentity {
    fn from(value: core_llm::ProviderRouteIdentity) -> Self {
        Self {
            provider: value.provider.into(),
            endpoint: value.endpoint.into(),
            model: value.model.into(),
        }
    }
}

impl From<RemoteProviderRouteIdentity> for core_llm::ProviderRouteIdentity {
    fn from(value: RemoteProviderRouteIdentity) -> Self {
        core_llm::ProviderRouteIdentity::new(value.provider, value.endpoint, value.model)
    }
}

impl From<core_llm::ResponseTextMeta> for RemoteResponseTextMeta {
    fn from(value: core_llm::ResponseTextMeta) -> Self {
        let core_llm::ResponseTextMeta {
            id,
            status,
            phase,
            provider_payload,
            origin,
            ..
        } = value;
        Self {
            id,
            status,
            phase,
            provider_payload,
            origin: origin.map(Into::into),
        }
    }
}

impl From<RemoteResponseTextMeta> for core_llm::ResponseTextMeta {
    fn from(value: RemoteResponseTextMeta) -> Self {
        let RemoteResponseTextMeta {
            id,
            status,
            phase,
            provider_payload,
            origin,
        } = value;
        Self {
            id,
            status,
            phase,
            provider_payload,
            origin: origin.map(Into::into),
            ..Default::default()
        }
    }
}

impl From<core_llm::ProviderReplayMeta> for RemoteProviderReplayMeta {
    fn from(value: core_llm::ProviderReplayMeta) -> Self {
        let core_llm::ProviderReplayMeta {
            item_id,
            opaque,
            origin,
        } = value;
        Self {
            item_id,
            opaque,
            origin: origin.map(Into::into),
        }
    }
}

impl From<RemoteProviderReplayMeta> for core_llm::ProviderReplayMeta {
    fn from(value: RemoteProviderReplayMeta) -> Self {
        let RemoteProviderReplayMeta {
            item_id,
            opaque,
            origin,
        } = value;
        Self {
            item_id,
            opaque,
            origin: origin.map(Into::into),
        }
    }
}

impl From<core_llm::ProviderReasoningReplay> for RemoteProviderReasoningReplay {
    fn from(value: core_llm::ProviderReasoningReplay) -> Self {
        let core_llm::ProviderReasoningReplay {
            item_id,
            encrypted_content,
            signature,
            redacted,
            summary,
            origin,
        } = value;
        Self {
            item_id,
            encrypted_content,
            signature,
            redacted,
            summary,
            origin: origin.map(Into::into),
        }
    }
}

impl From<RemoteProviderReasoningReplay> for core_llm::ProviderReasoningReplay {
    fn from(value: RemoteProviderReasoningReplay) -> Self {
        let RemoteProviderReasoningReplay {
            item_id,
            encrypted_content,
            signature,
            redacted,
            summary,
            origin,
        } = value;
        Self {
            item_id,
            encrypted_content,
            signature,
            redacted,
            summary,
            origin: origin.map(Into::into),
        }
    }
}

impl From<core_llm::AttachmentSource> for RemoteAttachmentSource {
    fn from(value: core_llm::AttachmentSource) -> Self {
        match value {
            core_llm::AttachmentSource::Inline { media_type, bytes } => Self::Inline {
                media_type: media_type.to_string(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
            core_llm::AttachmentSource::Stored { attachment_ref } => Self::Stored {
                attachment_ref: attachment_ref.into(),
            },
            core_llm::AttachmentSource::ExternalUrl { media_type, url } => Self::ExternalUrl {
                media_type: media_type.to_string(),
                url,
            },
            core_llm::AttachmentSource::ProviderFile {
                provider_scope,
                id,
                media_type,
            } => Self::ProviderFile {
                provider_scope: provider_scope.into(),
                id,
                media_type: media_type.map(|media_type| media_type.to_string()),
            },
        }
    }
}

impl TryFrom<RemoteAttachmentSource> for core_llm::AttachmentSource {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteAttachmentSource) -> Result<Self, Self::Error> {
        match value {
            RemoteAttachmentSource::Inline {
                media_type,
                data_base64,
            } => {
                let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_base64.as_bytes())
                .map_err(|err| RemoteProtocolError::InvalidAttachmentData {
                    id: "<inline-attachment>".to_string(),
                    message: err.to_string(),
                })?;
                Ok(Self::inline(parse_media_type(&media_type)?, bytes))
            }
            RemoteAttachmentSource::Stored { attachment_ref } => {
                Ok(Self::stored(attachment_ref.try_into()?))
            }
            RemoteAttachmentSource::ExternalUrl { media_type, url } => {
                Ok(Self::external_url(parse_media_type(&media_type)?, url))
            }
            RemoteAttachmentSource::ProviderFile {
                provider_scope,
                id,
                media_type,
            } => Ok(Self::provider_file(
                provider_scope.into(),
                id,
                media_type
                    .as_deref()
                    .map(parse_media_type)
                    .transpose()?,
            )),
        }
    }
}

impl From<core_llm::ProviderFileScope> for RemoteProviderFileScope {
    fn from(value: core_llm::ProviderFileScope) -> Self {
        Self {
            provider: value.provider,
            credential_scope: value.credential_scope,
        }
    }
}

impl From<RemoteProviderFileScope> for core_llm::ProviderFileScope {
    fn from(value: RemoteProviderFileScope) -> Self {
        Self::new(value.provider, value.credential_scope)
    }
}

impl From<lash_core::AttachmentRef> for RemoteAttachmentRef {
    fn from(value: lash_core::AttachmentRef) -> Self {
        let lash_core::AttachmentRef {
            id,
            media_type,
            byte_len,
            type_metadata,
            label,
        } = value;
        Self {
            id: id.to_string(),
            media_type: media_type.to_string(),
            byte_len,
            type_metadata: type_metadata.map(Into::into),
            label,
        }
    }
}

impl TryFrom<RemoteAttachmentRef> for lash_core::AttachmentRef {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteAttachmentRef) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteAttachmentRef {
            id,
            media_type,
            byte_len,
            type_metadata,
            label,
        } = value;
        let media_type = parse_media_type(&media_type)?;
        Ok(Self {
            id: lash_core::AttachmentId::new(id),
            media_type,
            byte_len,
            type_metadata: type_metadata.map(Into::into),
            label,
        })
    }
}

fn parse_media_type(value: &str) -> Result<lash_core::MediaType, RemoteProtocolError> {
    lash_core::MediaType::parse(value).map_err(|err| RemoteProtocolError::InvalidAttachmentRef {
        id: "<media-type>".to_string(),
        message: err.to_string(),
    })
}

impl From<lash_core::AttachmentTypeMetadata> for RemoteAttachmentTypeMetadata {
    fn from(value: lash_core::AttachmentTypeMetadata) -> Self {
        match value {
            lash_core::AttachmentTypeMetadata::Image { width, height } => {
                Self::Image { width, height }
            }
        }
    }
}

impl From<RemoteAttachmentTypeMetadata> for lash_core::AttachmentTypeMetadata {
    fn from(value: RemoteAttachmentTypeMetadata) -> Self {
        match value {
            RemoteAttachmentTypeMetadata::Image { width, height } => {
                Self::Image { width, height }
            }
        }
    }
}

impl From<core_llm::LlmToolSpec> for RemoteLlmToolSpec {
    fn from(value: core_llm::LlmToolSpec) -> Self {
        let core_llm::LlmToolSpec {
            name,
            description,
            input_schema,
            output_schema,
        } = value;
        Self {
            name,
            description,
            input_schema: input_schema.into(),
            output_schema: output_schema.into(),
        }
    }
}

impl From<RemoteLlmToolSpec> for core_llm::LlmToolSpec {
    fn from(value: RemoteLlmToolSpec) -> Self {
        let RemoteLlmToolSpec {
            name,
            description,
            input_schema,
            output_schema,
        } = value;
        Self {
            name,
            description,
            input_schema: input_schema.into(),
            output_schema: output_schema.into(),
        }
    }
}

impl From<core_llm::LlmToolChoice> for RemoteLlmToolChoice {
    fn from(value: core_llm::LlmToolChoice) -> Self {
        match value {
            core_llm::LlmToolChoice::Auto => Self::Auto,
            core_llm::LlmToolChoice::None => Self::None,
            core_llm::LlmToolChoice::Required => Self::Required,
        }
    }
}

impl From<RemoteLlmToolChoice> for core_llm::LlmToolChoice {
    fn from(value: RemoteLlmToolChoice) -> Self {
        match value {
            RemoteLlmToolChoice::Auto => Self::Auto,
            RemoteLlmToolChoice::None => Self::None,
            RemoteLlmToolChoice::Required => Self::Required,
        }
    }
}

impl From<core_llm::LlmOutputSpec> for RemoteLlmOutputSpec {
    fn from(value: core_llm::LlmOutputSpec) -> Self {
        match value {
            core_llm::LlmOutputSpec::JsonObject => Self::JsonObject,
            core_llm::LlmOutputSpec::JsonSchema(core_llm::LlmJsonSchema {
                name,
                schema,
                strict,
            }) => Self::JsonSchema {
                name,
                schema: schema.into(),
                strict,
            },
        }
    }
}

impl From<RemoteLlmOutputSpec> for core_llm::LlmOutputSpec {
    fn from(value: RemoteLlmOutputSpec) -> Self {
        match value {
            RemoteLlmOutputSpec::JsonObject => Self::JsonObject,
            RemoteLlmOutputSpec::JsonSchema {
                name,
                schema,
                strict,
            } => Self::JsonSchema(core_llm::LlmJsonSchema {
                name,
                schema: schema.into(),
                strict,
            }),
        }
    }
}

impl From<core_llm::LlmOutputPart> for RemoteLlmOutputPart {
    fn from(value: core_llm::LlmOutputPart) -> Self {
        match value {
            core_llm::LlmOutputPart::Text {
                text,
                response_meta,
            } => Self::Text {
                text,
                response_meta: response_meta.map(Into::into),
            },
            core_llm::LlmOutputPart::Reasoning { text, replay } => Self::Reasoning {
                text,
                replay: replay.map(Into::into),
            },
            core_llm::LlmOutputPart::ToolCall {
                call_id,
                tool_name,
                input_json,
                replay,
            } => Self::ToolCall {
                call_id,
                tool_name,
                input_json,
                replay: replay.map(Into::into),
            },
        }
    }
}

impl From<RemoteLlmOutputPart> for core_llm::LlmOutputPart {
    fn from(value: RemoteLlmOutputPart) -> Self {
        match value {
            RemoteLlmOutputPart::Text {
                text,
                response_meta,
            } => Self::Text {
                text,
                response_meta: response_meta.map(Into::into),
            },
            RemoteLlmOutputPart::Reasoning { text, replay } => Self::Reasoning {
                text,
                replay: replay.map(Into::into),
            },
            RemoteLlmOutputPart::ToolCall {
                call_id,
                tool_name,
                input_json,
                replay,
            } => Self::ToolCall {
                call_id,
                tool_name,
                input_json,
                replay: replay.map(Into::into),
            },
        }
    }
}

impl From<core_llm::LlmTerminalReason> for RemoteLlmTerminalReason {
    fn from(value: core_llm::LlmTerminalReason) -> Self {
        match value {
            core_llm::LlmTerminalReason::Stop => Self::Stop,
            core_llm::LlmTerminalReason::ToolUse => Self::ToolUse,
            core_llm::LlmTerminalReason::OutputLimit => Self::OutputLimit,
            core_llm::LlmTerminalReason::ContextOverflow => Self::ContextOverflow,
            core_llm::LlmTerminalReason::ContentFilter => Self::ContentFilter,
            core_llm::LlmTerminalReason::ProviderError => Self::ProviderError,
            core_llm::LlmTerminalReason::Cancelled => Self::Cancelled,
            core_llm::LlmTerminalReason::Unknown => Self::Unknown,
        }
    }
}

impl From<RemoteLlmTerminalReason> for core_llm::LlmTerminalReason {
    fn from(value: RemoteLlmTerminalReason) -> Self {
        match value {
            RemoteLlmTerminalReason::Stop => Self::Stop,
            RemoteLlmTerminalReason::ToolUse => Self::ToolUse,
            RemoteLlmTerminalReason::OutputLimit => Self::OutputLimit,
            RemoteLlmTerminalReason::ContextOverflow => Self::ContextOverflow,
            RemoteLlmTerminalReason::ContentFilter => Self::ContentFilter,
            RemoteLlmTerminalReason::ProviderError => Self::ProviderError,
            RemoteLlmTerminalReason::Cancelled => Self::Cancelled,
            RemoteLlmTerminalReason::Unknown => Self::Unknown,
        }
    }
}

impl From<lash_core::ProviderFailureKind> for RemoteProviderFailureKind {
    fn from(value: lash_core::ProviderFailureKind) -> Self {
        match value {
            lash_core::ProviderFailureKind::Transport => Self::Transport,
            lash_core::ProviderFailureKind::Timeout => Self::Timeout,
            lash_core::ProviderFailureKind::Http => Self::Http,
            lash_core::ProviderFailureKind::Stream => Self::Stream,
            lash_core::ProviderFailureKind::Auth => Self::Auth,
            lash_core::ProviderFailureKind::Validation => Self::Validation,
            lash_core::ProviderFailureKind::Quota => Self::Quota,
            lash_core::ProviderFailureKind::Unsupported => Self::Unsupported,
            lash_core::ProviderFailureKind::Unknown => Self::Unknown,
        }
    }
}

impl From<RemoteProviderFailureKind> for lash_core::ProviderFailureKind {
    fn from(value: RemoteProviderFailureKind) -> Self {
        match value {
            RemoteProviderFailureKind::Transport => Self::Transport,
            RemoteProviderFailureKind::Timeout => Self::Timeout,
            RemoteProviderFailureKind::Http => Self::Http,
            RemoteProviderFailureKind::Stream => Self::Stream,
            RemoteProviderFailureKind::Auth => Self::Auth,
            RemoteProviderFailureKind::Validation => Self::Validation,
            RemoteProviderFailureKind::Quota => Self::Quota,
            RemoteProviderFailureKind::Unsupported => Self::Unsupported,
            RemoteProviderFailureKind::Unknown => Self::Unknown,
        }
    }
}

impl From<core_llm::LlmUsage> for RemoteUsage {
    fn from(value: core_llm::LlmUsage) -> Self {
        let core_llm::LlmUsage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_output_tokens,
        } = value;
        Self {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_output_tokens,
        }
    }
}

impl From<RemoteUsage> for core_llm::LlmUsage {
    fn from(value: RemoteUsage) -> Self {
        let RemoteUsage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_output_tokens,
        } = value;
        Self {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_output_tokens,
        }
    }
}

impl From<lash_core::TokenUsage> for RemoteUsage {
    fn from(value: lash_core::TokenUsage) -> Self {
        let lash_core::TokenUsage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_output_tokens,
        } = value;
        Self {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_output_tokens,
        }
    }
}

impl From<RemoteUsage> for lash_core::TokenUsage {
    fn from(value: RemoteUsage) -> Self {
        let RemoteUsage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_output_tokens,
        } = value;
        Self {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_output_tokens,
        }
    }
}
