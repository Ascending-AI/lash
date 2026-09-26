use super::execution_context::RuntimeExecutionContext;
use crate::tool_dispatch::{
    ToolAttemptEffectIdentity, ToolCallLaunch, ToolDispatchOutcome, ToolPreparationOutcome,
    coordinate_tool_invocation, prepare_granted_tool_call_with_context,
    prepare_tool_call_with_context,
};
use crate::{
    ModelToolReturn, SessionStreamEvent, ToolCallOutput, ToolCallRecord, ToolCancellation,
    ToolFailure, ToolFailureClass, TurnActivityId, TurnEvent,
};
use std::collections::HashMap;
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
/// The host-code batch path (`call_tool_batch` from a provider or
/// orchestrating-tool body) keeps content identity. Two structurally
/// identical batches raised from one such body still share an identity; that
/// body is ordinary host code with no deterministic count of its own.
const TOOL_BATCH_FAMILY_VERSION: u8 = 3;

enum ToolCallAuthorization {
    Catalog(crate::ToolId),
    Granted(Box<crate::ToolExecutionGrant>),
    /// A replayed code cell's call on a host tool binding that drifted since
    /// the pass that journaled the cell (FIG-3587): authorized under the
    /// cell's recorded binding instead of the live catalog, and otherwise the
    /// catalog call it was, so its attempt envelope is the recorded one.
    Recorded(Box<crate::ToolExecutionGrant>),
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
            Self::Granted(grant) | Self::Recorded(grant) => &grant.manifest().id,
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
            Self::Granted(grant) | Self::Recorded(grant) => Some(grant.manifest().clone()),
        }
    }

    async fn prepare(
        &self,
        dispatch: &crate::tool_dispatch::ToolDispatchContext<'_>,
        pending: crate::sansio::PendingToolCall,
        call_id: String,
    ) -> ToolPreparationOutcome {
        match self {
            Self::Catalog(_) => {
                prepare_tool_call_with_context(dispatch, pending, Some(call_id)).await
            }
            Self::Granted(grant) => {
                prepare_granted_tool_call_with_context(dispatch, grant, pending, Some(call_id))
                    .await
            }
            Self::Recorded(binding) => {
                crate::tool_dispatch::prepare_recorded_tool_call_with_context(
                    dispatch,
                    binding,
                    pending,
                    Some(call_id),
                )
                .await
            }
        }
    }

    /// A recorded binding orchestrates as the catalog call it replays did
    /// (FIG-3587): a drifted orchestrating tool re-runs its body against its
    /// recorded nested effects, each served only from the journal (FIG-3719).
    fn allows_orchestration(&self) -> bool {
        matches!(self, Self::Catalog(_) | Self::Recorded(_))
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
    fn into_execution_grant(
        self,
    ) -> (
        Option<Box<crate::ToolExecutionGrant>>,
        Option<Box<crate::ToolExecutionGrant>>,
    ) {
        match self {
            Self::Catalog(_) => (None, None),
            Self::Granted(grant) => (Some(grant.clone()), Some(grant)),
            Self::Recorded(binding) => (None, Some(binding)),
        }
    }
}

#[derive(Clone)]
pub struct ToolInvocation {
    pub id: String,
    pub tool_id: crate::ToolId,
    pub args: serde_json::Value,
    pub execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    /// The binding a replayed code cell recorded for this call when the live
    /// tool has since drifted (FIG-3587). Never part of the call's identity.
    pub recorded_binding: Option<Box<crate::ToolExecutionGrant>>,
    pub child_execution_trace_hook: Option<crate::ToolChildExecutionTraceHook>,
    pub issuing_language_node_id: Option<String>,
}

