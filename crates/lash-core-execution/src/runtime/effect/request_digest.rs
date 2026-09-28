//! The journaled form of a model request (FIG-3980).
//!
//! `BeforeLlmCall` and `LlmCall` carry the whole request the turn built: every
//! message of the transcript, the tool catalog and the instructions. Replay
//! validation only needs to know the request is the same one, so the canonical
//! envelope of those two commands journals a digest of that content instead,
//! beside the request's small configuration fields verbatim. A journaled turn
//! therefore stops growing with its transcript, and a replay whose rebuilt
//! request differs in any message, tool or instruction byte still diverges.
//!
//! The digest names the content by count and BLAKE3 over its serde encoding.
//! The encoding is streamed into the hasher, so the transcript is never
//! materialized as a second string.

use std::io;
use std::sync::Arc;

use lash_sansio::core_support::Blake3DomainHasher;
use serde::Serialize;

use crate::llm::types::{LlmOutputSpec, LlmToolChoice};

use super::{
    EffectGroupMembership, RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectInvocation,
};

/// The canonical JSON an envelope journals: the envelope itself, except that a
/// model request's content is replaced by its digest.
/// The two request types share every journaled field's name and type.
macro_rules! journaled_request {
    ($request:expr) => {
        JournaledLlmRequest {
            instructions: $request.instructions.as_ref().map(digest_instructions),
            model: &$request.model,
            messages: ContentDigest::of_items(&$request.messages)?,
            tools: ContentDigest::of_items(&$request.tools)?,
            tool_choice: &$request.tool_choice,
            model_variant: &$request.model_variant,
            model_capability: &$request.model_capability,
            generation: &$request.generation,
            scope: &$request.scope,
            output_spec: &$request.output_spec,
        }
    };
}

pub(super) fn journaled_envelope_json(
    envelope: &RuntimeEffectEnvelope,
) -> Result<String, serde_json::Error> {
    let command = match &envelope.command {
        RuntimeEffectCommand::BeforeLlmCall { request } => JournaledLlmCommand::BeforeLlmCall {
            request: journaled_request!(request),
        },
        RuntimeEffectCommand::LlmCall {
            provider_id,
            request,
        } => JournaledLlmCommand::LlmCall {
            provider_id,
            request: journaled_request!(request),
        },
        _ => return crate::stable_hash::stable_json_string(envelope),
    };
    crate::stable_hash::stable_json_string(&JournaledEnvelope {
        invocation: &envelope.invocation,
        command,
        group: envelope.group.as_deref(),
    })
}

/// [`RuntimeEffectEnvelope`]'s field order and names, over the journaled
/// command.
#[derive(Serialize)]
struct JournaledEnvelope<'a> {
    invocation: &'a RuntimeEffectInvocation,
    command: JournaledLlmCommand<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    group: Option<&'a EffectGroupMembership>,
}

/// [`RuntimeEffectCommand`]'s tag and field names for the two model-request
/// commands.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum JournaledLlmCommand<'a> {
    BeforeLlmCall {
        request: JournaledLlmRequest<'a>,
    },
    LlmCall {
        provider_id: &'a str,
        request: JournaledLlmRequest<'a>,
    },
}

/// A model request's configuration verbatim, and its content by digest.
#[derive(Serialize)]
struct JournaledLlmRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<ContentDigest>,
    model: &'a str,
    messages: ContentDigest,
    tools: ContentDigest,
    tool_choice: &'a LlmToolChoice,
    model_variant: &'a crate::ReasoningSelection,
    model_capability: &'a crate::ModelCapability,
    generation: &'a crate::GenerationOptions,
    scope: &'a crate::LlmRequestScope,
    output_spec: &'a Option<LlmOutputSpec>,
}

/// How much content there is (items, or bytes for text) and its BLAKE3.
#[derive(Serialize)]
struct ContentDigest {
    len: usize,
    blake3: String,
}

