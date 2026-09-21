//! The durable request that reconstructs one tool child of an effect group
//! (ADR 0099 §3, FIG-3408).
//!
//! # Why a command variant exists at all
//!
//! ADR 0065 shipped groups over ordinary envelopes and recorded the reason in
//! [`RuntimeEffectGroup`](super::group::RuntimeEffectGroup): "groups introduce
//! no new command variant, because what is new is the *composition above*
//! attempts, not the attempts." That held while every child was something the
//! journal could already name — a sleep, a process command, an await.
//!
//! It stops holding for a **tool** child. ADR 0099 §2 makes a tool child a
//! replayable *invocation driver*: retry, completion-key derivation, deferred
//! await and the orchestrating lane are coordination that runs at handler
//! level, and only the atomic attempt runs inside a recorded body. Neither tool
//! command the journal has names that.
//! [`ToolAttempt`](super::envelope::RuntimeEffectCommand::ToolAttempt) is the
//! atomic body itself — one attempt of one call, the thing that goes inside
//! `ctx.run` — so a driver expressed as a `ToolAttempt` could not retry, because
//! a second attempt is a second envelope with a second hash.
//! [`ToolBatch`](super::envelope::RuntimeEffectCommand::ToolBatch) is the whole
//! batch, every leaf at once, which is the composition a group replaces. A tool
//! group child is neither, so it has nothing to name, and a group whose child
//! cannot be named cannot be retained (§3) or recovered (W1, W2).
//!
//! [`ToolChildRequest`] is that name, and it is deliberately **one** durable
//! shape rather than two. §3's list — input, replay identity, admitted grant,
//! the exact opener and scope, lineage, cancellation authority and the
//! environment reference — is exactly the input a handler-level driver needs to
//! run the child with no caller in scope. Splitting "the command" from "the
//! retained request" would put that list in two places that can disagree, and
//! the disagreement would surface as a child recovered under an authority its
//! own envelope hash never covered.
//!
//! # This module mints the shape; it does not execute it
//!
//! There is no product producer and no consumer yet. The handler-level driver
//! that runs a `ToolChildRequest` is FIG-2266's, and the group formation that
//! mints one is FIG-3397's. What this module owes them is a shape that is
//! frozen, complete and provable now: every field is retained before a group's
//! open is acknowledged, survives a round trip through both SQL stores, and
//! reconstructs a byte-identical child envelope out of the journal alone.
//!
//! # What rides here, and what already rides the envelope
//!
//! A `ToolChildRequest` is the payload of a
//! [`RuntimeEffectCommand`](super::envelope::RuntimeEffectCommand), so it sits
//! *inside* a [`RuntimeEffectEnvelope`](super::envelope::RuntimeEffectEnvelope)
//! whose [`invocation`](super::envelope::RuntimeEffectEnvelope::invocation)
//! already carries the child's **replay identity**: its `EffectAddress` is the
//! scope and replay key the claim is fenced on, and its `effect_id` is the
//! child's descriptive identity. That is not repeated here, because a second
//! copy of a journaled identity is a second thing to keep in sync and the first
//! thing to disagree at a crash boundary — the same reasoning that keeps a
//! position column off the child table (ADR 0065).
//!
//! **Lineage is carried once**, inside
//! [`attempt_identity`](ToolChildRequest::attempt_identity). Every arm of
//! [`ToolAttemptEffectIdentity`] holds the parent `RuntimeInvocation`, and that
//! parent *is* the dispatch context's `parent_invocation` at every construction
//! site in tree (`tool_dispatch/execution.rs` passes
//! `context.parent_invocation.clone()` into `Scalar` directly), so a separate
//! lineage field would be that same value spelled twice.
//!
//! # What is deployment wiring, not a recorded fact
//!
//! `ToolDispatchContext` holds 24 fields and most of them are **wiring**: the
//! plugin session, the tool provider, the registries, the session services, the
//! event sender, the clock. A durable request does not carry any of them, for
//! the same reason it does not carry the tool's code — they are what the
//! deployment supplies, and recording them would pin a recovered child to a
//! deployment that no longer exists.
//!
//! `attachment_source_policy` is named explicitly because it looks like policy
//! and is not data: it is an `Arc<dyn AttachmentSourcePolicy>`, a trait object
//! the host installs, exactly as much deployment wiring as the tool
//! implementation behind it.
//!
//! `intent_drain_slot` is **deliberately absent**. It is an `IntentDrainGuard`,
//! the in-process source-order gate, and ADR 0099 §5 replaces it with a durable
//! per-group final-commit order: "The in-process gate discharges from `Drop` …
//! which is right for a process-local future and **wrong** if copied into
//! durable recovery." Carrying it would durably record the device the ADR
//! removes. Its durable replacement is FIG-3409's.
//!
//! `checkpoint_messages` and `trigger_outcomes` are child-local buffers. What a
//! child accumulates in them rides its *settlement*, which is FIG-3411's (§6,
//! §13), not its request.
//!
//! # Turn context is never recorded, and on durable tiers cannot be set
//!
//! `TurnContext` is `#[derive(Clone)]` with no `Serialize`, and it holds a
//! live `LiveTurnInputs` (`HashMap<&'static str, Arc<dyn Any + Send + Sync>>`)
//! and an optional live `ProviderHandle`. ADR 0099 §3 already forbids carrying
//! it: "`RuntimeExecutionContext` is never serialized and there is no second
//! environment store … Semantic completion facts travel; live channels do not."
//!
//! The reason this costs nothing on the tiers a group child recovers on is that
//! the fence already exists upstream. `ensure_durable_effect_input`
//! (`crates/lash-core/src/runtime/turn_loop.rs`) refuses a turn carrying live
//! plugin inputs with [`RuntimeErrorCode::DurableEffectLivePluginInput`] before
//! it is ever admitted, and it runs on the durable admission paths
//! (`runtime/session_api.rs`, `runtime/turn_loop/prepare.rs`). Process runners
//! independently construct their tool dispatch with `TurnContext::default()`
//! (`session_manager/process_runners/{mod,runner}.rs`). So on every tier where
//! a group child is recovered from a journal, the only turn-context state a
//! tool can read — `ToolContext::plugin_input` is the sole tool-facing reader —
//! is already empty by construction. Recording "no turn context" would be
//! recording a fact that has no other representable value.

