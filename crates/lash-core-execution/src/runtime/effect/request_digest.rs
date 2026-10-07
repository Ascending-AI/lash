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

/// version_surface = "coexist"
/// version_guard(items(LASH_LLM_REQUEST_CONTENT_DOMAIN_VERSION, digest_instructions, of_items))
const LASH_LLM_REQUEST_CONTENT_DOMAIN_VERSION: &str = "lash-llm-request-content/v1";

use std::io;
use std::sync::Arc;

use lash_sansio::core_support::Blake3DomainHasher;
use serde::Serialize;

use crate::llm::types::{LlmOutputSpec, LlmToolChoice};

use super::{RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectInvocation};

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
            attachment_acceptance: $request.attachment_acceptance.as_ref(),
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
        RuntimeEffectCommand::LlmCall { request } => JournaledLlmCommand::LlmCall {
            request: journaled_request!(request),
        },
        // Trace provenance rides the command beside its business payload
        // and takes no part in the envelope's identity: a command that
        // carries some is journaled and hashed without it, so a retry under
        // another context reconstructs the same envelope.
        command => {
            return match command.without_trace_provenance() {
                Some(command) => {
                    crate::stable_hash::stable_json_string(&JournaledBusinessEnvelope {
                        invocation: &envelope.invocation,
                        command: &command,
                    })
                }
                None => crate::stable_hash::stable_json_string(envelope),
            };
        }
    };
    crate::stable_hash::stable_json_string(&JournaledEnvelope {
        invocation: &envelope.invocation,
        command,
    })
}

/// [`RuntimeEffectEnvelope`]'s field order and names, over a command with
/// its trace provenance projected out.
#[derive(Serialize)]
struct JournaledBusinessEnvelope<'a> {
    invocation: &'a RuntimeEffectInvocation,
    command: &'a RuntimeEffectCommand,
}

/// [`RuntimeEffectEnvelope`]'s field order and names, over the journaled
/// command.
#[derive(Serialize)]
struct JournaledEnvelope<'a> {
    invocation: &'a RuntimeEffectInvocation,
    command: JournaledLlmCommand<'a>,
}

/// [`RuntimeEffectCommand`]'s tag and field names for the two model-request
/// commands.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum JournaledLlmCommand<'a> {
    BeforeLlmCall { request: JournaledLlmRequest<'a> },
    LlmCall { request: JournaledLlmRequest<'a> },
}

/// A model request's recorded configuration and content digest.
#[derive(Serialize)]
struct JournaledLlmRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<ContentDigest>,
    model: &'a crate::LlmProfileConfig,
    messages: ContentDigest,
    tools: ContentDigest,
    tool_choice: &'a LlmToolChoice,
    #[serde(skip_serializing_if = "crate::provider::AttachmentCapabilitySnapshot::is_empty")]
    attachment_acceptance: &'a crate::provider::AttachmentCapabilitySnapshot,
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
        let mut writer = HashWriter(Blake3DomainHasher::new(
            LASH_LLM_REQUEST_CONTENT_DOMAIN_VERSION,
        ));
        serde_json::to_writer(&mut writer, items)?;
        Ok(Self {
            len: items.len(),
            blake3: writer.0.finalize_hex(),
        })
    }
}

