use super::execution_context::RuntimeExecutionContext;
use crate::tool_dispatch::{
    ToolAttemptLineage, ToolCallIds, ToolCallLaunch, ToolDispatchOutcome, ToolPreparationOutcome,
    coordinate_tool_invocation, prepare_granted_tool_call_with_context,
    prepare_tool_call_with_context,
};
use crate::{
    ModelToolReturn, SessionStreamEvent, ToolCallOutput, ToolCallRecord, ToolCancellation,
    ToolFailure, ToolFailureClass, TurnActivityId, TurnEvent,
};
use std::collections::BTreeMap;
use std::sync::Arc;

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

enum ToolCallAuthorization {
    Catalog(crate::ToolId),
    Granted(Box<crate::ToolExecutionGrant>),
    /// A replayed code cell's call on a host tool binding that drifted since
    /// the pass that journaled the cell (FIG-3587): authorized under the
    /// cell's recorded binding instead of the live catalog, and otherwise the
    /// catalog call it was, so its attempt envelope is the recorded one.
    Recorded(Box<crate::ToolDefinition>),
}

impl ToolCallAuthorization {
    fn from_invocation(call: &mut ToolInvocation) -> Self {
        if let Some(binding) = call.recorded_binding.take() {
            return Self::Recorded(binding);
        }
        match call.execution_grant.take() {
            Some(grant) => Self::Granted(grant),
            None => Self::Catalog(call.tool_id.clone()),
        }
    }

    fn tool_id(&self) -> &crate::ToolId {
        match self {
            Self::Catalog(tool_id) => tool_id,
            Self::Granted(grant) => &grant.manifest().id,
            Self::Recorded(binding) => &binding.manifest.id,
        }
    }

    /// The manifest this call is authorized under: the catalog's answer for a
    /// catalog call, the grant's carried manifest for a granted one. Kept whole
    /// rather than reduced to a name because group formation retains it as the
    /// child's admission (ADR 0099 §3).
    fn resolve_manifest(
        &self,
        dispatch: &crate::tool_dispatch::ToolDispatchContext<'_>,
    ) -> Option<crate::ToolManifest> {
        match self {
            Self::Catalog(tool_id) => {
                crate::tool_dispatch::resolve_callable_manifest_by_id(dispatch, tool_id)
            }
            Self::Granted(grant) => Some(grant.manifest().clone()),
            Self::Recorded(binding) => Some(binding.manifest.clone()),
        }
    }

    async fn prepare(
        &self,
        dispatch: &crate::tool_dispatch::ToolDispatchContext<'_>,
        pending: crate::sansio::PendingToolCall,
    ) -> ToolPreparationOutcome {
        match self {
            Self::Catalog(_) => prepare_tool_call_with_context(dispatch, pending).await,
            Self::Granted(grant) => {
                prepare_granted_tool_call_with_context(dispatch, grant, pending).await
            }
            Self::Recorded(binding) => {
                crate::tool_dispatch::prepare_recorded_tool_call_with_context(
                    dispatch, binding, pending,
                )
                .await
            }
        }
    }

    fn execution_grant(&self) -> Option<&crate::ToolExecutionGrant> {
        match self {
            Self::Catalog(_) | Self::Recorded(_) => None,
            Self::Granted(grant) => Some(grant),
        }
    }

    /// The grant the attempt carries, and the one its retry policy resolves
    /// under. A recorded binding carries none, as the catalog call it replays
    /// carried none, but its retry policy is the recorded manifest's.
    fn into_execution_grant(self) -> Option<Box<crate::ToolExecutionGrant>> {
        match self {
            Self::Granted(grant) => Some(grant),
            Self::Catalog(_) | Self::Recorded(_) => None,
        }
    }

    fn recorded_retry_policy(&self) -> Option<crate::ToolRetryPolicy> {
        match self {
            Self::Granted(grant) => Some(grant.manifest().retry_policy),
            Self::Recorded(binding) => Some(binding.manifest.retry_policy),
            Self::Catalog(_) => None,
        }
    }
}

