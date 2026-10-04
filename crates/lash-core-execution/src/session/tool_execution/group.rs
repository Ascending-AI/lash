//! Aggregate consumer modes and deferred completion projection.

use super::*;
use crate::runtime::effect::GroupWakePolicy;

/// How a group's consumer decides its aggregate (ADR 0099 §10 L1). A
/// caller-side loop decision, never journaled; [`Self::wake`] is the journaled
/// wake policy it implies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolAggregateConsumer {
    /// Every settlement: `allSettled` and every all-results batch.
    AllSettled,
    /// Stop at the first consumed rejection: `Promise.all`.
    All,
    /// Stop at the first settlement: `Promise.race`.
    Race,
    /// Stop at the first fulfilment: `Promise.any`.
    Any,
}

impl ToolAggregateConsumer {
    /// The three-way journaled wake policy this four-way consumer mode folds
    /// into every child's envelope (ADR 0099 §10 L1, ADR 0065).
    #[must_use]
    pub fn wake(self) -> GroupWakePolicy {
        match self {
            Self::AllSettled | Self::All => GroupWakePolicy::All,
            Self::Race => GroupWakePolicy::First,
            Self::Any => GroupWakePolicy::FirstSuccess,
        }
    }

    /// Whether a settlement with this fulfilment decides the aggregate.
    #[must_use]
    pub fn decides(self, fulfilled: bool) -> bool {
        match self {
            Self::AllSettled => false,
            Self::All => !fulfilled,
            Self::Race => true,
            Self::Any => fulfilled,
        }
    }
}

impl RuntimeExecutionContext<'_> {
    /// Emits a deferred call's already incorporated settlement.
    pub(crate) async fn apply_tool_child_settlement(
        &self,
        call_key: &str,
        call: &crate::PreparedToolCall,
        outcome: ToolDispatchOutcome,
        settlement: crate::runtime::ToolSettlement,
        duration_ms: u64,
    ) -> Result<CompletedProtocolToolCall, crate::RuntimeEffectControllerError> {
        let call_id = call.call_id.clone();
        let correlation_id = tool_activity_id(&call_id);
        // The deferred-completion caller incorporates settlement facts before
        // this projection emits the recorded return and activity events.
        // A child that ran with no live opener recorded its stream instead of
        // sending it (FIG-3712); it reaches the stream here, before the
        // child's own completion.
        self.emit_recorded_child_stream(call_key, &call_id, &outcome.record, &settlement.stream);
        {
            let context = self.with_call_observation_key(self.call_observation_key(call_key));
            let mut cursor = context.observation_cursor(&format!("tool:{call_id}:intents"));
            // A child's realized outcomes ride its settlement, where its
            // driver moved them; a declared start's launch receipt is one.
            for intent_outcome in &settlement.intent_outcomes {
                cursor.observe(
                    context.dispatch.observer.as_ref(),
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: Some(correlation_id.clone()),
                        event: TurnEvent::ToolIntentOutcome {
                            call_id: call_id.clone(),
                            outcome: intent_outcome.clone(),
                        },
                    },
                );
            }
        }
        let record = ToolCallRecord {
            call_id: call_id.clone(),
            provider_call_id: call.provider_call_id.clone(),
            tool: outcome.record.tool.clone(),
            args: outcome.record.args.clone(),
            output: outcome.record.output.clone(),
        };
        self.emit_tool_call_completed(
            call_key,
            &record,
            &outcome.attempts,
            duration_ms,
            &settlement.intent_outcomes,
        )
        .await;
        Ok(CompletedProtocolToolCall {
            completed: crate::sansio::CompletedToolCall {
                call_id,
                provider_call_id: call.provider_call_id.clone(),
                tool_name: outcome.record.tool,
                args: outcome.record.args,
                output: outcome.record.output,
                model_return: settlement.model_return,
                intent_outcomes: outcome.intent_outcomes,
                replay: call.replay.clone(),
            },
            record,
        })
    }
}

/// The tool failure a call refused by the session's `max_tool_calls` settles
/// with: typed by its code, never retried, and worded by the refusal so the
/// limit is named wherever the failure is shown (FIG-4546).
pub(crate) fn tool_call_limit_failure(exceeded: crate::ToolCallLimitExceeded) -> ToolFailure {
    ToolFailure::runtime(
        ToolFailureClass::ResourceLimit,
        crate::ToolCallLimitExceeded::CODE,
        exceeded.to_string(),
    )
}

impl RuntimeExecutionContext<'_> {
    /// Emits the stream events a group child recorded because no opener was
    /// live where it ran (FIG-3712): its session events publish through
    /// `ObservedEvent::Session`, so they project at emission exactly as the
    /// live opener's forwarder projected them, and its turn activities publish
    /// verbatim — they carry the identities the child's recorder minted and
    /// are not re-keyed. Both lanes key under the child's own replay key
    /// (`call_key`). A stream the recording budget cut says so on the session
    /// stream, as a `child_stream_truncated` message.
    fn emit_recorded_child_stream(
        &self,
        call_key: &str,
        call_id: &crate::ToolCallId,
        record: &crate::ToolCallRecord,
        stream: &crate::runtime::effect::AttemptStream,
    ) {
        let record = serde_json::to_value(record).unwrap_or_default();
        self.emit_recorded_child_stream_value(call_key, call_id, &record, stream);
    }

    fn emit_recorded_child_stream_value(
        &self,
        call_key: &str,
        call_id: &crate::ToolCallId,
        record: &serde_json::Value,
        stream: &crate::runtime::effect::AttemptStream,
    ) {
        let (events, undecodable) = stream.decode(record);
        if undecodable > 0 {
            tracing::warn!(
                call_id = call_id.as_str(),
                undecodable,
                "a tool child's recorded stream held events this build cannot decode; \
                 they are skipped"
            );
        }
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        let mut cursor = context.observation_cursor("stream");
        let observer = context.dispatch.observer.as_ref();
        for event in events {
            match event {
                crate::runtime::effect::DecodedStreamEvent::Session(event) => {
                    cursor.observe(observer, crate::engine::ObservedEvent::Session(event));
                }
                crate::runtime::effect::DecodedStreamEvent::Activity(activity) => {
                    cursor.observe(
                        observer,
                        crate::engine::ObservedEvent::RecordedActivity(activity),
                    );
                }
            }
        }
        if let Some(truncated) = stream.truncated {
            cursor.observe(
                observer,
                crate::engine::ObservedEvent::RecordedSession(crate::SessionStreamEvent::Message {
                    text: format!(
                        "tool child `{call_id}` recorded more stream than its budget holds; \
                             {} later events ({} bytes) were dropped",
                        truncated.dropped_events, truncated.dropped_bytes
                    ),
                    kind: crate::StreamMessageKind::ChildStreamTruncated,
                }),
            );
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "dispatch", rename_all = "snake_case")]
pub enum ToolDispatchResult {
    Done(Box<CompletedProtocolToolCall>),
    Deferred(Box<crate::tool_dispatch::DeferredToolCompletion>),
}