fn digest_instructions(instructions: &Arc<str>) -> ContentDigest {
    let mut hasher = Blake3DomainHasher::new(LASH_LLM_REQUEST_CONTENT_DOMAIN_VERSION);
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
            model: lash_sansio::llm_profile::LlmProfileConfig::new(
                lash_sansio::llm_profile::RecordedLlmProfile::mint(
                    lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                    lash_sansio::llm_profile::LlmProfileMetadata::builder(
                        "digest-model".to_string(),
                    )
                    .context_window_tokens(128_000)
                    .capability(Default::default())
                    .extra_body(Default::default())
                    .request_defaults(Default::default())
                    .build()
                    .expect("valid profile"),
                ),
            )
            .with_reasoning(Default::default()),
            messages,
            tools: Arc::new(Vec::new()),
            tool_choice: LlmToolChoice::None,
            attachment_acceptance: Default::default(),
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
                request: {
                    let mut request = Box::new(request(messages));
                    request.model.model = lash_sansio::llm_profile::RecordedLlmProfile::mint(
                        crate::LlmProfileKey::new("digest-model-key"),
                        request.model.metadata().clone(),
                    );
                    request
                },
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
        assert_eq!(
            journaled["command"]["request"]["model"]["model"]["key"],
            "digest-model-key"
        );
        assert_eq!(
            journaled["command"]["request"]["model"]["model"]["metadata"]["wire_model"],
            "digest-model"
        );
        assert_eq!(journaled["command"]["request"]["messages"]["len"], 2);
    }

    fn direct(profile_key: &str) -> RuntimeEffectEnvelope {
        let mut envelope = llm_call(transcript("direct"));
        envelope.command = RuntimeEffectCommand::Direct {
            request: {
                let mut request = Box::new(request(transcript("direct")));
                request.model.model = lash_sansio::llm_profile::RecordedLlmProfile::mint(
                    crate::LlmProfileKey::new(profile_key),
                    request.model.metadata().clone(),
                );
                request
            },
            usage_source: "direct-source".to_string(),
        };
        envelope
    }

    /// FIG-4405: a direct effect's envelope carries the recorded model key
    /// beside its request's wire model, and a replay that names another key
    /// for the same wire model diverges at the key.
    #[test]
    fn a_direct_envelope_journals_its_profile_key_and_another_key_diverges() {
        let recorded = direct("direct-key-a").canonical_form().expect("canonical");
        let journaled: Value = serde_json::from_str(recorded.json()).expect("journaled json");
        assert_eq!(journaled["command"]["type"], "direct");
        assert_eq!(
            journaled["command"]["request"]["model"]["model"]["key"],
            "direct-key-a"
        );
        assert_eq!(
            journaled["command"]["request"]["model"]["model"]["metadata"]["wire_model"],
            "digest-model"
        );
        assert_eq!(journaled["command"]["usage_source"], "direct-source");

        let other_key = direct("direct-key-b").canonical_form().expect("canonical");
        let error = validate_replayed_effect_envelope(
            &recorded,
            &other_key,
            crate::RuntimeErrorCode::EffectReplayDivergence,
            None,
        )
        .expect_err("another key for the same wire model diverges");
        assert_eq!(
            error.summary.expect("summary").first_divergent_paths,
            ["command.request.model.model.key"]
        );
        let same = direct("direct-key-a").canonical_form().expect("canonical");
        assert!(
            validate_replayed_effect_envelope(
                &recorded,
                &same,
                crate::RuntimeErrorCode::EffectReplayDivergence,
                None,
            )
            .is_ok(),
            "the same key replays"
        );
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

    #[test]
    fn extra_body_is_journaled_in_both_request_forms_and_changes_diverge() {
        for before_call in [false, true] {
            let make = |entries: &[(&str, i32)]| {
                let mut spec = request(transcript("same"));
                spec.model.metadata_mut().extra_body = entries
                    .iter()
                    .map(|(key, value)| ((*key).to_string(), serde_json::json!(value)))
                    .collect();
                let mut envelope = llm_call(transcript("same"));
                envelope.command = if before_call {
                    RuntimeEffectCommand::BeforeLlmCall {
                        request: Box::new(spec.into_request(None, None)),
                    }
                } else {
                    RuntimeEffectCommand::LlmCall {
                        request: {
                            let mut request = Box::new(spec);
                            request.model.model =
                                lash_sansio::llm_profile::RecordedLlmProfile::mint(
                                    crate::LlmProfileKey::new("digest-model-key"),
                                    request.model.metadata().clone(),
                                );
                            request
                        },
                    }
                };
                envelope.canonical_form().expect("canonical")
            };
            let recorded = make(&[("b", 2), ("a", 1)]);
            let same = make(&[("a", 1), ("b", 2)]);
            assert!(
                validate_replayed_effect_envelope(
                    &recorded,
                    &same,
                    crate::RuntimeErrorCode::EffectReplayDivergence,
                    None
                )
                .is_ok()
            );
            let changed = make(&[("a", 1), ("b", 3)]);
            let error = validate_replayed_effect_envelope(
                &recorded,
                &changed,
                crate::RuntimeErrorCode::EffectReplayDivergence,
                None,
            )
            .expect_err("changed body diverges");
            assert_eq!(
                error.summary.expect("summary").first_divergent_paths,
                ["command.request.model.model.metadata.extra_body.b"]
            );
            let journaled: Value = serde_json::from_str(recorded.json()).expect("json");
            assert_eq!(
                journaled["command"]["request"]["model"]["model"]["metadata"]["extra_body"],
                serde_json::json!({"a":1,"b":2})
            );
            assert!(
                journaled["command"]["request"]
                    .get("extra_headers")
                    .is_none()
            );
        }
    }

    /// The context of one producer: a sampled span of its own trace.
    fn producer(id: u8) -> lash_trace::TraceCarrier {
        lash_trace::TraceCarrier::parse_w3c(&format!("00-{id:032x}-{id:016x}-01"), None)
            .expect("a valid trace context")
    }

    fn process_effect(command: crate::ProcessCommand) -> RuntimeEffectEnvelope {
        RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                crate::EffectAddress::new(
                    crate::ExecutionScope::session_operation("session", "host-op"),
                    "process:provenance",
                )
                .expect("valid address"),
                crate::RuntimeAttribution::for_session(crate::SessionId::from("session")),
                "process:provenance",
            ),
            RuntimeEffectCommand::Process {
                command: Box::new(command),
            },
        )
    }

    /// Trace provenance rides a command beside its payload and is no part
    /// of the envelope's identity (FIG-4829): the same start, signal or
    /// accepted input under another trace context, or under
    /// none, journals the same bytes and hashes the same, so a retry replays
    /// its recorded effect instead of diverging from it.
    #[test]
    fn trace_provenance_is_no_part_of_an_envelopes_identity() {
        let linked = |id| lash_trace::TraceCause::linked_to(Some(producer(id)));
        let offer = |id| {
            lash_trace::TraceScopeOffer::new(
                linked(id),
                lash_trace::TraceAnchor::Context(producer(id + 100)),
            )
        };
        let start = |trace: lash_trace::TraceScopeOffer| {
            process_effect(crate::ProcessCommand::Start {
                registration: crate::ProcessStartRegistration::of_target(
                    crate::testing::held_engine_input(serde_json::json!({"report": "nightly"})),
                    crate::ProcessProvenance::host(),
                    crate::Lifetime::Detached,
                )
                .with_start_key(Some(crate::StartKey::for_host("provenance-start")))
                .with_trace(trace),
                observers: Vec::new(),
                execution_context: Box::default(),
            })
        };
        let signal = |cause: lash_trace::TraceCause| {
            process_effect(crate::ProcessCommand::Signal {
                signal: crate::ProcessSignal::new(
                    crate::ProcessSignalIdentity::new(
                        crate::ProcessId::fixture("provenance-target"),
                        "ready",
                        "one",
                    )
                    .expect("valid signal identity"),
                    serde_json::json!(1),
                )
                .with_trace_cause(cause),
            })
        };
        let accept = |cause: lash_trace::TraceCause| {
            let mut envelope = process_effect(crate::ProcessCommand::List {
                selection: crate::ProcessListSelection::HostRunning,
            });
            envelope.command = RuntimeEffectCommand::AcceptTurnInput {
                draft: Box::new(
                    crate::PendingTurnInputDraft::new(
                        "session",
                        crate::TurnInputIngress::next_turn(),
                        crate::TurnInput::text("the accepted words"),
                    )
                    .with_source_key("provenance-input")
                    .with_trace_cause(cause),
                ),
            };
            envelope
        };
        for (what, untraced, first, second) in [
            (
                "start",
                start(lash_trace::TraceScopeOffer::default()),
                start(offer(1)),
                start(offer(2)),
            ),
            (
                "signal",
                signal(lash_trace::TraceCause::Root),
                signal(linked(1)),
                signal(linked(2)),
            ),
            (
                "input",
                accept(lash_trace::TraceCause::Root),
                accept(linked(1)),
                accept(linked(2)),
            ),
        ] {
            let recorded = untraced.canonical_form().expect("canonical");
            for traced in [first, second] {
                assert!(
                    traced.command.without_trace_provenance().is_some(),
                    "{what}: the traced command carries provenance"
                );
                let retried = traced.canonical_form().expect("canonical");
                assert_eq!(retried.json(), recorded.json(), "{what}");
                assert_eq!(retried.hash(), recorded.hash(), "{what}");
                validate_replayed_effect_envelope(
                    &recorded,
                    &retried,
                    crate::RuntimeErrorCode::EffectReplayDivergence,
                    None,
                )
                .unwrap_or_else(|error| panic!("{what}: a retry replays: {error:?}"));
            }
            assert!(
                !recorded.json().contains("traceparent"),
                "{what}: no context is journaled"
            );
        }
    }
}
