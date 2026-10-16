use super::{
    ContentCategory as C, ContentField, ContentMarks, ContentPolicy, FieldValue, JsonField,
};
use crate::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

fn text(
    category: C,
    path: impl Into<String>,
    value: &mut String,
    f: &mut dyn FnMut(&mut ContentField<'_>),
) {
    f(&mut ContentField {
        category,
        path: path.into(),
        value: FieldValue::Text(value),
        omitted: false,
    });
}
struct DeferredJson<'a, T> {
    original: &'a mut T,
    encoded: Option<Value>,
    omitted: bool,
}
impl<T: Serialize + Default> JsonField for DeferredJson<'_, T> {
    fn value(&mut self) -> &mut Value {
        self.encoded
            .get_or_insert_with(|| serde_json::to_value(&*self.original).unwrap_or(Value::Null))
    }
    fn omit(&mut self) {
        self.omitted = true;
        self.encoded = None;
    }
}
fn json<T: Serialize + DeserializeOwned + Default>(
    category: C,
    path: impl Into<String>,
    value: &mut T,
    f: &mut dyn FnMut(&mut ContentField<'_>),
) {
    // A policy callback can omit without ever serializing or cloning the field.
    let mut deferred = DeferredJson {
        original: value,
        encoded: None,
        omitted: false,
    };
    f(&mut ContentField {
        category,
        path: path.into(),
        value: FieldValue::Json(&mut deferred),
        omitted: false,
    });
    if deferred.omitted {
        *deferred.original = T::default();
    } else if let Some(encoded) = deferred.encoded {
        *deferred.original = serde_json::from_value(encoded).unwrap_or_default();
    }
}

fn request(req: &mut TraceLlmRequest, f: &mut dyn FnMut(&mut ContentField<'_>)) {
    if let Some(instructions) = &mut req.system_instructions {
        text(
            C::SystemInstructions,
            "system_instructions",
            instructions,
            f,
        );
    }
    messages(&mut req.messages, C::InputMessages, "input_messages", f);
    tools(&mut req.tools, f);
    if let Some(spec) = &mut req.output_spec
        && spec.get("type").and_then(Value::as_str) == Some("json_schema")
    {
        if let Some(schema) = spec.get_mut("schema") {
            json(C::OutputSchema, "output_schema", schema, f);
        }
    } else if req
        .output_spec
        .as_ref()
        .is_some_and(|spec| spec.get("type").and_then(Value::as_str) != Some("json_object"))
    {
        json(C::OutputSchema, "output_schema", &mut req.output_spec, f);
    }
}
fn messages(
    messages: &mut [TraceLlmMessage],
    category: C,
    prefix: &str,
    f: &mut dyn FnMut(&mut ContentField<'_>),
) {
    for (i, message) in messages.iter_mut().enumerate() {
        for (j, block) in message.blocks.iter_mut().enumerate() {
            let path = format!("{prefix}[{i}].parts[{j}]");
            match block {
                TraceContentBlock::Text { text: value, .. } => text(category, path, value, f),
                TraceContentBlock::Attachment { reference } => json(category, path, reference, f),
                TraceContentBlock::ToolCall { input_json, .. } => {
                    json(category, path, input_json, f)
                }
                TraceContentBlock::ToolResult { content, .. } => {
                    for (k, part) in content.iter_mut().enumerate() {
                        match part {
                            TraceToolResultBlock::Text { text: value } => {
                                text(category, format!("{path}.content[{k}]"), value, f)
                            }
                            TraceToolResultBlock::Attachment { reference } => {
                                json(category, format!("{path}.content[{k}]"), reference, f)
                            }
                        }
                    }
                }
                TraceContentBlock::Reasoning {
                    text: value,
                    summary,
                    ..
                } => {
                    text(C::Reasoning, format!("{path}.text"), value, f);
                    for (k, summary) in summary.iter_mut().enumerate() {
                        text(C::Reasoning, format!("{path}.summary[{k}]"), summary, f);
                    }
                }
            }
        }
    }
}
fn tools(tools: &mut [TraceToolSpec], f: &mut dyn FnMut(&mut ContentField<'_>)) {
    for (i, tool) in tools.iter_mut().enumerate() {
        text(
            C::ToolDefinitions,
            format!("tool_definitions[{i}].description"),
            &mut tool.description,
            f,
        );
        json(
            C::ToolDefinitions,
            format!("tool_definitions[{i}].input_schema"),
            &mut tool.input_schema,
            f,
        );
        json(
            C::ToolDefinitions,
            format!("tool_definitions[{i}].output_schema"),
            &mut tool.output_schema,
            f,
        );
    }
}
impl TraceEvent {
    pub(crate) fn request_mut(&mut self) -> Option<&mut TraceLlmRequest> {
        match self {
            Self::LlmCallStarted { request }
            | Self::LlmCallCompleted {
                model: _,
                request,
                response_metadata: _,
                response: _,
                usage: _,
                provider_usage: _,
                stream_summary: _,
                attempts: _,
            }
            | Self::LlmCallFailed {
                model: _,
                request,
                error: _,
                stream_summary: _,
                attempts: _,
            } => Some(request),
            _ => None,
        }
    }
    /// The one exhaustive traversal, shared by policy, hook and bounds.
    pub(crate) fn visit_content(&mut self, f: &mut dyn FnMut(&mut ContentField<'_>)) {
        match self {
            Self::AttachmentDegraded {
                label,
                attachment_id: _,
                media_type: _,
                position: _,
                reason: _,
            } => json(C::InputMessages, "attachment.label", label, f),
            Self::CompositionChanged {
                rendered_system_prompt,
                tool_schemas,
                fingerprint: _,
            } => {
                text(
                    C::SystemInstructions,
                    "system_instructions",
                    rendered_system_prompt,
                    f,
                );
                tools(tool_schemas, f);
            }
            Self::LlmCallStarted { request: _ } => {}
            Self::LlmCallCompleted {
                model: _,
                request: req,
                response,
                response_metadata,
                usage: _,
                provider_usage: _,
                stream_summary: _,
                attempts: _,
            } => {
                request(req, f);
                text(
                    C::OutputMessages,
                    "output_messages.text",
                    &mut response.text,
                    f,
                );
                for (i, part) in response.parts.iter_mut().flatten().enumerate() {
                    let path = format!("output_messages.parts[{i}]");
                    match part {
                        lash_sansio::llm::types::LlmOutputPart::Text { text: value, .. } => {
                            text(C::OutputMessages, path, value, f)
                        }
                        lash_sansio::llm::types::LlmOutputPart::Reasoning {
                            text: value,
                            replay,
                        } => {
                            text(C::Reasoning, path.clone(), value, f);
                            if let Some(replay) = replay {
                                for (j, summary) in replay.summary.iter_mut().enumerate() {
                                    text(C::Reasoning, format!("{path}.summary[{j}]"), summary, f);
                                }
                            }
                        }
                        lash_sansio::llm::types::LlmOutputPart::ToolCall { input_json, .. } => {
                            let mut value = serde_json::from_str::<Value>(input_json)
                                .unwrap_or_else(|_| Value::String(input_json.clone()));
                            json(C::OutputMessages, path, &mut value, f);
                            *input_json = value.to_string();
                        }
                    }
                }
                json(
                    C::ResponseMetadata,
                    "response_metadata",
                    response_metadata,
                    f,
                );
            }
            Self::LlmCallFailed {
                model: _,
                request: req,
                error: _,
                stream_summary: _,
                attempts: _,
            } => request(req, f),
            Self::LlmAttemptCompleted {
                response_metadata,
                attempt: _,
                observation: _,
            } => json(
                C::ResponseMetadata,
                "response_metadata",
                response_metadata,
                f,
            ),
            Self::ProviderEvent { event } => json(
                C::ProviderPayloads,
                "provider_payloads",
                &mut event.raw_json,
                f,
            ),
            Self::EffectEnvelopeDiff { event } => {
                for (i, entry) in event.divergent_paths.iter_mut().enumerate() {
                    if let TraceEffectEnvelopeDiffValue::Present { value_json, .. } =
                        &mut entry.recorded
                    {
                        json(
                            C::ProviderPayloads,
                            format!("diff[{i}].recorded"),
                            value_json,
                            f,
                        );
                    }
                    if let TraceEffectEnvelopeDiffValue::Present { value_json, .. } =
                        &mut entry.reconstructed
                    {
                        json(
                            C::ProviderPayloads,
                            format!("diff[{i}].reconstructed"),
                            value_json,
                            f,
                        );
                    }
                }
            }
            Self::RuntimeStreamEvent { event } => match &mut event.payload {
                TraceRuntimeStreamPayload::Block { event, raw_text } => {
                    let category =
                        if event.kind() == lash_sansio::llm::types::StreamBlockKind::Reasoning {
                            C::Reasoning
                        } else {
                            C::OutputMessages
                        };
                    json(category, "stream.raw_text", raw_text, f);
                    match event {
                        lash_sansio::llm::types::StreamBlockEvent::Started { .. } => {}
                        lash_sansio::llm::types::StreamBlockEvent::Delta {
                            text: value, ..
                        }
                        | lash_sansio::llm::types::StreamBlockEvent::Completed {
                            text: value,
                            ..
                        } => text(category, "stream.text", value, f),
                    }
                }
                TraceRuntimeStreamPayload::TextPart { text: value, .. } => {
                    text(C::OutputMessages, "stream.text", value, f)
                }
                TraceRuntimeStreamPayload::ReasoningPart { text: value, .. } => {
                    text(C::Reasoning, "stream.reasoning", value, f)
                }
                TraceRuntimeStreamPayload::ToolCallPart { input_json, .. } => {
                    json(C::OutputMessages, "stream.arguments", input_json, f)
                }
                TraceRuntimeStreamPayload::Usage { .. } => {}
            },
            Self::ToolCallStarted {
                args,
                call_id: _,
                provider_call_id: _,
                name: _,
                issuing_node_id: _,
            } => json(C::ToolArguments, "tool_arguments", args, f),
            Self::ToolCallCompleted {
                args,
                output,
                attempts,
                call_id: _,
                provider_call_id: _,
                name: _,
                duration_ms: _,
                issuing_node_id: _,
            } => {
                json(C::ToolArguments, "tool_arguments", args, f);
                match &mut output.outcome {
                    TraceToolCallOutcome::Success(value) => {
                        json(C::ToolResults, "tool_results", value, f)
                    }
                    TraceToolCallOutcome::Failure(value)
                    | TraceToolCallOutcome::Cancelled(value) => {
                        let classification: Vec<_> = [
                            "class",
                            "code",
                            "source",
                            "suggested_delay_ms",
                            "origin",
                            "forced",
                        ]
                        .into_iter()
                        .filter_map(|key| value.get(key).cloned().map(|v| (key.to_owned(), v)))
                        .collect();
                        json(C::ToolResults, "tool_results", value, f);
                        if !classification.is_empty() && !value.is_object() {
                            *value = Value::Object(Default::default());
                        }
                        if let Some(object) = value.as_object_mut() {
                            object.extend(classification);
                        }
                    }
                    TraceToolCallOutcome::Discarded => {}
                }
                json(
                    C::ToolResults,
                    "tool_results.control",
                    &mut output.control,
                    f,
                );
                if let Some(attempts) = attempts {
                    for (i, attempt) in attempts.iter_mut().enumerate() {
                        match &mut attempt.detail {
                            TraceRetryAttemptDetail::Tool { outcome } => match outcome {
                                TraceToolAttemptOutcome::Failed { message, .. }
                                | TraceToolAttemptOutcome::Cancelled { message, .. } => text(
                                    C::ToolResults,
                                    format!("tool_results.attempts[{i}].message"),
                                    message,
                                    f,
                                ),
                                TraceToolAttemptOutcome::Completed => {}
                            },
                        }
                    }
                }
            }
            Self::ExecCodeStarted {
                language: _,
                code_chars: _,
            } => {}
            Self::ExecCodeCompleted {
                trailing_discarded: _,
                code,
                output,
                error,
                terminal_finish,
                language: _,
                started_at_ms: _,
                duration_ms: _,
                output_chars: _,
                observation_count: _,
                observation_projections: _,
                tool_calls: _,
            } => {
                text(C::Code, "code", code, f);
                text(C::CodeOutput, "code_output", output, f);
                if let Some(error) = error {
                    text(
                        C::CodeOutput,
                        "code_output.error.message",
                        &mut error.message,
                        f,
                    );
                    for (i, feedback) in error.feedback.iter_mut().enumerate() {
                        json(
                            C::CodeOutput,
                            format!("code_output.error.feedback[{i}].repairs"),
                            &mut feedback.repairs,
                            f,
                        );
                    }
                    if let Some(admission) = error.schema_admission.as_deref_mut() {
                        match admission {
                            lash_sansio::SchemaAdmissionError::Compilation { message, .. } => {
                                text(C::CodeOutput, "code_output.error.schema", message, f)
                            }
                            lash_sansio::SchemaAdmissionError::NonLocalReference {
                                reference,
                                ..
                            } => text(C::CodeOutput, "code_output.error.reference", reference, f),
                            lash_sansio::SchemaAdmissionError::InvalidKind { .. } => {}
                        }
                    }
                }
                json(
                    C::CodeOutput,
                    "code_output.terminal_finish",
                    terminal_finish,
                    f,
                );
            }
            Self::ExecCodeFailed {
                trailing_discarded: _,
                code,
                error,
                language: _,
                started_at_ms: _,
                duration_ms: _,
                reason: _,
            } => {
                text(C::Code, "code", code, f);
                text(C::CodeOutput, "code_output.error", error, f)
            }
            Self::StoreErrorObserved {
                message,
                operation: _,
                error_class: _,
            } => text(C::DiagnosticsText, "diagnostics.message", message, f),
            Self::ProgramStep {
                outcome,
                step_index: _,
            } => match outcome {
                TraceProgramStepOutcome::Ok => {}
                TraceProgramStepOutcome::Failure { diagnostic } => {
                    text(C::DiagnosticsText, "diagnostics.program", diagnostic, f)
                }
            },
            Self::ProtocolStep {
                payload,
                plugin_id: _,
            } => json(C::InputMessages, "protocol.payload", payload, f),
            Self::LanguageExecution { event, language: _ } => match &mut event.payload {
                TraceLanguageExecutionPayload::ExecutionFinished { error, .. } => {
                    json(C::DiagnosticsText, "diagnostics.execution", error, f)
                }
                TraceLanguageExecutionPayload::Node { fact, .. } => match fact {
                    TraceNodeFact::Failed { failure, .. } => match failure {
                        TraceLanguageExecutionFailure::Effect { message, .. }
                        | TraceLanguageExecutionFailure::Runtime { message, .. } => {
                            text(C::DiagnosticsText, "diagnostics.node", message, f)
                        }
                    },
                    TraceNodeFact::Started { .. }
                    | TraceNodeFact::Waiting { .. }
                    | TraceNodeFact::Resumed { .. }
                    | TraceNodeFact::Cancelled
                    | TraceNodeFact::Completed { .. }
                    | TraceNodeFact::BranchSelected { .. }
                    | TraceNodeFact::ChildStarted { .. } => {}
                },
                TraceLanguageExecutionPayload::ExecutionStarted { observation: _ }
                | TraceLanguageExecutionPayload::Ran { segment: _, ran: _ } => {}
            },
            Self::ExecCodeDiscarded {
                trailing_discarded: _,
                code,
                language: _,
                started_at_ms: _,
                duration_ms: _,
            } => text(C::Code, "code", code, f),
            Self::TurnCompleted {
                inference_calls: _,
                tool_calls: _,
                input,
                output,
                inputs: _,
                outcome: _,
            } => {
                messages(input, C::InputMessages, "input_messages", f);
                messages(output, C::OutputMessages, "output_messages", f);
            }
            Self::Custom { payload, name: _ } => json(C::Extension, "extension", payload, f),
            Self::TurnStarted { metadata: _ }
            | Self::PromptBuilt {
                prompt_hash: _,
                prompt_chars: _,
                components: _,
            }
            | Self::PromptCompositionFailed {
                plan: _,
                limits: _,
                error: _,
                elapsed_ms: _,
            }
            | Self::CompactionNeeded {
                used_tokens: _,
                max_context_tokens: _,
                threshold_tokens: _,
            }
            | Self::CompactionStarted {
                source_messages: _,
                instructions_present: _,
            }
            | Self::CompactionCompleted { summary_nodes: _ }
            | Self::PromptViewAttachmentsPruned {
                used_tokens: _,
                max_context_tokens: _,
                pruned_attachments: _,
            }
            | Self::DomainCompleted { completion: _ }
            | Self::ProviderReplayDropped { event: _ }
            | Self::StepBodyStarted { step: _ }
            | Self::ToolCheckConflict {
                plugin_id: _,
                conflict: _,
            }
            | Self::ObservationProjection { projections: _ }
            | Self::ProducerSettled {
                producer: _,
                outcome: _,
            } => {}
        }
    }
    pub(crate) fn apply_policy(&mut self, policy: &ContentPolicy, marks: &mut ContentMarks) {
        if let Self::LlmCallStarted { request } = self {
            request.system_instructions = None;
            request.messages.clear();
            request.output_spec = None;
            for tool in &mut request.tools {
                tool.description.clear();
                tool.input_schema = Value::Null;
                tool.output_schema = Value::Null;
            }
        }
        if let Self::TurnCompleted { input, output, .. } = self {
            if !policy.captures(C::InputMessages) {
                input.clear();
            }
            if !policy.captures(C::OutputMessages) {
                output.clear();
            }
        }
        // Remove reasoning parts, rather than leave empty reasoning in a message.
        if let Some(req) = self.request_mut() {
            if !policy.captures(C::InputMessages) {
                req.messages.clear();
            } else if !policy.captures(C::Reasoning) {
                for message in &mut req.messages {
                    for (i, block) in message.blocks.iter().enumerate() {
                        if matches!(block, TraceContentBlock::Reasoning { .. }) {
                            marks.omitted.push(format!("input_messages.reasoning[{i}]"));
                        }
                    }
                    message
                        .blocks
                        .retain(|block| !matches!(block, TraceContentBlock::Reasoning { .. }));
                }
            }
            if !policy.captures(C::SystemInstructions) {
                req.system_instructions = None;
            }
        }
        if let Self::LlmCallCompleted {
            model: _,
            response,
            request: _,
            response_metadata: _,
            usage: _,
            provider_usage: _,
            stream_summary: _,
            attempts: _,
        } = self
        {
            if !policy.captures(C::OutputMessages) {
                response.text.clear();
                response.parts = None;
            } else if !policy.captures(C::Reasoning)
                && let Some(parts) = &mut response.parts
            {
                for (i, part) in parts.iter().enumerate() {
                    if matches!(
                        part,
                        lash_sansio::llm::types::LlmOutputPart::Reasoning { .. }
                    ) {
                        marks
                            .omitted
                            .push(format!("output_messages.reasoning[{i}]"));
                    }
                }
                parts.retain(|part| {
                    !matches!(
                        part,
                        lash_sansio::llm::types::LlmOutputPart::Reasoning { .. }
                    )
                });
            }
        }
        self.visit_content(&mut |field| {
            if !policy.captures(field.category)
                || (field.category == C::Reasoning
                    && field.path.starts_with("stream.")
                    && !policy.captures(C::OutputMessages))
            {
                marks.omitted.push(field.path.clone());
                field.omit();
            }
        });
        if let Self::ProviderEvent { event } = self
            && !policy.captures(C::ProviderPayloads)
        {
            event.raw_json_omitted_reason = Some(TraceProviderBodyOmission::ContentPolicy);
        }
    }
}