impl ToolInvocation {
    pub fn new(id: impl Into<String>, tool_id: crate::ToolId, args: serde_json::Value) -> Self {
        Self {
            id: id.into(),
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
    pub fn with_recorded_binding(mut self, binding: crate::ToolExecutionGrant) -> Self {
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
            id,
            crate::ToolId::from("tool:test"),
            serde_json::json!({"value": value}),
        )
    }

    /// The v1 identity of `[invocation("a", 1), invocation("b", 2)]`, recorded
    /// so it can be refused rather than re-derived. Every assertion below that
    /// names it asserts it is *not* minted: an identity a v1 journal holds must
    /// not be reachable from this build under any occurrence, or the two
    /// generations would share a replay key and the second batch would read the
    /// first one's journalled outcome.
    const PREDECESSOR_BATCH_ID: &str =
        "tool-batch:v1:blake3:4095297ab62f9013e4325b7345584d72ffae0c9f1b5882053c5895c438b74a90";

    #[test]
    fn deterministic_batch_identity_is_stable_and_content_addressed() {
        let calls = vec![invocation("a", 1), invocation("b", 2)];
        let first = deterministic_tool_invocation_batch_id(&calls);
        let retry = deterministic_tool_invocation_batch_id(&calls);
        assert_eq!(first, retry);
        assert_eq!(
            first,
            "tool-batch:v3:blake3:487c96dd97c200501345a95f547a956a34cd2784884d3c0c0590f7c48689c657"
        );
        assert_eq!(
            hex(&tool_invocation_batch_preimage(&calls)),
            "6c6173682d737461626c652d6964656e746974790203000000000000001a6c6173682e746f6f6c2d696e766f636174696f6e2d626174636800000000000000020000000000000001610000000000000009746f6f6c3a74657374000000000000000b7b2276616c7565223a317d000000000000000001620000000000000009746f6f6c3a74657374000000000000000b7b2276616c7565223a327d00"
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

    /// v3 is content identity again (FIG-3586): the same calls mint one
    /// identity, and it is none a v1 or v2 journal holds, so no predecessor
    /// generation shares a key with this one.
    #[test]
    fn content_identity_mints_no_predecessor_identity() {
        let calls = vec![invocation("a", 1), invocation("b", 2)];
        let minted = deterministic_tool_invocation_batch_id(&calls);
        assert_ne!(minted, PREDECESSOR_BATCH_ID);
        assert_ne!(
            minted,
            "tool-batch:v2:blake3:2506ef842e2e5214ee5b3cbfce7596d3cf85f2f0dfd8560179f7b5f1b2c45639",
            "the v2 identity of the same calls at occurrence 1 must not be re-minted"
        );
    }

    #[test]
    fn granted_batch_identity_pins_present_grant_routing_grammar() {
        let grant = crate::ToolExecutionGrant::from_definition(crate::ToolDefinition::raw(
            "tool:granted",
            "granted",
            "golden",
            serde_json::json!({"type": "object"}),
            serde_json::json!({"type": "string"}),
        ))
        .with_source_id("plugin\0route")
        .with_execution_binding(serde_json::json!({"route": ["λ", -0.0]}));
        let calls = vec![
            ToolInvocation::new(
                "grant\0call",
                crate::ToolId::from("tool:granted"),
                serde_json::json!({"value": true}),
            )
            .with_execution_grant(grant),
        ];
        assert_eq!(
            hex(&tool_invocation_batch_preimage(&calls)),
            "6c6173682d737461626c652d6964656e746974790203000000000000001a6c6173682e746f6f6c2d696e766f636174696f6e2d62617463680000000000000001000000000000000a6772616e740063616c6c000000000000000c746f6f6c3a6772616e746564000000000000000e7b2276616c7565223a747275657d01000000000000000c746f6f6c3a6772616e74656401000000000000000c706c7567696e00726f75746500000000000000147b22726f757465223a5b22cebb222c302e305d7d"
        );
        assert_eq!(
            deterministic_tool_invocation_batch_id(&calls),
            "tool-batch:v3:blake3:6f304c2ae6527a194e82af6ffe53d6777d405ded66b0809bf74758e8d3de9262"
        );

        let without_source =
            crate::ToolExecutionGrant::from_definition(crate::ToolDefinition::raw(
                "tool:granted",
                "granted",
                "golden",
                serde_json::json!({"type": "object"}),
                serde_json::json!({"type": "string"}),
            ))
            .with_execution_binding(serde_json::json!({"route": ["λ", -0.0]}));
        let without_source = vec![
            ToolInvocation::new(
                "grant\0call",
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

    #[test]
    fn cancelled_tool_call_preserves_protocol_identity_and_typed_outcome() {
        let completed = cancelled_completed_tool_call(
            "call".to_string(),
            "tool".to_string(),
            serde_json::json!({"arg": true}),
            None,
        );
        assert_eq!(completed.call_id, "call");
        assert_eq!(completed.tool_name, "tool");
        assert_eq!(completed.model_return.call_id, "call");
        assert_eq!(
            completed.output.status(),
            lash_sansio::ToolCallStatus::Cancelled
        );
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

#[derive(Clone, Debug)]
pub struct CompletedProtocolToolCall {
    pub completed: crate::sansio::CompletedToolCall,
    pub record: ToolCallRecord,
}

fn cancelled_completed_tool_call(
    call_id: String,
    tool_name: String,
    args: serde_json::Value,
    replay: Option<crate::llm::types::ProviderReplayMeta>,
) -> crate::sansio::CompletedToolCall {
    let output = ToolCallOutput::cancelled(ToolCancellation::runtime("tool call cancelled"));
    crate::sansio::CompletedToolCall {
        call_id: call_id.clone(),
        tool_name: tool_name.clone(),
        args,
        model_return: ModelToolReturn {
            call_id,
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
        identity.string(id);
        identity.string(tool_id.as_str());
        identity.bytes(&crate::identity_json::payload_leaf(args));
        identity.optional(execution_grant.as_deref(), |identity, grant| {
            // This exhaustive destructure is the guard: adding a grant field must fail
            // compilation here until its batch-identity inclusion is ruled in or out.
            let crate::ToolExecutionGrant {
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
                compact_contract: _,
                activation: _,
                bindings: _,
                argument_projection: _,
                retry_policy: _,
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

pub(crate) fn deterministic_tool_invocation_batch_id(calls: &[ToolInvocation]) -> String {
    crate::stable_identity::rendered_hash(
        "tool-batch",
        TOOL_BATCH_FAMILY_VERSION,
        &tool_invocation_batch_preimage(calls),
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

pub(crate) fn tool_activity_id(call_id: &str) -> TurnActivityId {
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
};
pub use group::ToolAggregateConsumer;
#[cfg(test)]
#[path = "tool_execution/turn_cancel_gate_tests.rs"]
mod turn_cancel_gate_tests;

#[cfg(test)]
#[path = "tool_execution/scalar_presentation_tests.rs"]
mod scalar_presentation_tests;

impl RuntimeExecutionContext<'_> {
    /// `call_key` is the material the call's observation lanes key under —
    /// the call's own effect-invocation replay key where it has one (a group
    /// child's `{group}:child:{position}`, a command's key), else the
    /// caller's positional material (`{iteration}:{index}:{call_id}` on the
    /// protocol path, `{batch_id}:{index}:{call_id}` in a batch) — qualified
    /// against this context's observation base (ADR 0105 §1).
    pub(crate) fn emit_tool_call_started(
        &self,
        call_key: &str,
        call_id: &str,
        name: &str,
        args: serde_json::Value,
        activity_id: TurnActivityId,
    ) {
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        let mut cursor = context.observation_cursor(&format!("tool:{call_id}:start"));
        cursor.observe(
            context.dispatch.observer.as_ref(),
            crate::engine::ObservedEvent::Session(SessionStreamEvent::ToolCallStart {
                call_id: Some(call_id.to_string()),
                name: name.to_string(),
                args: args.clone(),
            }),
        );
        self.emit_tool_call_started_trace(call_id, name, &args);
        cursor.observe(
            context.dispatch.observer.as_ref(),
            crate::engine::ObservedEvent::Activity {
                correlation_id: Some(activity_id),
                event: TurnEvent::ToolCallStarted {
                    call_id: Some(call_id.to_string()),
                    name: name.to_string(),
                    args,
                    graph_key: self.code_block_graph_key(),
                    parent_call_id: self.batch_parent_call_id(),
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
        let call_id = Some(pending.call_id.clone());
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        prepare_tool_call_with_context(context.dispatch.as_ref(), pending, call_id).await
    }

    /// Prepares a call on a tool of the turn's recorded surface whose live
    /// definition drifted (FIG-3672 P7b): under `binding`, the recorded
    /// definition, with identity preparation, so the call's envelope is the
    /// one the journal recorded and the live tool is not consulted.
    pub async fn prepare_recorded_tool_call(
        &self,
        binding: &crate::ToolExecutionGrant,
        pending: crate::sansio::PendingToolCall,
        call_key: &str,
    ) -> ToolPreparationOutcome {
        let call_id = Some(pending.call_id.clone());
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        crate::tool_dispatch::prepare_recorded_tool_call_with_context(
            context.dispatch.as_ref(),
            binding,
            pending,
            call_id,
        )
        .await
    }

    /// The catalog entry a model-issued call names, if the catalog holds it.
    pub fn callable_tool_id_by_name(&self, tool_name: &str) -> Option<crate::ToolId> {
        self.dispatch
            .tool_catalog
            .tools
            .iter()
            .find(|tool| {
                tool.manifest.name == tool_name
                    && tool.manifest.activation != crate::ToolActivation::Internal
            })
            .map(|tool| tool.manifest.id.clone())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn execute_prepared_tool_attempt_effect(
        &self,
        prepared: crate::PreparedToolCall,
        execution_grant: Option<Box<crate::ToolExecutionGrant>>,
        attempt: u32,
        max_attempts: u32,
        attempt_invocation: crate::RuntimeInvocation,
        child_execution_trace_hook: Option<crate::ToolChildExecutionTraceHook>,
        completion_key: Option<crate::AwaitEventKey>,
    ) -> Result<crate::ToolAttemptEffectOutcome, crate::RuntimeEffectControllerError> {
        let mut attempt_dispatch = (*self.dispatch).clone();
        attempt_dispatch.parent_invocation = Some(attempt_invocation.clone());
        // The attempt's invocation is now the observation base; an inherited
        // per-call key would key every retry of it under the caller's lane.
        attempt_dispatch.observation_call_key = None;
        attempt_dispatch.direct_completions = attempt_dispatch
            .direct_completions
            .with_tool_attempt_parent_invocation(attempt_invocation.clone())
            .with_usage_ledger(crate::runtime::ToolUsageLedger::for_attempt(attempt));
        attempt_dispatch.trigger_outcomes =
            crate::tool_dispatch::ToolTriggerOutcomeBuffer::default();
        // Attempt-local: what this attempt commits is journaled on its
        // outcome's capture rather than read out of the shared buffer.
        attempt_dispatch.checkpoint_messages =
            crate::tool_dispatch::CheckpointMessageBuffer::default();
        let attempt_dispatch = std::sync::Arc::new(attempt_dispatch);
        let mut attempt_context = self.clone();
        attempt_context.dispatch = std::sync::Arc::clone(&attempt_dispatch);
        attempt_context.parent_invocation = Some(attempt_invocation.clone());

        // The attempt is a recorded step its engine cannot select away: its
        // body watches the turn's gate itself and gets the stop as its token,
        // so the recorded outcome says whether the stop won (FIG-3672 P9).
        Box::pin(self.run_turn_step_body(|stop| {
            self.execute_prepared_tool_attempt_body(
                prepared,
                execution_grant,
                attempt,
                max_attempts,
                attempt_invocation,
                child_execution_trace_hook,
                completion_key,
                attempt_dispatch,
                attempt_context,
                stop,
            )
        }))
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_prepared_tool_attempt_body(
        &self,
        prepared: crate::PreparedToolCall,
        execution_grant: Option<Box<crate::ToolExecutionGrant>>,
        attempt: u32,
        max_attempts: u32,
        attempt_invocation: crate::RuntimeInvocation,
        child_execution_trace_hook: Option<crate::ToolChildExecutionTraceHook>,
        completion_key: Option<crate::AwaitEventKey>,
        attempt_dispatch: std::sync::Arc<crate::tool_dispatch::ToolDispatchContext<'_>>,
        attempt_context: Self,
        stop: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<crate::ToolAttemptEffectOutcome, crate::RuntimeEffectControllerError> {
        let mut tool_context =
            crate::ToolContext::from_dispatch(std::sync::Arc::clone(&attempt_dispatch))
                .runtime_execution_context(attempt_context.clone())
                .prepared_call(&prepared)
                .cancellation_token(stop)
                .enclosing_process(self.process_id().map(String::from).map(Into::into))
                .parent_invocation(Some(attempt_invocation))
                .child_execution_trace_hook(child_execution_trace_hook);
        if let Some(process_id) = self.process_id()
            && let Some(process_events) = self.process_event_context()
        {
            tool_context = tool_context.process_events(
                process_id,
                process_events.execution_write_authority.clone(),
                process_events.process_work.clone(),
                process_events.store.clone(),
                process_events.session_store_factory.clone(),
                std::sync::Arc::clone(&process_events.queued_work),
                process_events.process_wake_delivery_policy,
                std::sync::Arc::clone(&process_events.clock),
            );
        }
        let tool_context = tool_context.build();
        tool_context.install_prederived_completion_key(completion_key);
        Box::pin(crate::tool_dispatch::execute_prepared_tool_attempt_effect(
            attempt_dispatch.as_ref(),
            prepared,
            execution_grant,
            attempt,
            max_attempts,
            tool_context,
        ))
        .await
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
        call_id: String,
        replay: Option<crate::llm::types::ProviderReplayMeta>,
        outcome: ToolDispatchOutcome,
        call_key: &str,
        duration_ms: u64,
    ) -> Result<CompletedProtocolToolCall, crate::RuntimeEffectControllerError> {
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        let this = &context;
        let tool_correlation_id = tool_activity_id(&call_id);
        let attempts = outcome.attempts.clone();
        let mut output = outcome.record.output.clone();
        // The settlement exists before the chain so a step can read its facts;
        // its `model_return` is overwritten by the presented return below.
        let mut settlement = crate::runtime::effect::ToolSettlement::from_dispatch(
            &outcome,
            ModelToolReturn::from_output(call_id.clone(), outcome.record.tool.clone(), &output),
        );
        // The presentation boundary (ADR 0099 §6, FIG-3420): the ordered
        // presentation steps run once through the journaled `PresentToolResult`
        // effect, keyed by `{call_id}:present`, so a replay serves the recorded
        // `ToolPresentation` and never re-runs a step.
        let presentation_replay_key = format!("{call_id}:present");
        let scoped = self.dispatch.effect_controller.scoped();
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
                            call_id: call_id.clone(),
                            tool_name: outcome.record.tool.clone(),
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
            replay_key: call_id.clone(),
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
            for intent_outcome in &outcome.intent_outcomes {
                model_return.parts.push(crate::ModelToolReturnPart::text(
                    intent_outcome.model_addendum(),
                ));
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
            call_id: Some(call_id.clone()),
            tool: outcome.record.tool.clone(),
            args: outcome.record.args.clone(),
            output: output.clone(),
        };
        this.emit_tool_call_completed(call_key, &record, &attempts, duration_ms);
        Ok(CompletedProtocolToolCall {
            completed: crate::sansio::CompletedToolCall {
                call_id,
                tool_name: outcome.record.tool,
                args: outcome.record.args,
                output,
                model_return,
                intent_outcomes: outcome.intent_outcomes,
                replay,
            },
            record,
        })
    }

    /// `call_key` is the material the call's observation lanes key under; see
    /// [`Self::emit_tool_call_started`]. `duration_ms` is the measured
    /// wall-clock the caller observed for the call — an observation-only
    /// value: recorded content carries no durations, so it arrives on this
    /// path rather than on the record (FIG-3696).
    fn emit_tool_call_completed(
        &self,
        call_key: &str,
        record: &ToolCallRecord,
        attempts: &[lash_trace::TraceRetryAttempt],
        duration_ms: u64,
    ) {
        self.emit_tool_call_completed_trace(record, attempts, duration_ms);
        let context = self.with_call_observation_key(self.call_observation_key(call_key));
        let mut cursor = context.observation_cursor(&format!(
            "tool:{}:complete",
            record.call_id.as_deref().unwrap_or_default()
        ));
        cursor.observe(
            context.dispatch.observer.as_ref(),
            crate::engine::ObservedEvent::Activity {
                correlation_id: Some(tool_activity_id(
                    record.call_id.as_deref().unwrap_or_default(),
                )),
                event: TurnEvent::ToolCallCompleted {
                    call_id: record.call_id.clone(),
                    name: record.tool.clone(),
                    args: record.args.clone(),
                    output: record.output.clone(),
                    duration_ms,
                    graph_key: self.code_block_graph_key(),
                    parent_call_id: self.batch_parent_call_id(),
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
        call_id: String,
        replay: Option<crate::llm::types::ProviderReplayMeta>,
        outcome: ToolDispatchOutcome,
        call_key: &str,
        duration_ms: u64,
    ) -> Result<CompletedProtocolToolCall, crate::RuntimeEffectControllerError> {
        self.emit_tool_call_started(
            call_key,
            &call_id,
            &outcome.record.tool,
            outcome.record.args.clone(),
            tool_activity_id(&call_id),
        );
        self.complete_tool_call(call_id, replay, outcome, call_key, duration_ms)
            .await
    }

    /// The completion a language runtime's call answers when a controller
    /// refused it — its attempt or its presentation: the error is recorded as
    /// the run's nested effect error, which stops the run at this command (a
    /// replay divergence parks it), and the call settles a failure that was
    /// never presented or journaled.
    pub(crate) async fn refused_completion(
        &self,
        call_id: String,
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
            call_id: Some(call_id.clone()),
            tool: tool.clone(),
            args: args.clone(),
            output: output.clone(),
        };
        self.emit_tool_call_completed(call_key, &record, &[], duration_ms);
        CompletedProtocolToolCall {
            completed: crate::sansio::CompletedToolCall {
                model_return: ModelToolReturn::from_output(call_id.clone(), tool.clone(), &output),
                call_id,
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
        call_id: String,
        replay: Option<crate::llm::types::ProviderReplayMeta>,
        outcome: ToolDispatchOutcome,
        undispatched: bool,
        call_key: &str,
        duration_ms: u64,
    ) -> CompletedProtocolToolCall {
        let (tool, args) = (outcome.record.tool.clone(), outcome.record.args.clone());
        // A run that already recorded a nested effect error aborts: this call
        // was settled from inside it — an orchestrating body whose nested call
        // was refused, a sibling's divergence — so it presents and journals
        // nothing of its own (FIG-3679).
        if let Some(error) = self.peek_nested_effect_error() {
            return self
                .refused_completion(call_id, tool, args, error, call_key, duration_ms)
                .await;
        }
        // Boxed: the presentation future would otherwise inflate every
        // language runtime's call future past clippy's large-future bound.
        let completed = if undispatched {
            Box::pin(self.complete_undispatched_tool_call(
                call_id.clone(),
                replay,
                outcome,
                call_key,
                duration_ms,
            ))
            .await
        } else {
            Box::pin(self.complete_tool_call(
                call_id.clone(),
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
                Box::pin(self.refused_completion(call_id, tool, args, error, call_key, duration_ms))
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
        self.emit_tool_call_started(
            call_key,
            &completed.call_id,
            &completed.tool_name,
            completed.args.clone(),
            tool_activity_id(&completed.call_id),
        );
        // The call completed host-side; no measured window exists on this
        // path, so the observation reports 0 rather than a live clock read
        // made long after the work ran (FIG-3696).
        self.emit_tool_call_completed(
            call_key,
            &ToolCallRecord {
                call_id: Some(completed.call_id.clone()),
                tool: completed.tool_name.clone(),
                args: completed.args.clone(),
                output: completed.output.clone(),
            },
            &[],
            0,
        );
    }

    /// `call_key` is the material the settled call's observation lanes key
    /// under — its await's journaled invocation replay key; see
    /// [`Self::emit_tool_call_started`].
    #[allow(clippy::too_many_arguments)]
    pub async fn pending_completion_dispatch_outcome(
        &self,
        call_id: &str,
        call_key: &str,
        tool_name: String,
        args: serde_json::Value,
        resolution: crate::Resolution,
        resolver: Option<&crate::PendingResolver>,
        attempts: Vec<lash_trace::TraceRetryAttempt>,
        mut captures: Vec<crate::runtime::ToolAttemptCapture>,
        mut triggers: Vec<crate::tool_dispatch::ToolTriggerEffectOutcome>,
    ) -> ToolDispatchOutcome {
        // The resume's own producers — the after-tool hook's directives —
        // write into buffers fresh to this resume, so what they commit is
        // captured into the outcome rather than into a buffer a sibling
        // attempt may still be writing into. The hook's duration input is an
        // observation of this resume's window: the journaled pending row
        // carries no clock facts (FIG-3696).
        let settle_started = self.dispatch.clock.now();
        let mut resumed_dispatch = (*self.dispatch).clone();
        resumed_dispatch.observation_call_key = Some(self.call_observation_key(call_key));
        resumed_dispatch.checkpoint_messages =
            crate::tool_dispatch::CheckpointMessageBuffer::default();
        resumed_dispatch.trigger_outcomes =
            crate::tool_dispatch::ToolTriggerOutcomeBuffer::default();
        let usage_ledger = crate::runtime::ToolUsageLedger::new();
        resumed_dispatch.direct_completions = resumed_dispatch
            .direct_completions
            .clone()
            .with_usage_ledger(usage_ledger.clone());
        let mut outcome = crate::tool_dispatch::settle_completed_pending_tool_call(
            &resumed_dispatch,
            call_id,
            tool_name,
            args,
            resolution,
            resolver,
            self.dispatch
                .clock
                .now()
                .saturating_duration_since(settle_started)
                .as_millis() as u64,
            attempts,
        )
        .await;
        triggers.extend(resumed_dispatch.trigger_outcomes.drain());
        let capture = crate::runtime::ToolAttemptCapture {
            version: crate::runtime::TOOL_ATTEMPT_CAPTURE_VERSION,
            messages: resumed_dispatch.checkpoint_messages.drain(),
            usage: usage_ledger.take(),
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
    async fn await_pending_tool_dispatch_outcome_with_suffix(
        &self,
        call_id: &str,
        parent_invocation: Option<crate::RuntimeInvocation>,
        replay_suffix: String,
        pending: crate::tool_dispatch::PendingToolDispatchOutcome,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<ToolDispatchOutcome, crate::RuntimeEffectControllerError> {
        let fallback;
        let parent = if let Some(parent) = parent_invocation.as_ref() {
            parent
        } else {
            fallback = crate::RuntimeInvocation::effect(
                crate::EffectAddress::new(
                    self.dispatch
                        .effect_controller
                        .scoped()
                        .execution_scope()
                        .clone(),
                    format!("tool:{call_id}:await"),
                )
                .expect("tool await carries an admitted effect scope"),
                self.dispatch.parentless_attribution(),
                format!("tool:{call_id}:await"),
            );
            &fallback
        };
        let parent_effect_id = parent.effect_id().unwrap_or("tool");
        let invocation = crate::runtime::causal::child_effect_invocation(
            self.dispatch.effect_controller.scoped().execution_scope(),
            parent,
            format!("{parent_effect_id}:{replay_suffix}"),
            replay_suffix,
        );
        // The await's journaled invocation is the settled call's observation
        // key: unique per (parent, call id) and re-derived identically on a
        // redrive (ADR 0105 §1).
        let call_key = invocation.replay_key().to_owned();
        // Arm before parking, never after: the resolver the call named is what
        // makes the wait finishable, and this runs on the redrive too, because
        // the recorded attempt body that named it does not re-run.
        if let Err(err) = crate::tool_dispatch::arm_pending_resolver(
            self.dispatch.processes.as_ref(),
            &pending.pending,
            &pending.key,
            self.process_scope(parent_invocation.clone()),
        )
        .await
        {
            return Ok(Self::unarmed_pending_outcome(pending, err));
        }
        let cancellation = cancellation.unwrap_or_default();
        let resolver = pending.pending.resolved_by.clone();
        let deadline = pending
            .pending
            .deadline
            .map(|duration| self.dispatch.clock.now() + duration);
        let outcome = self
            .dispatch
            .effect_controller
            .scoped()
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::AwaitEvent { key: pending.key },
                ),
                crate::RuntimeEffectLocalExecutor::await_event_under(
                    &self.turn_cancel_wait(cancellation),
                    deadline,
                    std::sync::Arc::clone(&self.dispatch.clock),
                ),
            )
            .await;
        let resolution = match outcome.and_then(crate::RuntimeEffectOutcome::into_await_event) {
            Ok(resolution) => resolution,
            Err(err) => {
                // An unrecorded `Err` from the journaled `AwaitEvent` — a live
                // controller fault (its claim or finalize failed) or a replay
                // divergence against its record — is a refusal, not the
                // tool's result: it returns to the caller, which aborts the
                // enclosing run and presents nothing, so neither a store
                // diagnostic nor a conflict commits or reaches the model as a
                // tool result (FIG-3528, FIG-3679). A journaled error is the
                // await's recorded `Failed` terminal replaying and stays on
                // the result surface.
                if !err.journaled {
                    return Err(err);
                }
                let record = ToolCallRecord {
                    call_id: None,
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
                    intent_outcomes: Vec::new(),
                    captures: pending.captures,
                    triggers: pending.triggers,
                });
            }
        };
        Ok(self
            .pending_completion_dispatch_outcome(
                call_id,
                &call_key,
                pending.tool_name,
                pending.args,
                resolution,
                resolver.as_ref(),
                pending.attempts,
                pending.captures,
                pending.triggers,
            )
            .await)
    }

    pub fn restore_tool_trigger_outcomes(
        &self,
        outcomes: Vec<crate::tool_dispatch::ToolTriggerEffectOutcome>,
    ) {
        for outcome in outcomes {
            self.dispatch.trigger_outcomes.enqueue(outcome);
        }
    }

    /// Fails a call whose named resolver could not be armed.
    ///
    /// Deliberately a failure and not a park: a wait nobody is going to resolve
    /// is indistinguishable from a hang, and the turn would hold until it was
    /// cancelled. Reporting it here keeps the fault at the site that knows what
    /// it was trying to arm.
    fn unarmed_pending_outcome(
        pending: crate::tool_dispatch::PendingToolDispatchOutcome,
        error: crate::PluginError,
    ) -> ToolDispatchOutcome {
        let record = ToolCallRecord {
            call_id: None,
            tool: pending.tool_name,
            args: pending.args,
            output: ToolCallOutput::failure(ToolFailure::runtime(
                ToolFailureClass::Internal,
                "pending_tool_resolver_unarmed",
                format!("the declared resolver for this call could not be armed: {error}"),
            )),
        };
        ToolDispatchOutcome {
            record,
            attempts: pending.attempts,
            intents: crate::ToolIntents::default(),
            intent_outcomes: Vec::new(),
            captures: pending.captures,
            triggers: pending.triggers,
        }
    }

    pub(crate) async fn await_process_with_cancellation(
        &self,
        process_ref: &crate::ProcessRef,
        parent_invocation: Option<crate::RuntimeInvocation>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        let _phase = self.named_phase("process.await_handle");
        let cancellation = cancellation.unwrap_or_default();
        let process_scope = self
            .process_scope(parent_invocation)
            .with_turn_cancellation(&self.turn_cancel_wait(cancellation));
        crate::runtime::release_process_execution_permit_while(
            self.dispatch
                .processes
                .await_process_ref(process_ref, process_scope),
        )
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
            self.dispatch.effect_controller.scoped().execution_scope(),
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
        call_id: String,
        handle: serde_json::Value,
    ) -> ToolInvocationReply {
        self.cancel_process_handle(call_id, handle).await
    }

    /// Awaits a deferred tool handle for code-executor implementors without re-executing the call.
    pub async fn await_tool_handle(
        &self,
        call_id: String,
        handle: serde_json::Value,
    ) -> ToolInvocationReply {
        self.await_process_handle(call_id, handle).await
    }

    async fn execute_tool_call(
        &self,
        command: crate::RuntimeInvocation,
        call_id: String,
        authorization: ToolCallAuthorization,
        args: serde_json::Value,
        child_execution_trace_hook: Option<crate::ToolChildExecutionTraceHook>,
    ) -> CompletedProtocolToolCall {
        let replay = None;
        let tool_correlation_id = tool_activity_id(&call_id);
        // The observed duration is this live window: measured here, carried
        // only onto the Completed observation — the recorded outcome holds no
        // wall-clock fields (FIG-3696).
        let call_started = self.dispatch.clock.now();
        let elapsed_ms = |this: &Self| {
            this.dispatch
                .clock
                .now()
                .saturating_duration_since(call_started)
                .as_millis() as u64
        };
        // The command's replay key is the call's own effect-invocation key:
        // every lane this call emits keys under it (ADR 0105 §1), so a model
        // repeating a `call_id` across commands mints distinct observations.
        let call_key = command
            .replay_key()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("command:{call_id}"));
        let Some(manifest) = authorization.resolve_manifest(self.dispatch.as_ref()) else {
            let tool_id = authorization.tool_id();
            let outcome = ToolDispatchOutcome {
                record: ToolCallRecord {
                    call_id: Some(call_id.clone()),
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
            return self
                .complete_language_tool_call(
                    call_id,
                    replay,
                    outcome,
                    true,
                    &call_key,
                    elapsed_ms(self),
                )
                .await;
        };
        self.emit_tool_call_started(
            &call_key,
            &call_id,
            &manifest.name,
            args.clone(),
            tool_correlation_id.clone(),
        );

        let parent_invocation = Some(command.clone());
        let mut dispatch = (*self.dispatch).clone();
        dispatch.parent_invocation = parent_invocation.clone();
        dispatch.observation_call_key = None;
        let pending = crate::sansio::PendingToolCall {
            call_id: call_id.clone(),
            tool_name: manifest.name.clone(),
            args,
            replay: replay.clone(),
        };
        let launch = match authorization
            .prepare(&dispatch, pending, call_id.clone())
            .await
        {
            ToolPreparationOutcome::Prepared(prepared) => {
                if authorization.allows_orchestration()
                    && self.dispatch.is_orchestrating_tool(&prepared.tool_id)
                {
                    #[expect(
                        clippy::expect_used,
                        reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
                    )]
                    let tool_context =
                        crate::ToolContext::from_dispatch(Arc::new(dispatch.clone()))
                            .prepared_call(&prepared)
                            .cancellation_token(self.cancellation_token.clone())
                            .runtime_execution_context(self.clone().with_parent_invocation(
                                parent_invocation.clone().unwrap_or_else(|| {
                                    crate::RuntimeInvocation::effect(
                                        crate::EffectAddress::new(
                                            dispatch
                                                .effect_controller
                                                .scoped()
                                                .execution_scope()
                                                .clone(),
                                            format!("orchestration:{call_id}"),
                                        )
                                        .expect("orchestration carries an admitted effect scope"),
                                        dispatch.parentless_attribution(),
                                        format!("orchestration:{call_id}"),
                                    )
                                }),
                            ))
                            .parent_invocation(parent_invocation.clone())
                            .child_execution_trace_hook(child_execution_trace_hook.clone())
                            .build();
                    ToolCallLaunch::Done(Box::new(
                        Box::pin(crate::tool_dispatch::execute_orchestrating_tool(
                            &dispatch,
                            *prepared,
                            tool_context,
                        ))
                        .await,
                    ))
                } else {
                    let (execution_grant, retry_grant) = authorization.into_execution_grant();
                    let retry_policy = crate::tool_dispatch::resolve_retry_policy(
                        &dispatch,
                        &prepared.tool_id,
                        retry_grant.as_deref(),
                    );
                    let intent_trace_hook = child_execution_trace_hook.clone();
                    let trace_hooks: HashMap<String, crate::ToolChildExecutionTraceHook> =
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
                        ToolAttemptEffectIdentity::Command {
                            command: command.clone(),
                        },
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
            }
            ToolPreparationOutcome::Completed(outcome) => ToolCallLaunch::Done(outcome),
        };
        let mut outcome = match launch {
            ToolCallLaunch::Done(outcome) => *outcome,
            ToolCallLaunch::Pending(pending) => {
                let (tool, args) = (pending.tool_name.clone(), pending.args.clone());
                match self
                    .await_pending_tool_dispatch_outcome_with_suffix(
                        &call_id,
                        parent_invocation.clone(),
                        "await".to_string(),
                        *pending,
                        self.cancellation_token.clone(),
                    )
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        return self
                            .refused_completion(
                                call_id,
                                tool,
                                args,
                                error,
                                &call_key,
                                elapsed_ms(self),
                            )
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
                        call_id,
                        "runtime_effect_controller".to_string(),
                        serde_json::Value::Null,
                        error,
                        &call_key,
                        elapsed_ms(self),
                    )
                    .await;
            }
        };
        outcome.record.call_id = Some(call_id.clone());

        self.complete_language_tool_call(
            call_id,
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
        call_id: String,
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
        let mut model_return = crate::ModelToolReturn::from_output(
            "call".to_string(),
            "workspace_badge".to_string(),
            &output,
        );

        surface_attachment_materialization_notices(
            &crate::attachments::attachment_test_capability().attachment_acceptance,
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
