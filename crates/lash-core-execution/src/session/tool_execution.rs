use super::execution_context::RuntimeExecutionContext;
use crate::tool_dispatch::{
    ToolCallIds, ToolDispatchOutcome, ToolPreparationOutcome, prepare_tool_call_with_context,
};
use crate::{
    ModelToolReturn, SessionStreamEvent, ToolCallOutput, ToolCallRecord, ToolCancellation,
    ToolFailure, ToolFailureClass, TurnActivityId, TurnEvent,
};

/// v3 (FIG-3586) retires the opener occurrence ordinal v2 folded into the
/// batch identity.
///
/// v1 hashed the calls and nothing else, so two textually identical
/// aggregates raised by one opener shared one identity; v2 folded the VM's
/// per-instruction occurrence in to separate them (ADR 0065). That ordinal was
/// compiler output — an instruction pointer's reach count — and moved with
/// lowering. A lashlang aggregate is now addressed by the issue ordinal its
/// runtime mints ([`CommandReplayKey`](crate::CommandReplayKey)), so a batch
/// identity is the batch's content again, and on the aggregate path it names
/// nothing durable: it is a digest the group head checks, never a key.
///
/// The host-code batch path (`call_tool_batch` outside a lashlang aggregate)
/// keeps content identity. Two structurally identical batches raised from one
/// such caller still share an identity; that caller is ordinary host code with
/// no deterministic count of its own.
///
/// version_guard(
///     items(tool_invocation_batch_preimage),
/// )
/// version_surface = "coexist"
const TOOL_BATCH_FAMILY_VERSION: u8 = 3;

#[derive(Clone)]
pub struct ToolInvocation {
    /// The call's lash-minted identity: its idempotency key and the root of
    /// every key its attempts, awaits and presentation journal under.
    pub id: crate::ToolCallId,
    pub tool_id: crate::ToolId,
    pub args: serde_json::Value,
    /// Original native request correlation and replay data, when supplied.
    pub pending: Option<Box<crate::sansio::PendingToolCall>>,
    pub execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    /// The binding a replayed code cell recorded for this call when the live
    /// tool has since drifted (FIG-3587). Never part of the call's identity.
    pub recorded_binding: Option<Box<crate::ToolDefinition>>,
    pub child_execution_trace_hook: Option<crate::ToolChildExecutionTraceHook>,
    pub issuing_language_node_id: Option<String>,
}

impl ToolInvocation {
    pub fn new(id: crate::ToolCallId, tool_id: crate::ToolId, args: serde_json::Value) -> Self {
        Self {
            id,
            tool_id,
            args,
            pending: None,
            execution_grant: None,
            recorded_binding: None,
            child_execution_trace_hook: None,
            issuing_language_node_id: None,
        }
    }

    pub fn from_pending(pending: crate::sansio::PendingToolCall, tool_id: crate::ToolId) -> Self {
        let mut invocation = Self::new(pending.call_id.clone(), tool_id, pending.args.clone());
        invocation.pending = Some(Box::new(pending));
        invocation
    }

    pub fn with_child_execution_trace_hook(
        mut self,
        hook: crate::ToolChildExecutionTraceHook,
    ) -> Self {
        self.child_execution_trace_hook = Some(hook);
        self
    }

    pub fn with_execution_grant(mut self, grant: crate::ToolExecutionGrant) -> Self {
        self.execution_grant = Some(Box::new(grant));
        self
    }

    /// Authorizes this call under a code cell's recorded binding (FIG-3587).
    pub fn with_recorded_binding(mut self, binding: crate::ToolDefinition) -> Self {
        self.recorded_binding = Some(Box::new(binding));
        self
    }

    pub fn with_issuing_language_node_id(mut self, node_id: impl Into<String>) -> Self {
        self.issuing_language_node_id = Some(node_id.into());
        self
    }
}