use serde::{Deserialize, Serialize};

use crate::tool_dispatch::ToolAttemptEffectIdentity;
use crate::{
    ExecutionScope, FrameNodeId, PreparedToolCall, ProcessExecutionEnvRef, ProcessId, SessionId,
    ToolExecutionGrant, ToolManifest, ToolRetryPolicy,
};

use super::executor::RuntimeEffectControllerError;

/// The durable format version of a retained tool-child request.
///
/// Guarded by `scripts/versioned-surfaces.toml`, so a field added to
/// [`ToolChildRequest`], [`ToolChildAdmission`] or [`ToolChildCompletionRouting`]
/// without a bump fails the repository gate rather than a production reopen.
///
/// Version 1 is the shape FIG-3408 froze. A reader refuses any other value
/// rather than defaulting: a request it cannot fully reconstruct is a child it
/// would run under partial authority, which is worse than refusing to run it.
pub const TOOL_CHILD_REQUEST_VERSION: u16 = 1;

/// The authority a tool child was admitted under, pinned at formation.
///
/// **One field with two arms rather than two optional fields**, because the two
/// are alternatives and "neither" and "both" are not states a child can be in.
///
/// ADR 0099 §3 requires that "a reopen uses the recorded facts, not current
/// session policy or fresh admission." A granted call already satisfies that:
/// `ToolExecutionGrant` exists precisely to "validate granted call arguments
/// without consulting the current Tool Catalog", and it carries its own
/// manifest and contract. An *ungranted* call is admitted by Tool Catalog
/// membership, and the catalog is live state that a reopen must not re-read — a
/// tool whose retry policy or argument projection changed between admission and
/// recovery would otherwise make a recovered child behave unlike the child that
/// was admitted.
///
/// The smallest fact that closes that gap is the admitted
/// [`ToolManifest`], because the manifest is what the catalog is consulted
/// *for*: `resolve_callable_manifest_by_id` and its siblings in
/// `tool_dispatch/preparation.rs` return a manifest and nothing else, and the
/// manifest carries `retry_policy` and `argument_projection` inline. Pinning it
/// is therefore the whole catalog dependency, not a snapshot of the catalog.
// `PartialEq` but not `Eq`, because a grant's `execution_binding` is a
// `serde_json::Value`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChildAdmission {
    /// Catalog membership at admission, pinned so a reopen never re-reads the
    /// live Tool Catalog.
    Catalog {
        /// The manifest the catalog answered with when the child was admitted.
        manifest: Box<ToolManifest>,
    },
    /// Explicit out-of-catalog authority, which already carries its own
    /// manifest and contract.
    Granted {
        /// The grant admitted at formation.
        grant: Box<ToolExecutionGrant>,
    },
}

