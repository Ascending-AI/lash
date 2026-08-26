//! Shared types and helpers used by protocol drivers. Concrete drivers and
//! their prompts live in protocol plugin crates; this module exposes the common
//! turn-driver surface:
//!
//! - [`TurnDriverConfig`], [`TurnDriverPreamble`] — the per-turn configuration
//!   driver-plugins populate.
//! - A small helper layer (`normalized_response_parts`, `reasoning_part`,
//!   `append_assistant_text_part`) that protocol drivers reuse for building
//!   assistant messages.

use std::sync::Arc;

use crate::PromptContribution;
use crate::PromptFingerprint;
use crate::llm::types::{
    LlmOutputPart, LlmResponse, LlmToolSpec, ProviderReasoningReplay, ResponseTextMeta,
};
use crate::sansio::{
    ChatContextProjector, ContextProjector, ProtocolDriverHandle, TurnProtocol, UnitTurnProtocol,
};
use crate::session_model::Part;

pub type TurnLimitFinalMessage =
    Arc<dyn Fn(String, usize) -> crate::Message + Send + Sync + 'static>;

#[derive(Clone)]
pub struct TurnDriverConfig<M: TurnProtocol = UnitTurnProtocol> {
    pub protocol: Arc<dyn ProtocolDriverHandle<M>>,
    pub projector: Arc<dyn ContextProjector<M>>,
    pub sync_execution_environment: bool,
    pub turn_limit_final_message: TurnLimitFinalMessage,
}

impl<M: TurnProtocol> TurnDriverConfig<M> {
    pub fn chat(
        protocol: Arc<dyn ProtocolDriverHandle<M>>,
        sync_execution_environment: bool,
        turn_limit_final_message: TurnLimitFinalMessage,
    ) -> Self {
        Self {
            protocol,
            projector: Arc::new(ChatContextProjector),
            sync_execution_environment,
            turn_limit_final_message,
        }
    }
}

#[derive(Clone)]
pub struct TurnDriverPreamble<M: TurnProtocol = UnitTurnProtocol> {
    pub config: TurnDriverConfig<M>,
    pub tool_specs: Arc<Vec<LlmToolSpec>>,
    pub tool_names: Arc<Vec<String>>,
    pub tool_names_fingerprint: PromptFingerprint,
    pub execution_prompt: Arc<str>,
    pub prompt_contributions: Vec<PromptContribution>,
}

/// Convert a raw `LlmResponse` into the visible stream of `LlmOutputPart`s that
/// downstream code can iterate.
pub fn normalized_response_parts(llm_response: &LlmResponse) -> Vec<LlmOutputPart> {
    visible_response_parts(llm_response.parts.clone())
}

/// Apply provider phase semantics to response parts. If a Responses-family
/// provider emits both `commentary` and `final_answer` text, the latter is the
/// final assistant prose and commentary is retained only in the raw provider
/// response, not in user-visible prose projection.
pub fn visible_response_parts(parts: Vec<LlmOutputPart>) -> Vec<LlmOutputPart> {
    let has_final_answer = parts.iter().any(|part| match part {
        LlmOutputPart::Text {
            text,
            response_meta: Some(meta),
        } => !text.is_empty() && meta.is_final_answer_phase(),
        _ => false,
    });
    if !has_final_answer {
        return parts;
    }
    parts
        .into_iter()
        .filter(|part| match part {
            LlmOutputPart::Text {
                response_meta: Some(meta),
                ..
            } => !meta.is_commentary_phase(),
            _ => true,
        })
        .collect()
}

pub fn visible_response_text_from_parts(parts: &[LlmOutputPart]) -> String {
    let has_final_answer = parts.iter().any(|part| match part {
        LlmOutputPart::Text {
            text,
            response_meta: Some(meta),
        } => !text.is_empty() && meta.is_final_answer_phase(),
        _ => false,
    });
    let mut full_text = String::new();
    for part in parts {
        let LlmOutputPart::Text {
            text,
            response_meta,
        } = part
        else {
            continue;
        };
        if has_final_answer
            && response_meta
                .as_ref()
                .is_some_and(ResponseTextMeta::is_commentary_phase)
        {
            continue;
        }
        full_text.push_str(text);
    }
    full_text
}

/// Build a Reasoning `Part` from a reasoning item. `meta` is Some when
/// the item carries provider replay metadata; None for display-only
/// summaries.
pub fn reasoning_part(
    asst_id: &str,
    index: usize,
    text: String,
    meta: Option<ProviderReasoningReplay>,
) -> Part {
    Part::reasoning(format!("{asst_id}.p{index}"), text, meta)
}

/// Append a streamed text part to the running assistant text, inserting
/// the right number of blank lines so consecutive parts don't glue
/// together.
pub fn append_assistant_text_part(out: &mut String, next: &str) {
    if out.is_empty() {
        out.push_str(next);
        return;
    }

    let prev_trailing_newlines = out.chars().rev().take_while(|ch| *ch == '\n').count();
    let next_leading_newlines = next.chars().take_while(|ch| *ch == '\n').count();
    let total_boundary_newlines = prev_trailing_newlines + next_leading_newlines;
    if total_boundary_newlines < 2 {
        out.push_str(&"\n".repeat(2 - total_boundary_newlines));
    }

    out.push_str(next);
}