impl ContentDigest {
    fn of_items<T: Serialize>(items: &[T]) -> Result<Self, serde_json::Error> {
        let mut writer = HashWriter(Blake3DomainHasher::new("lash-llm-request-content/v1"));
        serde_json::to_writer(&mut writer, items)?;
        Ok(Self {
            len: items.len(),
            blake3: writer.0.finalize_hex(),
        })
    }
}

fn digest_instructions(instructions: &Arc<str>) -> ContentDigest {
    let mut hasher = Blake3DomainHasher::new("lash-llm-request-content/v1");
    hasher.update(instructions.as_bytes());
    ContentDigest {
        len: instructions.len(),
        blake3: hasher.finalize_hex(),
    }
}

/// Streams serde's output into the hasher.
struct HashWriter(Blake3DomainHasher);

impl io::Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::super::validation::validate_replayed_effect_envelope;
    use super::*;
    use crate::llm::types::{LlmMessage, LlmRole};

    fn request(messages: Vec<LlmMessage>) -> super::super::LlmRequestSpec {
        super::super::LlmRequestSpec {
            instructions: Some(Arc::from("be brief")),
            model: "digest-model".to_string(),
            messages,
            tools: Arc::new(Vec::new()),
            tool_choice: LlmToolChoice::None,
            model_variant: Default::default(),
            model_capability: Default::default(),
            generation: Default::default(),
            scope: crate::LlmRequestScope::new("session", "session:frame", "request"),
            output_spec: None,
        }
    }

    fn llm_call(messages: Vec<LlmMessage>) -> RuntimeEffectEnvelope {
        RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                crate::EffectAddress::new(
                    crate::ExecutionScope::turn("session", "turn"),
                    "llm-call:digest",
                )
                .expect("valid address"),
                crate::RuntimeAttribution::for_turn("session", "turn", 0, 0),
                "llm-call:digest",
            ),
            RuntimeEffectCommand::LlmCall {
                provider_id: "digest-provider".to_string(),
                request: Box::new(request(messages)),
            },
        )
    }

    fn transcript(text: &str) -> Vec<LlmMessage> {
        vec![
            LlmMessage::text(LlmRole::User, text),
            LlmMessage::text(LlmRole::Assistant, format!("echo: {text}")),
        ]
    }

    #[test]
    fn a_model_request_journals_the_same_bytes_however_long_its_transcript() {
        let short = llm_call(transcript("hi"))
            .canonical_form()
            .expect("canonical");
        let long = llm_call(transcript(&"x".repeat(100_000)))
            .canonical_form()
            .expect("canonical");
        assert_eq!(short.json().len(), long.json().len());
        assert!(
            !long.json().contains("xxxx"),
            "no message text is journaled"
        );
        let journaled: Value = serde_json::from_str(long.json()).expect("journaled json");
        assert_eq!(journaled["command"]["type"], "llm_call");
        assert_eq!(journaled["command"]["provider_id"], "digest-provider");
        assert_eq!(journaled["command"]["request"]["model"], "digest-model");
        assert_eq!(journaled["command"]["request"]["messages"]["len"], 2);
    }

    #[test]
    fn a_replay_whose_transcript_differs_diverges_at_the_message_digest() {
        let recorded = llm_call(transcript("first"))
            .canonical_form()
            .expect("canonical");
        let replayed = llm_call(transcript("fist"))
            .canonical_form()
            .expect("canonical");
        let error = validate_replayed_effect_envelope(
            &recorded,
            &replayed,
            crate::RuntimeErrorCode::EffectReplayDivergence,
            None,
        )
        .expect_err("another transcript diverges");
        assert_eq!(
            error.summary.expect("summary").first_divergent_paths,
            ["command.request.messages.blake3"]
        );
        let same = llm_call(transcript("first"))
            .canonical_form()
            .expect("canonical");
        assert!(
            validate_replayed_effect_envelope(
                &recorded,
                &same,
                crate::RuntimeErrorCode::EffectReplayDivergence,
                None,
            )
            .is_ok(),
            "the same transcript replays"
        );
    }
}