impl std::fmt::Debug for ToolInvocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolInvocation")
            .field("id", &self.id)
            .field("tool_id", &self.tool_id)
            .field("args", &self.args)
            .field(
                "execution_grant",
                &self.execution_grant.as_ref().map(|_| "<grant>"),
            )
            .field(
                "recorded_binding",
                &self.recorded_binding.as_ref().map(|_| "<binding>"),
            )
            .field(
                "child_execution_trace_hook",
                &self.child_execution_trace_hook.as_ref().map(|_| "<hook>"),
            )
            .field("issuing_language_node_id", &self.issuing_language_node_id)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation(id: &str, value: i64) -> ToolInvocation {
        ToolInvocation::new(
            crate::ToolCallId::fixture(id),
            crate::ToolId::from("tool:test"),
            serde_json::json!({"value": value}),
        )
    }

    #[test]
    fn deterministic_batch_identity_is_stable_and_content_addressed() {
        let calls = vec![invocation("a", 1), invocation("b", 2)];
        let first = deterministic_tool_invocation_batch_id(&calls);
        let retry = deterministic_tool_invocation_batch_id(&calls);
        assert_eq!(first, retry);
        assert_eq!(
            first,
            "tool-batch:v3:blake3:b2d94b9d004eec029754ce86f2e8314d0c068283a5dd31ea31f7201ab344a573"
        );
        assert_eq!(
            hex(&tool_invocation_batch_preimage(&calls)),
            "6c6173682d737461626c652d6964656e746974790203000000000000001a6c6173682e746f6f6c2d696e766f636174696f6e2d62617463680000000000000002000000000000004374635f623937633230306463653665393537386635383463613566323234356433643131323261333231313036366462386332643739666364663036346634306234620000000000000009746f6f6c3a74657374000000000000000b7b2276616c7565223a317d00000000000000004374635f336339393433336562303636666134613066343132653131316565333632643964316436663035616537626665626337313833346233663339373837623436660000000000000009746f6f6c3a74657374000000000000000b7b2276616c7565223a327d00"
        );

        let changed_args = vec![invocation("a", 1), invocation("b", 3)];
        let reordered = vec![invocation("b", 2), invocation("a", 1)];
        assert_ne!(first, deterministic_tool_invocation_batch_id(&changed_args));
        assert_ne!(first, deterministic_tool_invocation_batch_id(&reordered));
    }

    #[test]
    fn issuing_node_id_does_not_change_tool_batch_identity() {
        let plain = vec![invocation("a", 1)];
        let attributed = vec![invocation("a", 1).with_issuing_language_node_id("node:issuer")];

        assert_eq!(
            tool_invocation_batch_preimage(&plain),
            tool_invocation_batch_preimage(&attributed),
            "trace attribution must not enter the durable tool-batch preimage"
        );
        assert_eq!(
            deterministic_tool_invocation_batch_id(&plain),
            deterministic_tool_invocation_batch_id(&attributed),
        );
    }

    #[test]
    fn granted_batch_identity_pins_present_grant_routing_grammar() {
        let grant = crate::ToolExecutionGrant::from_definition(
            crate::plugin::PluginRevision::new("mock", crate::plugin::BehaviorRevision::ONE),
            crate::ToolDefinition::raw(
                "tool:granted",
                "granted",
                "golden",
                serde_json::json!({"type": "object"}),
                serde_json::json!({"type": "string"}),
            )
            .expect("valid declared tool schemas"),
        )
        .with_source_id("plugin\0route")
        .with_execution_binding(serde_json::json!({"route": ["λ", -0.0]}));
        let calls = vec![
            ToolInvocation::new(
                crate::ToolCallId::fixture("grant\0call"),
                crate::ToolId::from("tool:granted"),
                serde_json::json!({"value": true}),
            )
            .with_execution_grant(grant),
        ];
        assert_eq!(
            hex(&tool_invocation_batch_preimage(&calls)),
            "6c6173682d737461626c652d6964656e746974790203000000000000001a6c6173682e746f6f6c2d696e766f636174696f6e2d62617463680000000000000001000000000000004374635f65323738373230356261356666643964303535323439653030353632356537363731623635383662363634383665313934363739323630636564383939646435000000000000000c746f6f6c3a6772616e746564000000000000000e7b2276616c7565223a747275657d01000000000000000c746f6f6c3a6772616e74656401000000000000000c706c7567696e00726f75746500000000000000147b22726f757465223a5b22cebb222c302e305d7d"
        );
        assert_eq!(
            deterministic_tool_invocation_batch_id(&calls),
            "tool-batch:v3:blake3:a23081705b7fb825a1e6f93180c85b108c73193eb2d42f2c8a01d0ec45784cd9"
        );

        let without_source = crate::ToolExecutionGrant::from_definition(
            crate::plugin::PluginRevision::new("mock", crate::plugin::BehaviorRevision::ONE),
            crate::ToolDefinition::raw(
                "tool:granted",
                "granted",
                "golden",
                serde_json::json!({"type": "object"}),
                serde_json::json!({"type": "string"}),
            )
            .expect("valid declared tool schemas"),
        )
        .with_execution_binding(serde_json::json!({"route": ["λ", -0.0]}));
        let without_source = vec![
            ToolInvocation::new(
                crate::ToolCallId::fixture("grant\0call"),
                crate::ToolId::from("tool:granted"),
                serde_json::json!({"value": true}),
            )
            .with_execution_grant(without_source),
        ];
        assert_ne!(
            deterministic_tool_invocation_batch_id(&calls),
            deterministic_tool_invocation_batch_id(&without_source),
            "grant source presence must occupy a distinct option arm"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

#[derive(Clone, Debug)]
pub struct ToolInvocationReply {
    pub output: ToolCallOutput,
    pub record: Option<ToolCallRecord>,
    pub completed: Option<Box<crate::sansio::CompletedToolCall>>,
}

impl ToolInvocationReply {
    pub fn success(value: serde_json::Value) -> Self {
        Self {
            output: ToolCallOutput::success(value),
            record: None,
            completed: None,
        }
    }

    pub fn error(value: serde_json::Value) -> Self {
        let message = value
            .as_str()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| value.to_string());
        let mut failure = ToolFailure::tool(ToolFailureClass::Execution, "tool_error", message);
        failure.raw = Some(crate::ToolValue::untrusted_json(value));
        Self {
            output: ToolCallOutput::failure(failure),
            record: None,
            completed: None,
        }
    }

    pub fn from_output(output: ToolCallOutput) -> Self {
        Self {
            output,
            record: None,
            completed: None,
        }
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::from_output(ToolCallOutput::cancelled(ToolCancellation::runtime(
            message,
        )))
    }

    pub(crate) fn with_record(mut self, record: ToolCallRecord) -> Self {
        self.record = Some(record);
        self
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CompletedProtocolToolCall {
    pub completed: crate::sansio::CompletedToolCall,
    pub record: ToolCallRecord,
}

/// Permanent tag registry for tool-batch identities.
///
/// Grant presence uses the universal option tags 0/1. Grant manifest and
/// contract fields outside the explicit execution-address allowlist are
/// exhaustively ignored below. Retired tags remain burned.
fn tool_invocation_batch_preimage(calls: &[ToolInvocation]) -> Vec<u8> {
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.tool-invocation-batch",
        TOOL_BATCH_FAMILY_VERSION,
    );
    identity.sequence(calls, |identity, call| {
        let ToolInvocation {
            id,
            tool_id,
            args,
            pending: _,
            execution_grant,
            recorded_binding: _,
            child_execution_trace_hook: _,
            issuing_language_node_id: _,
        } = call;
        identity.string(id.as_str());
        identity.string(tool_id.as_str());
        identity.bytes(&crate::identity_json::payload_leaf(args));
        identity.optional(execution_grant.as_deref(), |identity, grant| {
            // This exhaustive destructure is the guard: adding a grant field must fail
            // compilation here until its batch-identity inclusion is ruled in or out.
            let crate::ToolExecutionGrant {
                // The executable owner is retained by admission; a redrive keeps the logical call identity.
                owner: _,
                manifest,
                contract: _,
                source_id,
                execution_binding,
            } = grant;
            let crate::ToolManifest {
                inline: _,
                id,
                name: _,
                description: _,
                module: _,
                compact_contract: _,
                bindings: _,
                argument_projection: _,
                execution_policy: _,
                expected_execution: _,
                // The declaration is admission policy, like the retry policy:
                // it never names the logical call.
                declaration: _,
            } = manifest;
            identity.string(id.as_str());
            identity.optional(source_id.as_deref(), |identity, source_id| {
                identity.string(source_id);
            });
            identity.bytes(&crate::identity_json::payload_leaf(execution_binding));
        });
    });
    identity.finish()
}

pub(crate) fn deterministic_tool_invocation_batch_id(calls: &[ToolInvocation]) -> crate::BatchId {
    crate::BatchId::prefixed(
        "tool-batch",
        crate::stable_identity::rendered_hash_tail(
            TOOL_BATCH_FAMILY_VERSION,
            &tool_invocation_batch_preimage(calls),
        ),
    )
}

/// Whether a reported settlement order is an ordering of the batch's own
/// launches: one position per launch, each in range, none repeated.
///
/// A malformed order is refused rather than trimmed. Trimming produces a
/// well-formed permutation that no later validator can tell from a real one.
fn validate_batch_settlement_order(order: &[usize], launches: usize) -> Result<(), String> {
    if order.len() != launches {
        return Err(format!(
            "tool batch reported {} settled positions for {launches} launches",
            order.len()
        ));
    }
    let mut seen = vec![false; launches];
    for position in order {
        let Some(slot) = seen.get_mut(*position) else {
            return Err(format!(
                "tool batch reported settled position {position} for {launches} launches"
            ));
        };
        if *slot {
            return Err(format!(
                "tool batch reported settled position {position} more than once"
            ));
        }
        *slot = true;
    }
    Ok(())
}

/// The replies to a tool batch, in input order, with the order they settled.
#[derive(Debug, Default)]
pub struct ToolBatchReplies {
    /// One reply per invocation, in input order.
    pub replies: Vec<ToolInvocationReply>,
    /// Input indices in the order the invocations settled.
    pub settlement_order: Vec<usize>,
}

pub(crate) fn tool_activity_id(call_id: &crate::ToolCallId) -> TurnActivityId {
    TurnActivityId::new(format!("tool:{call_id}"))
}

impl ToolBatchReplies {
    /// Replies whose invocations settled in the order they were issued.
    pub fn settled_in_input_order(replies: Vec<ToolInvocationReply>) -> Self {
        let settlement_order = (0..replies.len()).collect();
        Self {
            replies,
            settlement_order,
        }
    }
}

mod aggregate;
#[path = "tool_execution/batch.rs"]
mod batch;
mod group;

pub use aggregate::{
    ToolAggregateLeaf, ToolAggregateLeafReply, ToolAggregateOutcome, ToolAggregateRequest,
    ToolRunAggregateCursor, ToolRunAggregatePoll,
};
pub use group::ToolAggregateConsumer;
pub(crate) use group::tool_call_limit_failure;

impl RuntimeExecutionContext<'_> {
    pub fn tool_execution_owner(
        &self,
        tool: &crate::ToolId,
        source: Option<&str>,
    ) -> Result<crate::plugin::PluginRevision, crate::PluginError> {
        self.dispatch.plugins.tool_execution_owner(tool, source)
    }

    /// `call_key` is the material the call's observation lanes key under —
    /// the call's own effect-invocation replay key where it has one (a group
    /// child's `{group}:child:{position}`, a command's key), else the
    /// caller's positional material (`{iteration}:{index}:{call_id}` on the
    /// protocol path, `{batch_id}:{index}:{call_id}` in a batch) — qualified
    /// against this context's observation base (ADR 0105 §1).
    pub(crate) async fn emit_tool_call_started(
        &self,
        call_key: &str,
        ids: &ToolCallIds,
        name: &str,
        args: serde_json::Value,
        activity_id: TurnActivityId,
    ) {
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        let mut cursor = context.observation_cursor(&format!("tool:{}:start", ids.call_id));
        cursor.observe(
            context.dispatch.observer.as_ref(),
            crate::engine::ObservedEvent::Session(SessionStreamEvent::ToolCallStart {
                call_id: ids.call_id.clone(),
                provider_call_id: ids.provider_call_id.clone(),
                name: name.to_string(),
                args: args.clone(),
            }),
        );
        cursor.observe(
            context.dispatch.observer.as_ref(),
            crate::engine::ObservedEvent::Activity {
                correlation_id: Some(activity_id),
                event: TurnEvent::ToolCallStarted {
                    call_id: ids.call_id.clone(),
                    provider_call_id: ids.provider_call_id.clone(),
                    name: name.to_string(),
                    args,
                    graph_key: self.code_block_graph_key(),
                },
            },
        );
    }

    /// `call_key` is the material the call's observation lanes key under —
    /// `{iteration}:{index}:{call_id}` on the turn-dispatched protocol path —
    /// qualified against this context's observation base (ADR 0105 §1).
    pub async fn prepare_tool_call(
        &self,
        pending: crate::sansio::PendingToolCall,
        call_key: &str,
    ) -> ToolPreparationOutcome {
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        let requested_at_ms = self.dispatch.clock.timestamp_ms();
        let preparation = prepare_tool_call_with_context(context.dispatch.as_ref(), pending).await;
        if let ToolPreparationOutcome::Completed(outcome) = &preparation
            && let Err(error) =
                context.trace_tool_call_started((&outcome.record).into(), requested_at_ms)
        {
            context.record_nested_effect_error(error);
        }
        preparation
    }

    /// Settles a call its round's admission refused, without preparing it:
    /// no hook and no provider callback runs for it.
    pub async fn refuse_tool_call(
        &self,
        pending: crate::sansio::PendingToolCall,
        refusal: crate::ToolAdmissionRefusal,
        call_key: &str,
    ) -> ToolPreparationOutcome {
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        let requested_at_ms = self.dispatch.clock.timestamp_ms();
        let failure = crate::tool_dispatch::admission_failure(&pending.tool_name, refusal);
        let outcome = crate::tool_dispatch::normalized_outcome(
            context.dispatch.as_ref(),
            &ToolCallIds::of_pending(&pending),
            pending.tool_name,
            pending.args,
            crate::ToolOutcome::failure(failure),
        )
        .await;
        if let Err(error) =
            context.trace_tool_call_started((&outcome.record).into(), requested_at_ms)
        {
            context.record_nested_effect_error(error);
        }
        ToolPreparationOutcome::Completed(Box::new(outcome))
    }

    /// Prepares a call on a tool of the turn's recorded surface whose live
    /// definition drifted (FIG-3672 P7b): under `binding`, the recorded
    /// definition, with identity preparation, so the call's envelope is the
    /// one the journal recorded and the live tool is not consulted.
    pub async fn prepare_recorded_tool_call(
        &self,
        binding: &crate::ToolDefinition,
        pending: crate::sansio::PendingToolCall,
        call_key: &str,
    ) -> ToolPreparationOutcome {
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        let requested_at_ms = self.dispatch.clock.timestamp_ms();
        let preparation = crate::tool_dispatch::prepare_recorded_tool_call_with_context(
            context.dispatch.as_ref(),
            binding,
            pending,
        )
        .await;
        if let ToolPreparationOutcome::Completed(outcome) = &preparation
            && let Err(error) =
                context.trace_tool_call_started((&outcome.record).into(), requested_at_ms)
        {
            context.record_nested_effect_error(error);
        }
        preparation
    }

    /// The catalog entry a model-issued call names, if the catalog holds it.
    pub fn callable_tool_id_by_name(&self, tool_name: &str) -> Option<crate::ToolId> {
        self.dispatch
            .tool_catalog
            .tools
            .iter()
            .find(|tool| tool.manifest.name == tool_name)
            .map(|tool| tool.manifest.id.clone())
    }

    /// Presents a settled call through its journaled `PresentToolResult`
    /// effect and incorporates its settlement.
    ///
    /// # Errors
    /// When the presentation effect itself fails — a replay divergence
    /// against its recorded envelope, a journal fault — the call has no
    /// presentation to show. The error is returned rather than turned into
    /// the tool's model-facing result, so a divergence parks the turn like any
    /// other recorded effect's (FIG-3587) and the model is shown nothing.
    /// `call_key` is the material the call's observation lanes key under; see
    /// [`Self::emit_tool_call_started`].
    pub async fn complete_tool_call(
        &self,
        ids: ToolCallIds,
        tool_id: crate::ToolId,
        replay: Option<crate::llm::types::ProviderReplayMeta>,
        outcome: ToolDispatchOutcome,
        call_key: &str,
        duration_ms: u64,
    ) -> Result<CompletedProtocolToolCall, crate::RuntimeEffectControllerError> {
        Box::pin(async {
            let context = self.with_call_observation_key(self.call_observation_key(call_key));
            let this = &context;
            let call_id = &ids.call_id;
            let attempts = outcome.attempts.clone();
            let output = outcome.record.output.clone();
            let facts = crate::plugin::ToolPresentationFacts {
                intent_outcomes: outcome.intent_outcomes.clone(),
            };
            // The presentation boundary (ADR 0099 §6, FIG-3420): the ordered
            // presentation steps run once through the journaled `PresentToolResult`
            // effect, keyed by `{call_id}:present`, so a replay serves the recorded
            // `ToolPresentation` and never re-runs a step.
            let plan =
                crate::runtime::effect::record_tool_presentation_plan(&self.dispatch, call_id)
                    .await?;
            let presentation_replay_key = format!("{call_id}:present");
            let scoped = self.dispatch.effect_controller.clone();
            let presentation = match crate::EffectAddress::new(
                scoped.execution_scope().clone(),
                presentation_replay_key.clone(),
            ) {
                Ok(address) => scoped
                    .tool_effect(
                        crate::RuntimeEffectEnvelope::new(
                            crate::RuntimeEffectInvocation::new(
                                address,
                                self.dispatch.parentless_attribution(),
                                presentation_replay_key,
                            ),
                            crate::RuntimeEffectCommand::PresentToolResult {
                                plan: Box::new(plan.clone()),
                                call_id: call_id.clone(),
                                tool_id,
                                tool_name: outcome.record.tool.clone(),
                                render: self.dispatch.execution_env_spec.render.clone(),
                                args: outcome.record.args.clone(),
                                output: Box::new(outcome.record.output.clone()),
                            },
                        ),
                        crate::RuntimeEffectLocalExecutor::presentation(
                            std::sync::Arc::clone(&self.dispatch.plugins),
                            std::sync::Arc::new(facts),
                            std::sync::Arc::clone(&self.dispatch.attachment_store),
                            self.attachment_acceptance().clone(),
                            duration_ms,
                            &plan,
                        ),
                    )
                    .await
                    .and_then(crate::RuntimeEffectOutcome::into_tool_presentation),
                Err(error) => Err(error.into()),
            }?;
            let mut model_return = presentation.model_return;
            let possession = outcome
                .intent_outcomes
                .iter()
                .filter_map(|outcome| match outcome {
                    crate::ToolIntentExecutionOutcome::Executed {
                        realized: crate::ToolIntentRealized::StartProcess(handle),
                        ..
                    } => Some(handle.process_id.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let messages = outcome
                .captures
                .iter()
                .flat_map(|capture| capture.messages.iter().cloned())
                .collect::<Vec<_>>();
            this.incorporate_tool_facts(
                crate::session::SettlementSource::Invocation {
                    call_id: call_id.clone(),
                },
                &possession,
                &messages,
                &outcome.triggers,
            )?;
            for intent_outcome in crate::tool_dispatch::model_visible_intent_outcomes(&outcome) {
                model_return.parts.push(crate::ModelToolReturnPart::text(
                    intent_outcome.model_addendum(),
                ));
            }
            self.emit_tool_intent_outcome_activities(call_key, call_id, &outcome.intent_outcomes);

            let record = ToolCallRecord {
                call_id: ids.call_id.clone(),
                provider_call_id: ids.provider_call_id.clone(),
                tool: outcome.record.tool.clone(),
                args: outcome.record.args.clone(),
                output: output.clone(),
            };
            this.emit_tool_call_completed(call_key, &record, &attempts, duration_ms)
                .await;
            Ok(CompletedProtocolToolCall {
                completed: crate::sansio::CompletedToolCall {
                    call_id: ids.call_id,
                    provider_call_id: ids.provider_call_id,
                    tool_name: outcome.record.tool,
                    args: outcome.record.args,
                    output,
                    model_return,
                    intent_outcomes: outcome.intent_outcomes,
                    replay,
                },
                record,
            })
        })
        .await
    }

    /// `call_key` is the material the call's observation lanes key under; see
    /// [`Self::emit_tool_call_started`]. `duration_ms` is the measured
    /// wall-clock the caller observed for the call — an observation-only
    /// value: recorded content carries no durations, so it arrives on this
    /// path rather than on the record (FIG-3696).
    async fn emit_tool_call_completed(
        &self,
        call_key: &str,
        record: &ToolCallRecord,
        attempts: &[lash_trace::TraceRetryAttempt],
        duration_ms: u64,
    ) {
        self.trace_tool_call_completed(record, attempts);
        self.emit_tool_call_completed_activity(call_key, record, duration_ms);
    }

    /// Observes each of a call's intent outcomes, ahead of its completion.
    pub(crate) fn emit_tool_intent_outcome_activities(
        &self,
        call_key: &str,
        call_id: &crate::ToolCallId,
        intent_outcomes: &[crate::ToolIntentExecutionOutcome],
    ) {
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        let mut cursor = context.observation_cursor(&format!("tool:{call_id}:intents"));
        for intent_outcome in intent_outcomes {
            cursor.observe(
                context.dispatch.observer.as_ref(),
                crate::engine::ObservedEvent::Activity {
                    correlation_id: Some(tool_activity_id(call_id)),
                    event: TurnEvent::ToolIntentOutcome {
                        call_id: call_id.clone(),
                        outcome: intent_outcome.clone(),
                    },
                },
            );
        }
    }

    pub(crate) fn emit_tool_call_completed_activity(
        &self,
        call_key: &str,
        record: &ToolCallRecord,
        duration_ms: u64,
    ) {
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        let mut cursor = context.observation_cursor(&format!("tool:{}:complete", record.call_id));
        cursor.observe(
            context.dispatch.observer.as_ref(),
            crate::engine::ObservedEvent::Activity {
                correlation_id: Some(tool_activity_id(&record.call_id)),
                event: TurnEvent::ToolCallCompleted {
                    call_id: record.call_id.clone(),
                    provider_call_id: record.provider_call_id.clone(),
                    name: record.tool.clone(),
                    args: record.args.clone(),
                    output: record.output.clone(),
                    duration_ms,
                    graph_key: self.code_block_graph_key(),
                },
            },
        );
    }

    /// `call_key` is the material the call's observation lanes key under —
    /// `{iteration}:{index}:{call_id}` on the turn-dispatched protocol path;
    /// see [`Self::emit_tool_call_started`]. `duration_ms` is the caller's
    /// measured window for the call; it rides the observation only.
    pub async fn complete_undispatched_tool_call(
        &self,
        ids: ToolCallIds,
        tool_id: crate::ToolId,
        replay: Option<crate::llm::types::ProviderReplayMeta>,
        outcome: ToolDispatchOutcome,
        call_key: &str,
        duration_ms: u64,
    ) -> Result<CompletedProtocolToolCall, crate::RuntimeEffectControllerError> {
        if let Some(error) = self.peek_nested_effect_error() {
            return Err(error);
        }
        self.emit_tool_call_started(
            call_key,
            &ids,
            &outcome.record.tool,
            outcome.record.args.clone(),
            tool_activity_id(&ids.call_id),
        )
        .await;
        self.complete_tool_call(ids, tool_id, replay, outcome, call_key, duration_ms)
            .await
    }

    /// `call_key` is the material the call's observation lanes key under; see
    /// [`Self::emit_tool_call_started`].
    pub async fn report_undispatched_tool_call(
        &self,
        completed: &crate::sansio::CompletedToolCall,
        call_key: &str,
    ) {
        let record = ToolCallRecord {
            call_id: completed.call_id.clone(),
            provider_call_id: completed.provider_call_id.clone(),
            tool: completed.tool_name.clone(),
            args: completed.args.clone(),
            output: completed.output.clone(),
        };
        // The protocol supplied this original call identity and refused it
        // before dispatch.
        if let Err(error) =
            self.trace_tool_call_started((&record).into(), self.dispatch.clock.timestamp_ms())
        {
            self.record_nested_effect_error(error);
            return;
        }
        self.emit_tool_call_started(
            call_key,
            &ToolCallIds {
                call_id: completed.call_id.clone(),
                provider_call_id: completed.provider_call_id.clone(),
            },
            &completed.tool_name,
            completed.args.clone(),
            tool_activity_id(&completed.call_id),
        )
        .await;
        // The call completed host-side; no measured window exists on this
        // path, so the observation reports 0 rather than a live clock read
        // made long after the work ran (FIG-3696).
        self.emit_tool_call_completed(call_key, &record, &[], 0)
            .await;
    }

    /// `call_key` is the material the settled call's observation lanes key
    /// under — its await's journaled invocation replay key; see
    /// [`Self::emit_tool_call_started`].
    #[allow(clippy::too_many_arguments)]
    pub async fn pending_completion_dispatch_outcome(
        &self,
        ids: &ToolCallIds,
        call_key: &str,
        tool_name: String,
        args: serde_json::Value,
        resolution: crate::Resolution,
        resolver: Option<&crate::PendingResolver>,
        attempts: Vec<lash_trace::TraceRetryAttempt>,
        mut captures: Vec<crate::runtime::ToolAttemptCapture>,
        mut triggers: Vec<crate::tool_dispatch::ToolTriggerEffectOutcome>,
    ) -> ToolDispatchOutcome {
        // The resume's own producers — the after-checks' contributions —
        // write into buffers fresh to this resume, so what they commit is
        // captured into the outcome rather than into a buffer a sibling
        // attempt may still be writing into.
        let mut resumed_dispatch = (*self.dispatch).clone();
        resumed_dispatch.observation_call_key = Some(self.call_observation_key(call_key));
        resumed_dispatch.checkpoint_messages =
            crate::tool_dispatch::CheckpointMessageBuffer::default();
        resumed_dispatch.trigger_outcomes =
            crate::tool_dispatch::ToolTriggerOutcomeBuffer::default();
        // The parked row keeps the call's name, not its tool id: the catalog
        // names the id while the tool is still a member.
        let tool_id = crate::tool_dispatch::resolve_callable_manifest(&self.dispatch, &tool_name)
            .map_or_else(
                || crate::ToolId::from(tool_name.as_str()),
                |manifest| manifest.id,
            );
        let prepared = crate::plugin::PreparedCallReadView::new(crate::PreparedToolCall {
            call_id: ids.call_id.clone(),
            provider_call_id: ids.provider_call_id.clone(),
            tool_id,
            tool_name,
            args,
            replay: None,
            prepared_payload: serde_json::Value::Null,
        });
        let mut outcome = crate::tool_dispatch::settle_completed_pending_tool_call(
            &resumed_dispatch,
            ids,
            &prepared,
            resolution,
            resolver,
            attempts,
        )
        .await;
        triggers.extend(resumed_dispatch.trigger_outcomes.drain());
        let capture = crate::runtime::ToolAttemptCapture {
            messages: resumed_dispatch.checkpoint_messages.drain(),
        };
        if !capture.is_empty() {
            captures.push(capture);
        }
        outcome.captures = captures;
        outcome.triggers = triggers;
        outcome
    }

    pub fn restore_tool_trigger_outcomes(
        &self,
        outcomes: Vec<crate::tool_dispatch::ToolTriggerEffectOutcome>,
    ) {
        for outcome in outcomes {
            self.dispatch.trigger_outcomes.enqueue(outcome);
        }
    }

    pub(crate) async fn await_process_with_cancellation(
        &self,
        process_id: &crate::ProcessId,
        parent_invocation: Option<crate::RuntimeInvocation>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        let _phase = self.named_phase("process.await_handle");
        let cancellation = cancellation.unwrap_or_default();
        let process_scope = self
            .process_scope(parent_invocation)
            .with_turn_cancellation(&self.turn_cancel_wait(cancellation));
        self.dispatch
            .processes
            .await_process_ref(process_id, process_scope)
            .await
    }

    /// Executes one tool call a replayed language program issued as a command
    /// (FIG-3586), for code-executor implementors.
    ///
    /// `command` is the command's replay key: every attempt, retry sleep and
    /// deferred-completion await of the call is journaled under it, and
    /// nothing about the call itself — its id, tool name, or the site that
    /// issued it — is key material. The invocation's grant, when it carries
    /// one, authorizes a call outside Tool Catalog membership; its trace hook,
    /// when present, reports nested child execution.
    pub async fn call_command_tool(
        &self,
        command: &crate::CommandReplayKey,
        invocation: ToolInvocation,
    ) -> ToolInvocationReply {
        let call_id = invocation.id.clone();
        let outcome = self
            .call_tool_aggregate(ToolAggregateRequest {
                leaves: vec![ToolAggregateLeaf::Tool(invocation)],
                consumer: ToolAggregateConsumer::AllSettled,
                settled_value_after: None,
                command: command.clone(),
            })
            .await;
        match outcome {
            ToolAggregateOutcome::AllResults(mut replies) => match replies.pop().flatten() {
                Some(ToolAggregateLeafReply::Tool(reply)) => *reply,
                _ => ToolInvocationReply::error(serde_json::json!("a scalar tool has no terminal")),
            },
            ToolAggregateOutcome::HostControl(message) => {
                ToolInvocationReply::error(serde_json::Value::String(message))
            }
            ToolAggregateOutcome::ToolCallLimitExceeded(exceeded) => {
                ToolInvocationReply::error(serde_json::json!(exceeded.to_string()))
            }
            _ => ToolInvocationReply::error(serde_json::json!(format!(
                "scalar call {call_id} returned an aggregate selection"
            ))),
        }
    }

    /// Delivers cancellation to a deferred tool handle for code-executor implementors.
    pub async fn cancel_tool_handle(
        &self,
        call_id: crate::ToolCallId,
        handle: serde_json::Value,
    ) -> ToolInvocationReply {
        self.cancel_process_handle(call_id, handle).await
    }

    /// Awaits a deferred tool handle for code-executor implementors without re-executing the call.
    pub async fn await_tool_handle(
        &self,
        call_id: crate::ToolCallId,
        handle: serde_json::Value,
    ) -> ToolInvocationReply {
        self.await_process_handle(call_id, handle).await
    }
}

pub(crate) fn surface_attachment_materialization_notices(
    snapshot: &crate::provider::AttachmentCapabilitySnapshot,
    output: &ToolCallOutput,
    model_return: &mut ModelToolReturn,
) {
    for notice in output.attachments().iter().filter_map(|source| {
        crate::attachments::attachment_materialization_notice(snapshot, source)
    }) {
        model_return
            .parts
            .push(crate::ModelToolReturnPart::text(notice.model_placeholder()));
        model_return.attachment_notices.push(notice);
    }
}

#[cfg(test)]
mod attachment_materialization_tests {
    use super::*;

    #[test]
    fn successful_unsupported_attachment_surfaces_typed_admission_notice() {
        let attachment_ref = crate::AttachmentRef {
            id: crate::AttachmentId::parse("unsupported-tool-attachment").expect("attachment id"),
            media_type: crate::MediaType::parse("application/octet-stream").expect("binary MIME"),
            byte_len: 34,
            type_metadata: None,
            label: Some("workspace_badge.bin".to_string()),
        };
        let output = crate::ToolCallOutput::success_tool_value(crate::ToolValue::Attachment(
            crate::AttachmentSource::stored(attachment_ref),
        ));
        let mut model_return =
            crate::ModelToolReturn::from_output("workspace_badge".to_string(), &output);

        surface_attachment_materialization_notices(
            &crate::attachments::attachment_test_acceptance(),
            &output,
            &mut model_return,
        );

        assert!(output.is_success(), "admission must not fail the tool");
        assert_eq!(model_return.attachment_notices.len(), 1);
        assert_eq!(
            model_return.attachment_notices[0].reason,
            crate::AttachmentMaterializationReason::NoProviderAcceptsMimeAndSource
        );
        assert!(model_return.parts.iter().any(|part| {
            matches!(
                part,
                crate::ModelToolReturnPart::Text { text }
                    if text.contains("attachment_unavailable")
                        && text.contains("workspace_badge.bin")
            )
        }));
    }
}

#[cfg(test)]
mod settlement_order_boundary_tests {
    use super::validate_batch_settlement_order as validate;

    /// The reviewer's probe table. Every malformed order must be refused at the
    /// boundary; repairing one into an input-order permutation is what silently
    /// restored the original rejection-selection bug.
    #[test]
    fn a_malformed_settlement_order_is_refused_not_repaired() {
        assert!(
            validate(&[1, 0], 2).is_ok(),
            "a genuine out-of-order settle"
        );
        assert!(
            validate(&[0, 1], 2).is_ok(),
            "input order is still an order"
        );
        let out_of_range =
            validate(&[usize::MAX, 0], 2).expect_err("an out-of-range position must be refused");
        assert!(
            out_of_range.contains("settled position"),
            "the refusal names the position: {out_of_range}"
        );
        let empty = validate(&[], 2).expect_err("an empty order must be refused");
        assert!(empty.contains("0 settled positions"), "{empty}");
        let duplicate = validate(&[0, 0], 2).expect_err("a duplicated position must be refused");
        assert!(duplicate.contains("more than once"), "{duplicate}");
        assert!(
            validate(&[0, 1, 0], 2).is_err(),
            "an over-long order must be refused"
        );
    }
}