impl ToolChildAdmission {
    /// The admitted manifest, whichever way the child was authorized.
    #[must_use]
    pub fn manifest(&self) -> &ToolManifest {
        match self {
            Self::Catalog { manifest } => manifest,
            Self::Granted { grant } => grant.manifest(),
        }
    }

    /// The retry policy the child was admitted under.
    ///
    /// Read through the manifest rather than stored beside it: `ToolManifest`
    /// already carries `retry_policy`, and a second copy on this type would be
    /// free to disagree with the manifest a granted call brings with it.
    #[must_use]
    pub fn retry_policy(&self) -> ToolRetryPolicy {
        self.manifest().retry_policy
    }

    /// The grant, for a call authorized outside catalog membership.
    #[must_use]
    pub fn grant(&self) -> Option<&ToolExecutionGrant> {
        match self {
            Self::Catalog { .. } => None,
            Self::Granted { grant } => Some(grant),
        }
    }
}

/// How a recovered child's completion is routed back to it (ADR 0099 §14).
///
/// Recorded because deriving it at recovery can derive a key nothing will ever
/// resolve. Completion-key preparation today answers
/// `Issued | NotNeeded | Unsupported` from two live inputs — whether the tool
/// may defer (`ToolDispatchContext::attempt_may_defer`, which consults the live
/// registry or provider) and whether the host routes completions durably. Both
/// are deployment facts at recovery time and admission facts at formation time,
/// and only the admission facts are the ones the child was accepted under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChildCompletionRouting {
    /// The child settles inside its own attempt and needs no completion key.
    Inline,
    /// The child may defer, and its completion key is routed durably — a
    /// completion still resolves after the worker that issued it is gone.
    Durable,
    /// The child may defer, and its completion key lives only as long as the OS
    /// process that issued it (`NativeEffectHost::allow_process_lifetime_completion_keys`,
    /// ADR 0099 §14: "Native durability ends at the runtime's lifetime").
    ///
    /// Recovering such a child in a different process is a typed refusal, never
    /// a fresh key: the original key is unresolvable and a new one would be a
    /// second dispatch of an opaque tool body.
    ProcessLifetime,
}

