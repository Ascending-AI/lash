//! Closed vocabulary and handling classes for Responses stream events.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ResponsesStreamEvent {
    EmptyPing,
    ResponseAudioDelta,
    ResponseAudioDone,
    ResponseAudioTranscriptDelta,
    ResponseAudioTranscriptDone,
    ResponseCodeInterpreterCallCodeDelta,
    ResponseCodeInterpreterCallCodeDone,
    ResponseCodeInterpreterCallCompleted,
    ResponseCodeInterpreterCallInProgress,
    ResponseCodeInterpreterCallInterpreting,
    ResponseCompleted,
    ResponseContentPartAdded,
    ResponseContentPartDone,
    ResponseCreated,
    ResponseCustomToolCallInputDelta,
    ResponseCustomToolCallInputDone,
    ResponseDebug,
    ResponseDone,
    ResponseFailed,
    ResponseFileSearchCallCompleted,
    ResponseFileSearchCallInProgress,
    ResponseFileSearchCallSearching,
    ResponseFunctionCallArgumentsDelta,
    ResponseFunctionCallArgumentsDone,
    ResponseImageGenerationCallCompleted,
    ResponseImageGenerationCallGenerating,
    ResponseImageGenerationCallInProgress,
    ResponseImageGenerationCallPartialImage,
    ResponseInProgress,
    ResponseIncomplete,
    ResponseMcpCallArgumentsDelta,
    ResponseMcpCallArgumentsDone,
    ResponseMcpCallCompleted,
    ResponseMcpCallFailed,
    ResponseMcpCallInProgress,
    ResponseMcpListToolsCompleted,
    ResponseMcpListToolsFailed,
    ResponseMcpListToolsInProgress,
    ResponseOutputItemAdded,
    ResponseOutputItemDone,
    ResponseOutputTextAnnotationAdded,
    ResponseOutputTextDelta,
    ResponseOutputTextDone,
    ResponseQueued,
    ResponseReasoningSummaryPartAdded,
    ResponseReasoningSummaryPartDone,
    ResponseReasoningSummaryTextDelta,
    ResponseReasoningSummaryTextDone,
    ResponseReasoningTextDelta,
    ResponseReasoningTextDone,
    ResponseRefusalDelta,
    ResponseRefusalDone,
    ResponseWebSearchCallCompleted,
    ResponseWebSearchCallInProgress,
    ResponseWebSearchCallSearching,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ResponsesStreamEventClass {
    Terminal,
    Structural,
    EvidenceOnly,
    Lifecycle,
    Unknown,
}

impl ResponsesStreamEvent {
    pub(super) fn parse(name: &str) -> Self {
        match name {
            "" => Self::EmptyPing,
            "response.audio.delta" => Self::ResponseAudioDelta,
            "response.audio.done" => Self::ResponseAudioDone,
            "response.audio.transcript.delta" => Self::ResponseAudioTranscriptDelta,
            "response.audio.transcript.done" => Self::ResponseAudioTranscriptDone,
            "response.code_interpreter_call_code.delta" => {
                Self::ResponseCodeInterpreterCallCodeDelta
            }
            "response.code_interpreter_call_code.done" => Self::ResponseCodeInterpreterCallCodeDone,
            "response.code_interpreter_call.completed" => {
                Self::ResponseCodeInterpreterCallCompleted
            }
            "response.code_interpreter_call.in_progress" => {
                Self::ResponseCodeInterpreterCallInProgress
            }
            "response.code_interpreter_call.interpreting" => {
                Self::ResponseCodeInterpreterCallInterpreting
            }
            "response.completed" => Self::ResponseCompleted,
            "response.content_part.added" => Self::ResponseContentPartAdded,
            "response.content_part.done" => Self::ResponseContentPartDone,
            "response.created" => Self::ResponseCreated,
            "response.custom_tool_call_input.delta" => Self::ResponseCustomToolCallInputDelta,
            "response.custom_tool_call_input.done" => Self::ResponseCustomToolCallInputDone,
            "response.debug" => Self::ResponseDebug,
            "response.done" => Self::ResponseDone,
            "response.failed" => Self::ResponseFailed,
            "response.file_search_call.completed" => Self::ResponseFileSearchCallCompleted,
            "response.file_search_call.in_progress" => Self::ResponseFileSearchCallInProgress,
            "response.file_search_call.searching" => Self::ResponseFileSearchCallSearching,
            "response.function_call_arguments.delta" => Self::ResponseFunctionCallArgumentsDelta,
            "response.function_call_arguments.done" => Self::ResponseFunctionCallArgumentsDone,
            "response.image_generation_call.completed" => {
                Self::ResponseImageGenerationCallCompleted
            }
            "response.image_generation_call.generating" => {
                Self::ResponseImageGenerationCallGenerating
            }
            "response.image_generation_call.in_progress" => {
                Self::ResponseImageGenerationCallInProgress
            }
            "response.image_generation_call.partial_image" => {
                Self::ResponseImageGenerationCallPartialImage
            }
            "response.in_progress" => Self::ResponseInProgress,
            "response.incomplete" => Self::ResponseIncomplete,
            "response.mcp_call_arguments.delta" => Self::ResponseMcpCallArgumentsDelta,
            "response.mcp_call_arguments.done" => Self::ResponseMcpCallArgumentsDone,
            "response.mcp_call.completed" => Self::ResponseMcpCallCompleted,
            "response.mcp_call.failed" => Self::ResponseMcpCallFailed,
            "response.mcp_call.in_progress" => Self::ResponseMcpCallInProgress,
            "response.mcp_list_tools.completed" => Self::ResponseMcpListToolsCompleted,
            "response.mcp_list_tools.failed" => Self::ResponseMcpListToolsFailed,
            "response.mcp_list_tools.in_progress" => Self::ResponseMcpListToolsInProgress,
            "response.output_item.added" => Self::ResponseOutputItemAdded,
            "response.output_item.done" => Self::ResponseOutputItemDone,
            "response.output_text.annotation.added" => Self::ResponseOutputTextAnnotationAdded,
            "response.output_text.delta" => Self::ResponseOutputTextDelta,
            "response.output_text.done" => Self::ResponseOutputTextDone,
            "response.queued" => Self::ResponseQueued,
            "response.reasoning_summary_part.added" => Self::ResponseReasoningSummaryPartAdded,
            "response.reasoning_summary_part.done" => Self::ResponseReasoningSummaryPartDone,
            "response.reasoning_summary_text.delta" => Self::ResponseReasoningSummaryTextDelta,
            "response.reasoning_summary_text.done" => Self::ResponseReasoningSummaryTextDone,
            "response.reasoning_text.delta" => Self::ResponseReasoningTextDelta,
            "response.reasoning_text.done" => Self::ResponseReasoningTextDone,
            "response.refusal.delta" => Self::ResponseRefusalDelta,
            "response.refusal.done" => Self::ResponseRefusalDone,
            "response.web_search_call.completed" => Self::ResponseWebSearchCallCompleted,
            "response.web_search_call.in_progress" => Self::ResponseWebSearchCallInProgress,
            "response.web_search_call.searching" => Self::ResponseWebSearchCallSearching,
            _ => Self::Unknown,
        }
    }