#[derive(Clone)]
pub struct ToolInvocation {
    /// The call's lash-minted identity: its idempotency key and the root of
    /// every key its attempts, awaits and presentation journal under.
    pub id: crate::ToolCallId,
    pub tool_id: crate::ToolId,
    pub args: serde_json::Value,
    pub execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    /// The binding a replayed code cell recorded for this call when the live
    /// tool has since drifted (FIG-3587). Never part of the call's identity.
    pub recorded_binding: Option<Box<crate::ToolDefinition>>,
    pub child_execution_trace_hook: Option<crate::ToolChildExecutionTraceHook>,
    pub issuing_language_node_id: Option<String>,
}

struct AdmittedCallIdentity(crate::ToolCallId, crate::ToolId);

impl ToolInvocation {
    pub fn new(id: crate::ToolCallId, tool_id: crate::ToolId, args: serde_json::Value) -> Self {
        Self {
            id,
            tool_id,
            args,
            execution_grant: None,
            recorded_binding: None,
            child_execution_trace_hook: None,
            issuing_language_node_id: None,
        }
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
}

impl ToolInvocationReply {
    pub fn success(value: serde_json::Value) -> Self {
        Self {
            output: ToolCallOutput::success(value),
            record: None,
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
        }
    }

    pub fn from_output(output: ToolCallOutput) -> Self {
        Self {
            output,
            record: None,
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

fn cancelled_completed_tool_call(
    ids: ToolCallIds,
    tool_name: String,
    args: serde_json::Value,
    replay: Option<crate::llm::types::ProviderReplayMeta>,
) -> crate::sansio::CompletedToolCall {
    let output = ToolCallOutput::cancelled(ToolCancellation::runtime("tool call cancelled"));
    crate::sansio::CompletedToolCall {
        call_id: ids.call_id,
        provider_call_id: ids.provider_call_id,
        tool_name: tool_name.clone(),
        args,
        model_return: ModelToolReturn {
            tool_name,
            parts: vec![crate::ModelToolReturnPart::text(
                "[Tool execution cancelled]\ntool call cancelled".to_string(),
            )],
            attachment_notices: Vec::new(),
        },
        output,
        intent_outcomes: Vec::new(),
        replay,
    }
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
                retry_policy: _,
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
mod deferred;
mod group;

pub use aggregate::{
    ToolAggregateLeaf, ToolAggregateLeafReply, ToolAggregateOutcome, ToolAggregateRequest,
};
pub use group::{ToolAggregateConsumer, ToolDispatchResult};
#[cfg(test)]
#[path = "tool_execution/turn_cancel_gate_tests.rs"]
mod turn_cancel_gate_tests;

#[cfg(test)]
#[path = "tool_execution/scalar_presentation_tests.rs"]
mod scalar_presentation_tests;

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
            && let Err(error) = context
                .retain_unadmitted_tool_request(&outcome.record, requested_at_ms)
                .await
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
        if let Err(error) = context
            .retain_unadmitted_tool_request(&outcome.record, requested_at_ms)
            .await
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
            && let Err(error) = context
                .retain_unadmitted_tool_request(&outcome.record, requested_at_ms)
                .await
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
            let tool_correlation_id = tool_activity_id(call_id);
            let attempts = outcome.attempts.clone();
            let mut output = outcome.record.output.clone();
            // The settlement exists before the chain so a step can read its facts;
            // its `model_return` is overwritten by the presented return below.
            let mut settlement = crate::runtime::effect::ToolSettlement::from_dispatch(
                &outcome,
                ModelToolReturn::from_output(outcome.record.tool.clone(), &output),
            );
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
                    .execute_effect(
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
                            std::sync::Arc::new(settlement.clone()),
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
            // ADR 0099 §6/§13: the applicator owns possession, committed messages,
            // trigger receipts and usage charging, exactly once per source. A
            // refusal — an unreadable settlement or a spend with no charge sink —
            // fails the call closed rather than presenting a result whose
            // recorded facts were dropped.
            settlement.model_return = model_return.clone();
            let settlement_source = crate::session::SettlementSource::Invocation {
                call_id: call_id.clone(),
            };
            if let Err(error) = self.incorporate_tool_settlement(settlement_source, &settlement) {
                let message = error.message;
                output = ToolCallOutput::failure(ToolFailure::runtime(
                    ToolFailureClass::Internal,
                    "tool_settlement_incorporation_failed",
                    message.clone(),
                ));
                model_return
                    .parts
                    .push(crate::ModelToolReturnPart::text(format!(
                        "settlement incorporation refused: {message}"
                    )));
            }
            {
                let mut cursor = this.observation_cursor(&format!("tool:{call_id}:intents"));
                for intent_outcome in crate::tool_dispatch::model_visible_intent_outcomes(&outcome)
                {
                    model_return.parts.push(crate::ModelToolReturnPart::text(
                        intent_outcome.model_addendum(),
                    ));
                }
                for intent_outcome in &outcome.intent_outcomes {
                    cursor.observe(
                        this.dispatch.observer.as_ref(),
                        crate::engine::ObservedEvent::Activity {
                            correlation_id: Some(tool_correlation_id.clone()),
                            event: TurnEvent::ToolIntentOutcome {
                                call_id: call_id.clone(),
                                outcome: intent_outcome.clone(),
                            },
                        },
                    );
                }
            }

            let record = ToolCallRecord {
                call_id: ids.call_id.clone(),
                provider_call_id: ids.provider_call_id.clone(),
                tool: outcome.record.tool.clone(),
                args: outcome.record.args.clone(),
                output: output.clone(),
            };
            this.emit_tool_call_completed(
                call_key,
                &record,
                &attempts,
                duration_ms,
                &outcome.intent_outcomes,
            )
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
        intent_outcomes: &[crate::ToolIntentExecutionOutcome],
    ) {
        if let Err(error) = self
            .emit_tool_call_completed_trace(record, attempts, intent_outcomes)
            .await
        {
            self.record_nested_effect_error(error);
        }
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

    /// The completion a language runtime's call answers when a controller
    /// refused it — its attempt or its presentation: the error is recorded as
    /// the run's nested effect error, which stops the run at this command (a
    /// replay divergence parks it), and the call settles a failure that was
    /// never presented or journaled.
    pub(crate) async fn refused_completion(
        &self,
        ids: ToolCallIds,
        tool: String,
        args: serde_json::Value,
        error: crate::RuntimeEffectControllerError,
        call_key: &str,
        duration_ms: u64,
    ) -> CompletedProtocolToolCall {
        self.record_nested_effect_error(error.clone());
        let output = ToolCallOutput::failure(ToolFailure::runtime(
            ToolFailureClass::Internal,
            error.code.as_str(),
            error.message,
        ));
        let record = ToolCallRecord {
            call_id: ids.call_id.clone(),
            provider_call_id: ids.provider_call_id.clone(),
            tool: tool.clone(),
            args: args.clone(),
            output: output.clone(),
        };
        self.emit_tool_call_completed(call_key, &record, &[], duration_ms, &[])
            .await;
        CompletedProtocolToolCall {
            completed: crate::sansio::CompletedToolCall {
                model_return: ModelToolReturn::from_output(tool.clone(), &output),
                call_id: ids.call_id,
                provider_call_id: ids.provider_call_id,
                tool_name: tool,
                args,
                output,
                intent_outcomes: Vec::new(),
                replay: None,
            },
            record,
        }
    }

    /// [`Self::complete_tool_call`] for a language runtime's call, whose
    /// presentation failure stops the run instead of reaching its caller.
    /// `call_key` is the material the call's observation lanes key under; see
    /// [`Self::emit_tool_call_started`].
    async fn complete_language_tool_call(
        &self,
        identity: AdmittedCallIdentity,
        replay: Option<crate::llm::types::ProviderReplayMeta>,
        outcome: ToolDispatchOutcome,
        undispatched: bool,
        call_key: &str,
        duration_ms: u64,
    ) -> CompletedProtocolToolCall {
        let AdmittedCallIdentity(call_id, tool_id) = identity;
        // A language runtime's call is lash's own: it carries no provider
        // correlation.
        let ids = ToolCallIds {
            call_id,
            provider_call_id: None,
        };
        let (tool, args) = (outcome.record.tool.clone(), outcome.record.args.clone());
        // A run that already recorded a nested effect error aborts: this call
        // was settled from inside it — a sibling's divergence — so it presents
        // and journals nothing of its own (FIG-3679).
        if let Some(error) = self.peek_nested_effect_error() {
            return self
                .refused_completion(ids, tool, args, error, call_key, duration_ms)
                .await;
        }
        // Boxed: the presentation future would otherwise inflate every
        // language runtime's call future past clippy's large-future bound.
        let completed = if undispatched {
            Box::pin(self.complete_undispatched_tool_call(
                ids.clone(),
                tool_id,
                replay,
                outcome,
                call_key,
                duration_ms,
            ))
            .await
        } else {
            Box::pin(self.complete_tool_call(
                ids.clone(),
                tool_id,
                replay,
                outcome,
                call_key,
                duration_ms,
            ))
            .await
        };
        match completed {
            Ok(completed) => completed,
            Err(error) => {
                Box::pin(self.refused_completion(ids, tool, args, error, call_key, duration_ms))
                    .await
            }
        }
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
        // before dispatch. Retain that fact before notifying observers.
        if let Err(error) = self
            .retain_unadmitted_tool_request(&record, self.dispatch.clock.timestamp_ms())
            .await
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
        self.emit_tool_call_completed(call_key, &record, &[], 0, &completed.intent_outcomes)
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
            version: crate::runtime::TOOL_ATTEMPT_CAPTURE_VERSION,
            messages: resumed_dispatch.checkpoint_messages.drain(),
        };
        if !capture.is_empty() {
            captures.push(capture);
        }
        outcome.captures = captures;
        outcome.triggers = triggers;
        outcome
    }

    #[expect(
        clippy::expect_used,
        reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
    )]
    async fn complete_pending_tool_dispatch_outcome(
        &self,
        parent_invocation: Option<crate::RuntimeInvocation>,
        pending: crate::tool_dispatch::PendingToolDispatchOutcome,
        cancellation: Option<tokio_util::sync::CancellationToken>,
        child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
    ) -> Result<ToolDispatchOutcome, crate::RuntimeEffectControllerError> {
        let ids = ToolCallIds {
            call_id: pending.call_id.clone(),
            provider_call_id: pending.provider_call_id.clone(),
        };
        let call_id = &ids.call_id;
        let replay_suffix = crate::runtime::causal::CommandSubKey::ToolAwait {
            call_id: call_id.clone(),
        }
        .to_string();
        let fallback;
        let parent = if let Some(parent) = parent_invocation.as_ref() {
            parent
        } else {
            fallback = crate::RuntimeInvocation::effect(
                crate::EffectAddress::new(
                    self.dispatch.effect_controller.execution_scope().clone(),
                    format!("tool:{replay_suffix}"),
                )
                .expect("tool await carries an admitted effect scope"),
                self.dispatch.parentless_attribution(),
                format!("tool:{replay_suffix}"),
            );
            &fallback
        };
        let parent_effect_id = parent.effect_id().unwrap_or("tool");
        let invocation = crate::runtime::causal::child_effect_invocation(
            self.dispatch.effect_controller.execution_scope(),
            parent,
            format!("{parent_effect_id}:{replay_suffix}"),
            replay_suffix,
        );
        // The await's journaled invocation is the settled call's observation
        // key: unique per (parent, call id) and re-derived identically on a
        // redrive (ADR 0105 §1).
        let call_key = invocation.effect_replay_key().to_owned();
        // Arm before parking, never after: the resolver the call named is what
        // makes the wait finishable, and this runs on the redrive too, because
        // the recorded attempt body that named it does not re-run.
        let cancellation = cancellation.unwrap_or_default();
        let site = crate::tool_dispatch::ParkSite {
            processes: self.dispatch.processes.as_ref(),
            owner: self.dispatch.owner.runtime_owner(),
            call_id,
            scope: self
                .process_scope(parent_invocation.clone())
                .with_turn_cancellation(&self.turn_cancel_wait(cancellation.clone())),
            child_trace_hook,
        };
        let (armed, resolved) =
            match crate::tool_dispatch::arm_pending_resolver(&site, &pending).await? {
                crate::tool_dispatch::ResolverArming::Armed(armed) => (armed, None),
                crate::tool_dispatch::ResolverArming::Resolved { resolution, armed } => {
                    (armed, Some(*resolution))
                }
                crate::tool_dispatch::ResolverArming::Settled { failure, armed } => {
                    return Ok(pending.settle_unarmed(*failure, &armed));
                }
            };
        let resolver = pending.pending.resolved_by.clone();
        let completion_key = pending.key.clone();
        let armed_deadline = self
            .dispatch
            .effect_controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::ArmToolCompletion {
                        key: pending.key.clone(),
                        timeout_ms: pending
                            .pending
                            .deadline
                            .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64),
                    },
                ),
                crate::RuntimeEffectLocalExecutor::await_event_under(
                    &self.turn_cancel_wait(cancellation),
                    None,
                    Arc::clone(&self.dispatch.clock),
                ),
            )
            .await?;
        let crate::RuntimeEffectOutcome::ArmToolCompletion { deadline_ms } = armed_deadline else {
            return Err(crate::RuntimeEffectControllerError::wrong_outcome(
                crate::RuntimeEffectKind::ArmToolCompletion,
                armed_deadline.kind(),
            ));
        };
        let outcome = if let Some(resolution) = resolved {
            Ok(resolution)
        } else {
            match self
                .await_deferred_tool_completions(
                    &format!("{call_key}:completion"),
                    vec![crate::ToolCompletionWait {
                        key: pending.key.clone(),
                        deadline_ms,
                    }],
                    None,
                    false,
                )
                .await
            {
                Ok(crate::ToolCompletionEvent::Resolved {
                    position: 0,
                    resolution,
                }) => Ok(resolution),
                Ok(_) => Err(crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    "a scalar tool completion wait returned an invalid branch",
                )),
                Err(error)
                    if error.code == crate::RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled =>
                {
                    if self.observe_turn_cancel {
                        self.note_turn_cancelled();
                    }
                    let terminal = self
                        .dispatch
                        .effect_controller
                        .controller()
                        .resolve_await_event(&pending.key, crate::Resolution::Cancelled)
                        .await?;
                    Ok(match terminal {
                        crate::ResolveOutcome::AlreadyResolved { terminal } => terminal,
                        _ => crate::Resolution::Cancelled,
                    })
                }
                Err(error) => Err(error),
            }
        };
        let resolution = match outcome {
            Ok(resolution) => resolution,
            Err(err) => {
                // An unrecorded `Err` from the completion wait — a live
                // controller fault (its claim or finalize failed) or a replay
                // divergence against its record — is a refusal, not the
                // tool's result: it returns to the caller, which aborts the
                // enclosing run and presents nothing, so neither a store
                // diagnostic nor a conflict commits or reaches the model as a
                // tool result (FIG-3528, FIG-3679). A journaled error is the
                // wait's recorded `Failed` terminal replaying and stays on
                // the result surface.
                if !err.journaled {
                    return Err(err);
                }
                let record = ToolCallRecord {
                    call_id: ids.call_id.clone(),
                    provider_call_id: ids.provider_call_id.clone(),
                    tool: pending.tool_name,
                    args: pending.args,
                    output: ToolCallOutput::failure(ToolFailure::runtime(
                        ToolFailureClass::Internal,
                        "pending_tool_completion_failed",
                        err.to_string(),
                    )),
                };
                let mut attempts = pending.attempts;
                attempts.push(crate::trace::trace_tool_attempt(
                    attempts
                        .len()
                        .saturating_add(1)
                        .try_into()
                        .unwrap_or(u32::MAX),
                    &record,
                    None,
                ));
                return Ok(ToolDispatchOutcome {
                    record,
                    attempts,
                    intents: crate::ToolIntents::default(),
                    intent_outcomes: armed.intent_outcomes(),
                    captures: pending.captures,
                    triggers: pending.triggers,
                });
            }
        };
        crate::tool_dispatch::finish_parked_wait(
            &site,
            &pending.pending,
            &armed,
            &completion_key,
            &resolution,
        )
        .await?;
        let mut outcome = self
            .pending_completion_dispatch_outcome(
                &ids,
                &call_key,
                pending.tool_name,
                pending.args,
                resolution,
                resolver.as_ref(),
                pending.attempts,
                pending.captures,
                pending.triggers,
            )
            .await;
        // A declared start's launch receipt is the call's one intent outcome
        // (ADR 0116 §3.8).
        outcome.intent_outcomes = armed.intent_outcomes();
        Ok(outcome)
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
        let executed = Box::pin(self.execute_command_tool(command, invocation)).await;
        let reply = ToolInvocationReply::from_output(executed.completed.output);
        reply.with_record(executed.record)
    }

    pub(crate) async fn execute_command_tool(
        &self,
        command: &crate::CommandReplayKey,
        mut invocation: ToolInvocation,
    ) -> CompletedProtocolToolCall {
        let authorization = ToolCallAuthorization::from_invocation(&mut invocation);
        let ToolInvocation {
            id,
            args,
            child_execution_trace_hook,
            issuing_language_node_id,
            ..
        } = invocation;
        let context = match issuing_language_node_id {
            Some(node_id) => self.clone().with_issuing_language_node_id(node_id),
            None => self.clone(),
        };
        let command = crate::runtime::command_invocation(
            self.dispatch.effect_controller.execution_scope(),
            self.effect_attribution(),
            self.parent_invocation.as_ref(),
            command,
        )
        .into_runtime_invocation();
        Box::pin(context.execute_tool_call(
            command,
            id,
            authorization,
            args,
            child_execution_trace_hook,
        ))
        .await
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

    async fn execute_tool_call(
        &self,
        command: crate::RuntimeInvocation,
        call_id: crate::ToolCallId,
        authorization: ToolCallAuthorization,
        args: serde_json::Value,
        child_execution_trace_hook: Option<crate::ToolChildExecutionTraceHook>,
    ) -> CompletedProtocolToolCall {
        let replay = None;
        let tool_correlation_id = tool_activity_id(&call_id);
        let ids = ToolCallIds {
            call_id: call_id.clone(),
            provider_call_id: None,
        };
        // The observed duration is this live window: measured here, carried
        // only onto the Completed observation — the recorded outcome holds no
        // wall-clock fields (FIG-3696).
        let call_started = self.dispatch.clock.now();
        let requested_at_ms = self.dispatch.clock.timestamp_ms();
        let elapsed_ms = |this: &Self| {
            this.dispatch
                .clock
                .now()
                .saturating_duration_since(call_started)
                .as_millis() as u64
        };
        // The command's replay key is the call's own effect-invocation key:
        // every lane this call emits keys under it (ADR 0105 §1).
        let call_key = command
            .effect_replay_key()
            .map_or_else(|| call_id.to_string(), str::to_owned);
        let Some(manifest) = authorization.resolve_manifest(self.dispatch.as_ref()) else {
            let tool_id = authorization.tool_id();
            let outcome = ToolDispatchOutcome {
                record: ToolCallRecord {
                    call_id: call_id.clone(),
                    provider_call_id: None,
                    tool: tool_id.to_string(),
                    args,
                    output: ToolCallOutput::failure(ToolFailure::runtime(
                        ToolFailureClass::Unavailable,
                        "tool_unavailable",
                        format!("Tool id `{tool_id}` is unavailable in this session"),
                    )),
                },
                attempts: Vec::new(),
                intents: crate::ToolIntents::default(),
                intent_outcomes: Vec::new(),
                captures: Vec::new(),
                triggers: Vec::new(),
            };
            if let Err(error) = self
                .retain_unadmitted_tool_request(&outcome.record, requested_at_ms)
                .await
            {
                self.record_nested_effect_error(error);
            }
            return self
                .complete_language_tool_call(
                    AdmittedCallIdentity(call_id, tool_id.clone()),
                    replay,
                    outcome,
                    true,
                    &call_key,
                    elapsed_ms(self),
                )
                .await;
        };
        let admitted_tool_id = authorization.tool_id().clone();
        let park_trace_hook = child_execution_trace_hook.clone();
        self.emit_tool_call_started(
            &call_key,
            &ids,
            &manifest.name,
            args.clone(),
            tool_correlation_id.clone(),
        )
        .await;

        let request = ToolCallRecord {
            call_id: call_id.clone(),
            provider_call_id: None,
            tool: manifest.name.clone(),
            args: args.clone(),
            output: ToolCallOutput::success(serde_json::Value::Null),
        };
        if let Err(error) = self
            .retain_unadmitted_tool_request(&request, requested_at_ms)
            .await
        {
            return self
                .refused_completion(
                    ids,
                    request.tool,
                    request.args,
                    error,
                    &call_key,
                    elapsed_ms(self),
                )
                .await;
        }

        let parent_invocation = Some(command.clone());
        let mut dispatch = (*self.dispatch).clone();
        dispatch.parent_invocation = parent_invocation.clone();
        dispatch.observation_call_key = None;
        let pending = crate::sansio::PendingToolCall {
            call_id: call_id.clone(),
            provider_call_id: None,
            tool_name: manifest.name.clone(),
            args,
            replay: replay.clone(),
        };
        let launch = match authorization.prepare(&dispatch, pending).await {
            ToolPreparationOutcome::Prepared(prepared) => {
                let retry_policy = authorization.recorded_retry_policy().unwrap_or_else(|| {
                    crate::tool_dispatch::resolve_retry_policy(&dispatch, &prepared.tool_id, None)
                });
                let execution_grant = authorization.into_execution_grant();
                let intent_trace_hook = child_execution_trace_hook.clone();
                let trace_hooks: BTreeMap<crate::ToolCallId, crate::ToolChildExecutionTraceHook> =
                    child_execution_trace_hook
                        .map(|hook| std::iter::once((call_id.clone(), hook)).collect())
                        .unwrap_or_default();
                let turn_cancel_wait = Box::new(
                    self.turn_cancel_wait(self.cancellation_token.clone().unwrap_or_default()),
                );
                let coordinated = coordinate_tool_invocation(
                    &dispatch,
                    *prepared,
                    execution_grant,
                    retry_policy,
                    None,
                    ToolAttemptLineage::under(command.clone()),
                    turn_cancel_wait.as_ref(),
                    intent_trace_hook,
                    |completion_key| {
                        crate::RuntimeEffectLocalExecutor::tool_attempt(
                            self.clone(),
                            trace_hooks.clone(),
                            completion_key,
                        )
                    },
                )
                .await;
                coordinated.launch
            }
            ToolPreparationOutcome::Completed(outcome) => ToolCallLaunch::Done(outcome),
        };
        let mut outcome = match launch {
            ToolCallLaunch::Done(outcome) => *outcome,
            ToolCallLaunch::Pending(pending) => {
                let (tool, args) = (pending.tool_name.clone(), pending.args.clone());
                match self
                    .complete_pending_tool_dispatch_outcome(
                        parent_invocation.clone(),
                        *pending,
                        self.cancellation_token.clone(),
                        park_trace_hook.as_ref(),
                    )
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        return self
                            .refused_completion(ids, tool, args, error, &call_key, elapsed_ms(self))
                            .await;
                    }
                }
            }
            // A refusal, not a settlement: the call has no recorded outcome to
            // present, so it journals no presentation (a fabricated one would
            // be replayed against the call's real result on the redrive).
            ToolCallLaunch::ControllerAborted(error) => {
                return self
                    .refused_completion(
                        ids,
                        "runtime_effect_controller".to_string(),
                        serde_json::Value::Null,
                        error,
                        &call_key,
                        elapsed_ms(self),
                    )
                    .await;
            }
        };
        outcome.record.call_id = call_id.clone();
        outcome.record.provider_call_id = None;

        self.complete_language_tool_call(
            AdmittedCallIdentity(call_id, admitted_tool_id),
            replay,
            outcome,
            false,
            &call_key,
            elapsed_ms(self),
        )
        .await
    }

    /// Delivers a named signal and JSON payload to a deferred tool handle for code-executor
    /// implementors.
    pub async fn signal_tool_handle(
        &self,
        call_id: crate::ToolCallId,
        handle: serde_json::Value,
        signal_name: String,
        payload: serde_json::Value,
    ) -> ToolInvocationReply {
        self.signal_process_handle(call_id, handle, signal_name, payload)
            .await
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