/// Everything needed to run one tool child of a durable effect group, with no
/// caller in scope (ADR 0099 §3).
///
/// Retained before the group's open is acknowledged and read back by a reopen,
/// so the authority a recovered child runs under is the authority it was
/// admitted under — never the current session's, and never a fresh admission.
///
/// # Who writes each field, and who reads it
///
/// This is the seam between three lanes, so it is stated field by field rather
/// than left to be inferred:
///
/// | field | written by | read by |
/// |---|---|---|
/// | [`call`](Self::call) | group formation (FIG-3397), from the prepared batch call | the handler-level driver (FIG-2266), as the call to execute |
/// | [`admission`](Self::admission) | group formation, from the grant or the admitted catalog manifest | the driver, for authority, retry policy and argument projection, without the live catalog |
/// | [`attempt_identity`](Self::attempt_identity) | group formation, as the identity the leaf's attempts derive from | the driver, to derive each attempt's replay key and causal parent |
/// | [`opener`](Self::opener) | group formation, as the logical opener of ADR 0099 §1 | recovery (FIG-3396 §1), to validate the opener still exists and matches |
/// | [`admitted_scope`](Self::admitted_scope) | group formation, as the scope the child's claim is fenced on | the driver, to reconstruct the admitted controller |
/// | [`session_id`](Self::session_id) | group formation | the driver, for session-scoped services |
/// | [`agent_frame_id`](Self::agent_frame_id) | group formation | the driver, for the frame the child's work belongs to |
/// | [`enclosing_process`](Self::enclosing_process) | group formation, when the opener is a process | the driver, to set the call's enclosing process |
/// | [`cancellation_authority`](Self::cancellation_authority) | group formation, from the opener's turn-control binding | the cooperative cancel path (FIG-2266) and the cancel disposition (FIG-3409) |
/// | [`execution_env`](Self::execution_env) | group formation, from `captured_process_execution_env_ref` | the driver, to resolve the captured environment; retained under `ArtifactOwner::Execution` until the last dependency |
/// | [`completion_routing`](Self::completion_routing) | group formation, from the admitted deferral and routing facts | the driver and recovery, to refuse a key nothing can resolve |
///
/// # What is deliberately absent
///
/// See the module documentation for the three groups: deployment wiring (the
/// registries, services, sender, clock and `attachment_source_policy`), live
/// state (`turn_context`, and `RuntimeExecutionContext` generally), and facts
/// that belong to a sibling ticket (`intent_drain_slot` to FIG-3409, the
/// child-local checkpoint and trigger buffers to FIG-3411).
///
/// **No child invocation id.** Retaining the dispatched invocation's id across
/// handover is ADR 0099 §8 and belongs to FIG-3411. W2 — a crash after dispatch
/// and before that id was published — is answered here instead by
/// *reconstruction*: this request plus its envelope re-derives the same replay
/// key and the same canonical hash, so recovery reissues that dispatch's own
/// identity rather than a fresh unrelated call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolChildRequest {
    /// The durable format version, refused rather than defaulted.
    pub version: u16,
    /// The prepared call this child executes.
    pub call: PreparedToolCall,
    /// The authority the child was admitted under.
    pub admission: ToolChildAdmission,
    /// The identity this child's attempts derive their replay keys and causal
    /// parent from. Carries the parent invocation, so it is also the request's
    /// lineage.
    pub attempt_identity: ToolAttemptEffectIdentity,
    /// The exact logical opener the group binds (ADR 0099 §1).
    ///
    /// Recorded as an [`ExecutionScope`] deliberately. §1 requires a process
    /// opener to carry its incarnation, and `ExecutionScope::Process` does not
    /// carry one yet; binding the incarnation into that scope is FIG-3394's, and
    /// when it lands this field carries it with no change to this shape. A
    /// parallel opener type minted here would have been a second spelling of the
    /// same fact, and the two would disagree the first time only one was updated.
    pub opener: ExecutionScope,
    /// The scope this child is admitted and claimed under, which a process
    /// opener's child need not share with its opener.
    pub admitted_scope: ExecutionScope,
    /// The session the child's work is attributed to.
    pub session_id: SessionId,
    /// The agent frame the child's work belongs to.
    ///
    /// Not reconstructible from [`session_id`](Self::session_id): one session
    /// holds many frames (ADR 0092), so a recovered child that re-derived a
    /// frame from its session would attribute its work to the wrong one.
    pub agent_frame_id: FrameNodeId,
    /// The process this call executes inside, when the opener is a process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enclosing_process: Option<ProcessId>,
    /// The turn-control binding that authorizes cancelling this child, as
    /// `turn_control_binding_id_for_scope` derives it. `None` where the opener
    /// participates in no turn-control authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_authority: Option<String>,
    /// The captured process-execution environment this child resolves, retained
    /// under `ArtifactOwner::Execution` through its last dependency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_env: Option<ProcessExecutionEnvRef>,
    /// How this child's completion is routed back to it.
    pub completion_routing: ToolChildCompletionRouting,
}