    pub(super) fn handling_class(self) -> ResponsesStreamEventClass {
        match self {
            Self::ResponseCompleted
            | Self::ResponseDone
            | Self::ResponseFailed
            | Self::ResponseIncomplete => ResponsesStreamEventClass::Terminal,
            Self::ResponseFunctionCallArgumentsDelta
            | Self::ResponseFunctionCallArgumentsDone
            | Self::ResponseOutputItemAdded
            | Self::ResponseOutputItemDone
            | Self::ResponseOutputTextDelta
            | Self::ResponseOutputTextDone
            | Self::ResponseReasoningSummaryPartAdded
            | Self::ResponseReasoningSummaryPartDone
            | Self::ResponseReasoningSummaryTextDelta
            | Self::ResponseReasoningSummaryTextDone => ResponsesStreamEventClass::Structural,
            Self::ResponseCreated | Self::ResponseInProgress | Self::ResponseQueued => {
                ResponsesStreamEventClass::Lifecycle
            }
            Self::EmptyPing
            | Self::ResponseAudioDelta
            | Self::ResponseAudioDone
            | Self::ResponseAudioTranscriptDelta
            | Self::ResponseAudioTranscriptDone
            | Self::ResponseCodeInterpreterCallCodeDelta
            | Self::ResponseCodeInterpreterCallCodeDone
            | Self::ResponseCodeInterpreterCallCompleted
            | Self::ResponseCodeInterpreterCallInProgress
            | Self::ResponseCodeInterpreterCallInterpreting
            | Self::ResponseContentPartAdded
            | Self::ResponseContentPartDone
            | Self::ResponseCustomToolCallInputDelta
            | Self::ResponseCustomToolCallInputDone
            | Self::ResponseDebug
            | Self::ResponseFileSearchCallCompleted
            | Self::ResponseFileSearchCallInProgress
            | Self::ResponseFileSearchCallSearching
            | Self::ResponseImageGenerationCallCompleted
            | Self::ResponseImageGenerationCallGenerating
            | Self::ResponseImageGenerationCallInProgress
            | Self::ResponseImageGenerationCallPartialImage
            | Self::ResponseMcpCallArgumentsDelta
            | Self::ResponseMcpCallArgumentsDone
            | Self::ResponseMcpCallCompleted
            | Self::ResponseMcpCallFailed
            | Self::ResponseMcpCallInProgress
            | Self::ResponseMcpListToolsCompleted
            | Self::ResponseMcpListToolsFailed
            | Self::ResponseMcpListToolsInProgress
            | Self::ResponseOutputTextAnnotationAdded
            | Self::ResponseReasoningTextDelta
            | Self::ResponseReasoningTextDone
            | Self::ResponseRefusalDelta
            | Self::ResponseRefusalDone
            | Self::ResponseWebSearchCallCompleted
            | Self::ResponseWebSearchCallInProgress
            | Self::ResponseWebSearchCallSearching => ResponsesStreamEventClass::EvidenceOnly,
            Self::Unknown => ResponsesStreamEventClass::Unknown,
        }
    }

    pub(super) fn is_terminal(self) -> bool {
        self.handling_class() == ResponsesStreamEventClass::Terminal
    }
}