impl ToolChildRequest {
    /// Assembles a request at the current format version.
    ///
    /// The facts with no sensible absence are taken here; the three that may
    /// legitimately be absent are set through the builders below, so a caller
    /// cannot omit an opener or an admission by forgetting a field.
    #[must_use]
    pub fn new(
        call: PreparedToolCall,
        admission: ToolChildAdmission,
        attempt_identity: ToolAttemptEffectIdentity,
        opener: ExecutionScope,
        admitted_scope: ExecutionScope,
        session_id: SessionId,
        agent_frame_id: FrameNodeId,
        completion_routing: ToolChildCompletionRouting,
    ) -> Self {
        Self {
            version: TOOL_CHILD_REQUEST_VERSION,
            call,
            admission,
            attempt_identity,
            opener,
            admitted_scope,
            session_id,
            agent_frame_id,
            enclosing_process: None,
            cancellation_authority: None,
            execution_env: None,
            completion_routing,
        }
    }

    /// Binds the process this call executes inside.
    #[must_use]
    pub fn with_enclosing_process(mut self, process_id: ProcessId) -> Self {
        self.enclosing_process = Some(process_id);
        self
    }

    /// Binds the turn-control authority that may cancel this child.
    #[must_use]
    pub fn with_cancellation_authority(mut self, binding_id: impl Into<String>) -> Self {
        self.cancellation_authority = Some(binding_id.into());
        self
    }

    /// Binds the captured process-execution environment this child resolves.
    #[must_use]
    pub fn with_execution_env(mut self, env: ProcessExecutionEnvRef) -> Self {
        self.execution_env = Some(env);
        self
    }

    /// The retry policy the child was admitted under.
    #[must_use]
    pub fn retry_policy(&self) -> ToolRetryPolicy {
        self.admission.retry_policy()
    }

    /// Refuses a request this build cannot reconstruct completely.
    ///
    /// Called by envelope validation, so a malformed request is refused at
    /// construction and at decode rather than at the moment a recovered child
    /// runs under partial authority.
    pub fn validate(&self) -> Result<(), RuntimeEffectControllerError> {
        if self.version != TOOL_CHILD_REQUEST_VERSION {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectToolChildRequestVersion,
                format!(
                    "retained tool-child request records format version {}, and this build \
                     reconstructs version {TOOL_CHILD_REQUEST_VERSION}; a request that cannot \
                     be read completely is refused rather than run under partial authority",
                    self.version
                ),
            ));
        }
        if self.call.call_id.trim().is_empty() {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectToolChildRequestCallId,
                "retained tool-child request requires a non-empty call id",
            ));
        }
        if self.admission.manifest().id != self.call.tool_id {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectToolChildRequestAdmission,
                format!(
                    "retained tool-child request admits tool `{}` but calls tool `{}`; the \
                     admitted authority and the call it authorizes are one fact",
                    self.admission.manifest().id,
                    self.call.tool_id
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolDefinition, ToolId};

    fn definition(id: &str) -> ToolDefinition {
        ToolDefinition::raw(
            id,
            id,
            "search the web",
            serde_json::json!({ "type": "object" }),
            serde_json::json!({ "type": "object" }),
        )
    }

    fn manifest(id: &str) -> ToolManifest {
        definition(id).manifest()
    }

    fn frame() -> FrameNodeId {
        FrameNodeId::new("frame-1").expect("a valid frame id")
    }

    fn call(tool_id: &str) -> PreparedToolCall {
        PreparedToolCall::from_parts(
            "call-1",
            ToolId::from(tool_id),
            "search",
            serde_json::json!({ "q": "lash" }),
            None,
            serde_json::Value::Null,
        )
    }

    fn request() -> ToolChildRequest {
        let tool = manifest("search");
        ToolChildRequest::new(
            call(tool.id.as_str()),
            ToolChildAdmission::Catalog {
                manifest: Box::new(tool),
            },
            ToolAttemptEffectIdentity::Scalar { parent: None },
            ExecutionScope::turn("session", "turn"),
            ExecutionScope::turn("session", "turn"),
            SessionId::from("session"),
            frame(),
            ToolChildCompletionRouting::Durable,
        )
    }

    /// Every field survives the durable round trip. A request that lost a field
    /// in serialization is a child recovered under partial authority, which is
    /// the exact failure §3 retains input to prevent.
    #[test]
    fn a_request_round_trips_every_field_through_its_durable_bytes() {
        let original = request()
            .with_enclosing_process(ProcessId::from("process-9"))
            .with_cancellation_authority("binding-7")
            .with_execution_env(ProcessExecutionEnvRef::new("env-ref"));
        let json = serde_json::to_string(&original).expect("a request serializes");
        let decoded: ToolChildRequest = serde_json::from_str(&json).expect("a request decodes");
        assert_eq!(decoded, original);
        assert_eq!(decoded.version, TOOL_CHILD_REQUEST_VERSION);
        assert_eq!(decoded.cancellation_authority.as_deref(), Some("binding-7"));
        assert_eq!(
            decoded
                .execution_env
                .as_ref()
                .map(ProcessExecutionEnvRef::as_str),
            Some("env-ref")
        );
        assert_eq!(decoded.enclosing_process.as_deref(), Some("process-9"));
        assert_eq!(decoded.agent_frame_id.as_str(), "frame-1");
    }

    /// A field this build does not know is refused, not dropped. A retired field
    /// that vanished into a default would silently narrow the authority a
    /// recovered child runs under (prelude, FIG-2886 review).
    #[test]
    fn an_unknown_field_is_refused_rather_than_ignored() {
        let mut value = serde_json::to_value(request()).expect("a request serializes");
        value
            .as_object_mut()
            .expect("a request is a JSON object")
            .insert("fabricated".to_string(), serde_json::Value::Bool(true));
        let error = serde_json::from_value::<ToolChildRequest>(value)
            .expect_err("an unknown field must be refused");
        assert!(
            error.to_string().contains("fabricated"),
            "the refusal must name the field it refused, got {error}"
        );
    }

    /// A version no build wrote is refused rather than read under this build's
    /// field meanings.
    #[test]
    fn a_foreign_format_version_is_refused_rather_than_defaulted() {
        let mut foreign = request();
        foreign.version = TOOL_CHILD_REQUEST_VERSION + 1;
        let error = foreign
            .validate()
            .expect_err("a foreign version is refused");
        assert_eq!(
            error.code,
            crate::RuntimeErrorCode::RuntimeEffectToolChildRequestVersion
        );
    }

    #[test]
    fn an_empty_call_id_is_refused() {
        let mut blank = request();
        blank.call.call_id = "   ".to_string();
        assert_eq!(
            blank
                .validate()
                .expect_err("a blank call id is refused")
                .code,
            crate::RuntimeErrorCode::RuntimeEffectToolChildRequestCallId
        );
    }

    /// The admitted authority and the call it authorizes are one fact. A request
    /// pinning tool A's manifest against a call to tool B would let a recovered
    /// child run tool B under tool A's retry policy and argument projection.
    #[test]
    fn an_admission_for_another_tool_is_refused() {
        let mut crossed = request();
        crossed.call.tool_id = ToolId::from("other-tool");
        assert_eq!(
            crossed
                .validate()
                .expect_err("a crossed admission is refused")
                .code,
            crate::RuntimeErrorCode::RuntimeEffectToolChildRequestAdmission
        );
    }

    /// The pinned manifest is the whole catalog dependency: the retry policy a
    /// reopen uses comes from the record, so a catalog edited after admission
    /// cannot change how a recovered child retries (ADR 0099 §3).
    #[test]
    fn the_retry_policy_is_read_from_the_pinned_admission_not_a_live_catalog() {
        let mut tool = manifest("search");
        tool.retry_policy = ToolRetryPolicy::safe(4, 10, 100);
        let pinned = ToolChildRequest::new(
            call(tool.id.as_str()),
            ToolChildAdmission::Catalog {
                manifest: Box::new(tool),
            },
            ToolAttemptEffectIdentity::Scalar { parent: None },
            ExecutionScope::turn("session", "turn"),
            ExecutionScope::turn("session", "turn"),
            SessionId::from("session"),
            frame(),
            ToolChildCompletionRouting::Inline,
        );
        let decoded: ToolChildRequest =
            serde_json::from_str(&serde_json::to_string(&pinned).expect("serializes"))
                .expect("decodes");
        assert_eq!(decoded.retry_policy(), ToolRetryPolicy::safe(4, 10, 100));
    }

    /// A granted call carries its own manifest and contract, so it needs no
    /// pinned catalog entry beside it — the two arms are alternatives, and the
    /// grant arm answers `manifest()` from the grant.
    #[test]
    fn a_granted_admission_answers_from_its_own_grant() {
        let admission = ToolChildAdmission::Granted {
            grant: Box::new(ToolExecutionGrant::from_definition(definition("search"))),
        };
        assert_eq!(admission.manifest().id, ToolId::from("search"));
        assert!(admission.grant().is_some());
        assert!(
            ToolChildAdmission::Catalog {
                manifest: Box::new(manifest("search"))
            }
            .grant()
            .is_none()
        );
    }

    /// The opener and the admitted scope are two facts, not one. A process
    /// opener's child is claimed under its own scope, and collapsing them would
    /// make a recovered child validate its opener against the wrong identity.
    #[test]
    fn the_opener_and_the_admitted_scope_are_retained_separately() {
        let mut request = request();
        request.opener = ExecutionScope::process("process-1");
        request.admitted_scope = ExecutionScope::runtime_operation("op-1");
        let decoded: ToolChildRequest =
            serde_json::from_str(&serde_json::to_string(&request).expect("serializes"))
                .expect("decodes");
        assert_eq!(decoded.opener, ExecutionScope::process("process-1"));
        assert_eq!(
            decoded.admitted_scope,
            ExecutionScope::runtime_operation("op-1")
        );
    }

    /// Completion routing is recorded, not re-derived. A process-lifetime key is
    /// a different fact from a durable one, and a reopen that guessed would
    /// derive a key nothing resolves (ADR 0099 §14).
    #[test]
    fn completion_routing_round_trips_every_mode() {
        for mode in [
            ToolChildCompletionRouting::Inline,
            ToolChildCompletionRouting::Durable,
            ToolChildCompletionRouting::ProcessLifetime,
        ] {
            let mut request = request();
            request.completion_routing = mode;
            let decoded: ToolChildRequest =
                serde_json::from_str(&serde_json::to_string(&request).expect("serializes"))
                    .expect("decodes");
            assert_eq!(decoded.completion_routing, mode);
        }
    }

    /// Lineage is the attempt identity's parent, carried once. Losing it would
    /// reparent every attempt a recovered child makes.
    #[test]
    fn lineage_rides_the_attempt_identity_and_survives_the_round_trip() {
        let parent = crate::RuntimeInvocation::effect(
            crate::EffectAddress::new(ExecutionScope::turn("session", "turn"), "parent-effect")
                .expect("a valid address"),
            crate::RuntimeAttribution::for_session("session"),
            "parent-effect",
        );
        let mut request = request();
        request.attempt_identity = ToolAttemptEffectIdentity::Batch {
            parent: parent.clone(),
            replay_suffix: "leaf-2".to_string(),
        };
        let decoded: ToolChildRequest =
            serde_json::from_str(&serde_json::to_string(&request).expect("serializes"))
                .expect("decodes");
        match decoded.attempt_identity {
            ToolAttemptEffectIdentity::Batch {
                parent: decoded_parent,
                replay_suffix,
            } => {
                assert_eq!(decoded_parent, parent);
                assert_eq!(replay_suffix, "leaf-2");
            }
            other => panic!("the batch identity must survive, got {other:?}"),
        }
    }
}
