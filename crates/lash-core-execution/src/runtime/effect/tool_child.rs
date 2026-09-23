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
//! level, and only the atomic attempt runs inside a recorded body.
//! [`ToolAttempt`](super::envelope::RuntimeEffectCommand::ToolAttempt) does not
//! name that: it is the atomic body itself — one attempt of one call, the thing
//! that goes inside `ctx.run` — so a driver expressed as a `ToolAttempt` could
//! not retry, because a second attempt is a second envelope with a second hash.
//! A group whose child cannot be named cannot be retained (§3) or recovered
//! (W1, W2).
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
//! # This module mints the shape; the group path executes it
//!
//! The handler-level driver that runs a `ToolChildRequest` is FIG-2266's
//! [`super::tool_child_driver`], and the group formation that mints one is
//! FIG-3397's `session::tool_execution::group` — the producer is
//! `RuntimeExecutionContext::call_tool_batch` and the standard-protocol turn
//! driver, the consumer the same module's settlement loop. What this module
//! owes them is a shape that is frozen, complete and provable: every field is
//! retained before a group's open is acknowledged, survives a round trip
//! through both SQL stores, and reconstructs a byte-identical child envelope
//! out of the journal alone.
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
//! No drain-order slot rides here. ADR 0099 §5 orders sibling drains by a
//! durable per-group final-commit order recorded at the §4 commit, not by any
//! fact the request could carry.
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
    AdmittedScope, EffectOpener, FrameNodeId, PreparedToolCall, ProcessExecutionEnvRef, ProcessRef,
    SessionId, ToolExecutionGrant, ToolManifest, ToolRetryPolicy, TurnControlBindingId,
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
///
/// Version 2 adds the queue-drain arm to [`EffectOpener`] (FIG-3394, ADR 0099
/// §1), which widens `scope.opener`. Nothing persists this shape in production
/// yet, so the move costs nothing at run time — but the constant moves anyway,
/// because a version that did not would let a v1 reader decode a v2 request,
/// find an opener arm it has no branch for, and fail somewhere other than the
/// boundary. The v1 refusal is kept as its own test.
///
/// Version 3 adds the issuing-authority field to
/// [`ToolChildCompletionRouting::ProcessLifetime`]: a process-lifetime key can
/// only be resolved by the registry identity that minted it, so the request
/// records *who* issued it, not just *that* it was process-lifetime. Without
/// the issuer, a reopen on a second host — or on the same host after its
/// registry was rebuilt — would prepare a key under an authority that cannot
/// authenticate it.
///
/// Version 4 retires the bare claim scope: `scope.admitted_scope` is now an
/// [`AdmittedScope`], the checked scope/incarnation pair controller
/// construction takes (FIG-3430, ADR 0099 §1). A v3 journal could pair a
/// process claim with no pin — or with a pin the scope never agreed to — and
/// only the driver's post-admission pin block caught it; the checked pair
/// makes that shape unrepresentable, on the wire as everywhere else, because
/// decoding runs `AdmittedScope::new` rather than trusting the bytes.
pub const TOOL_CHILD_REQUEST_VERSION: u16 = 4;

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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChildCompletionRouting {
    /// The child settles inside its own attempt and needs no completion key.
    Inline,
    /// The child may defer, and its completion key is routed durably — a
    /// completion still resolves after the worker that issued it is gone.
    ///
    /// Valid only when the claim's admitted scope journals its effects
    /// durably; the driver refuses a durable routing request whose scoped
    /// controller reports [`EffectJournaling::Local`].
    ///
    /// [`EffectJournaling::Local`]:
    ///     crate::runtime::effect::EffectJournaling::Local
    Durable,
    /// The child may defer, and its completion key lives only as long as the OS
    /// process that issued it (`NativeEffectHost::allow_process_lifetime_completion_keys`,
    /// ADR 0099 §14: "Native durability ends at the runtime's lifetime").
    ///
    /// `issuer` is the awaiting authority's durable identity — the host's
    /// `EffectHost::turn_control_binding_id` at formation. Recovering such a
    /// child under a different registry identity is a typed refusal, never a
    /// fresh key: the original key is unresolvable and a new one would be a
    /// second dispatch of an opaque tool body.
    ProcessLifetime {
        /// The await-event authority that minted the key.
        issuer: TurnControlBindingId,
    },
}

/// Where a tool child runs and whose work it is.
///
/// Four facts that always travel together and are never independently
/// meaningful: a child admitted under one opener, one claim scope, one session
/// and one frame. Grouping them keeps the request's constructor honest about
/// what a caller must supply, and makes "the child's binding" a thing a reader
/// can name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolChildScope {
    /// The exact logical opener the group binds (ADR 0099 §1): a turn, or one
    /// process **incarnation**.
    ///
    /// [`EffectOpener`], not an [`ExecutionScope`]. §1 requires a process
    /// opener to carry its incarnation and `ExecutionScope::Process` carries
    /// only `process_id`, so a scope retained here would leave recovery-time
    /// validation with nothing to validate and would let a process
    /// re-registered under the same name alias its predecessor's groups and
    /// fences. The scope is the child's *claim address*, which is
    /// [`admitted_scope`](Self::admitted_scope); the opener is who owns it.
    pub opener: EffectOpener,
    /// The scope this child is admitted and claimed under, which a process
    /// opener's child need not share with its opener.
    ///
    /// The checked [`AdmittedScope`] pair, not a bare [`ExecutionScope`]: a
    /// process claim carries its store-minted incarnation inside the same
    /// value, so the half-admitted shape — a process scope with no pin, or a
    /// pin naming another process — cannot be journaled. Decoding re-runs
    /// `AdmittedScope::new` through the wire helper rather than trusting the
    /// bytes; this is the pin the child's controller is constructed from,
    /// never `enclosing_process`.
    #[serde(with = "admitted_scope_wire")]
    pub admitted_scope: AdmittedScope,
    /// The session the child's work is attributed to.
    ///
    /// Carried beside the opener rather than derived from it because a
    /// **process** opener has no session of its own — ADR 0094 governs its
    /// lifetime through its own Parent Scope — while its tool work is still
    /// attributed to a session. For a turn opener the two are one fact, and
    /// [`validate`](Self::validate) refuses a request where they disagree.
    pub session_id: SessionId,
    /// The agent frame the child's work belongs to.
    ///
    /// Not reconstructible from [`session_id`](Self::session_id): one session
    /// holds many frames (ADR 0092), so a recovered child that re-derived a
    /// frame from its session would attribute its work to the wrong one.
    pub agent_frame_id: FrameNodeId,
}

/// The wire shape of an admitted scope: the bare pair, re-checked at decode.
///
/// [`AdmittedScope`] does not implement `Deserialize` on purpose — its only
/// construction is [`AdmittedScope::new`], which refuses a process scope with
/// no incarnation, a pin naming another process, or a pin on a non-process
/// scope. The journal carries the two halves plainly so the durable shape
/// stays legible, and decoding runs the check again rather than trusting the
/// bytes: a hand-edited or cross-version journal entry cannot smuggle in the
/// half-admitted pair the type exists to make unrepresentable.
mod admitted_scope_wire {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::*;

    /// The serialized pair: the claim address and, for a process claim, the
    /// incarnation the admission authority bound.
    #[derive(Serialize, Deserialize)]
    pub struct AdmittedScopeWire {
        pub scope: crate::ExecutionScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub process: Option<ProcessRef>,
    }

    pub fn serialize<S: Serializer>(
        admitted: &AdmittedScope,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        AdmittedScopeWire {
            scope: admitted.scope().clone(),
            process: admitted.process_ref().cloned(),
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<AdmittedScope, D::Error> {
        let wire = AdmittedScopeWire::deserialize(deserializer)?;
        AdmittedScope::new(wire.scope, wire.process).map_err(serde::de::Error::custom)
    }
}

impl ToolChildScope {
    /// Refuses a binding whose opener and session disagree.
    ///
    /// A turn opener already names its session, so the pair is representable
    /// and invalid in exactly one way. Refused at the boundary, as
    /// `EffectGroupShape::validate_wire` refuses its own two-halves mismatch,
    /// rather than left for a recovered child to attribute to the wrong session.
    pub fn validate(&self) -> Result<(), RuntimeEffectControllerError> {
        if let Some(opener_session) = self.opener.session_id()
            && *opener_session != self.session_id
        {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectToolChildRequestOpener,
                format!(
                    "retained tool-child request binds opener session `{opener_session}` but \
                     attributes its work to session `{}`; a turn opener and a queue-drain \
                     opener each name their own session, so the two are one fact",
                    self.session_id
                ),
            ));
        }
        Ok(())
    }
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
/// | [`scope`](Self::scope) | group formation, as the opener, claim scope, session and frame | recovery (FIG-3396 §1) validates the opener; the driver reconstructs the admitted controller and its session-scoped services |
/// | [`enclosing_process`](Self::enclosing_process) | group formation, when the opener is a process, as a `ProcessRef` | the driver, to set the call's enclosing process incarnation |
/// | [`cancellation_authority`](Self::cancellation_authority) | group formation, from the opener's turn-control binding | the cooperative cancel path (FIG-2266) and the cancel disposition (FIG-3409) |
/// | [`execution_env`](Self::execution_env) | group formation, from `captured_process_execution_env_ref` (required) | the driver, to resolve the captured environment; retained under `ArtifactOwner::Execution` until the last dependency |
/// | [`completion_routing`](Self::completion_routing) | group formation, from the admitted deferral and routing facts | the driver and recovery, to refuse a key nothing can resolve |
///
/// # What is deliberately absent
///
/// See the module documentation for the three groups: deployment wiring (the
/// registries, services, sender, clock and `attachment_source_policy`), live
/// state (`turn_context`, and `RuntimeExecutionContext` generally), and facts
/// that belong to a sibling ticket (the child-local checkpoint and trigger
/// buffers to FIG-3411).
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
    /// Where this child runs and whose work it is.
    pub scope: ToolChildScope,
    /// The process **incarnation** this call executes inside, when the opener
    /// is a process.
    ///
    /// A [`ProcessRef`], not a `ProcessId`, for §1's reason: the enclosing
    /// process a recovered child reports must be the incarnation it was
    /// admitted under, never whatever process currently carries that name.
    /// `None` for a non-process opener, which encloses no process — and
    /// required to *be* the opener's own incarnation for a process opener
    /// ([`validate`](Self::validate) refuses any other pair).
    ///
    /// This is tool execution context only. The child's **claim** pin — the
    /// incarnation its controller is constructed under — is inside
    /// [`scope.admitted_scope`](ToolChildScope::admitted_scope), the checked
    /// pair; nothing here re-pins a claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enclosing_process: Option<ProcessRef>,
    /// The turn-cancellation authority that may cancel this child.
    ///
    /// This is the identity `turn_control_binding_id_for_scope` mints and
    /// `binding_id_admits_scope` checks — the address the cooperative cancel
    /// path (FIG-2266) signals and the one FIG-3409's cancel disposition is
    /// fenced on. Typed rather than a bare string because a frozen durable
    /// shape may not carry an unvalidated identity.
    ///
    /// **`None` is legal in exactly one case**: the opener's controller
    /// journals *locally* (`EffectJournaling::Local`) rather than through a
    /// durable journaled authority, so there is no durable address to record
    /// and a recovered child has no cancellation to honour. Every `Journaled`
    /// opener records `Some`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_authority: Option<TurnControlBindingId>,
    /// The captured process-execution environment this child resolves, retained
    /// under `ArtifactOwner::Execution` through its last dependency.
    ///
    /// **Required, not optional.** ADR 0099 §3 makes the environment reference
    /// part of the retained authority, and the capture it comes from is total:
    /// `RuntimeExecutionContext::captured_process_execution_env_ref` returns
    /// `Result<ProcessExecutionEnvRef, _>`, inheriting the caller's reference
    /// when there is one and publishing a fresh one otherwise. There is no path
    /// that legitimately yields "no environment", so an `Option` here would be a
    /// representable state with no producer — and a recovered child that found
    /// `None` would have to invent an environment, which is the silent default
    /// §3 exists to prevent.
    pub execution_env: ProcessExecutionEnvRef,
    /// How this child's completion is routed back to it.
    pub completion_routing: ToolChildCompletionRouting,
}

impl ToolChildRequest {
    /// The completion key this child's deferred attempt parks on, as the
    /// scope and wait identity it is minted from — `None` for an `Inline`
    /// child, which never takes one.
    ///
    /// The child's controller is constructed under its admitted scope, and a
    /// tool context mints its key there for its own call id, so this is the
    /// promise a completion for this child is delivered to. The substrate that
    /// owns the child's cancel fence closes it at the cancel decision (ADR
    /// 0099 §4, W17): completion delivery is one of the sinks the fence
    /// covers.
    #[must_use]
    pub fn completion_wait(
        &self,
    ) -> Option<(crate::ExecutionScope, crate::AwaitEventWaitIdentity)> {
        match self.completion_routing {
            ToolChildCompletionRouting::Inline => None,
            ToolChildCompletionRouting::Durable
            | ToolChildCompletionRouting::ProcessLifetime { .. } => Some((
                self.scope.admitted_scope.scope().clone(),
                crate::AwaitEventWaitIdentity::tool_completion(self.call.call_id.clone()),
            )),
        }
    }

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
        scope: ToolChildScope,
        execution_env: ProcessExecutionEnvRef,
        completion_routing: ToolChildCompletionRouting,
    ) -> Self {
        Self {
            version: TOOL_CHILD_REQUEST_VERSION,
            call,
            admission,
            attempt_identity,
            scope,
            enclosing_process: None,
            cancellation_authority: None,
            execution_env,
            completion_routing,
        }
    }

    #[must_use]
    pub fn with_enclosing_process(mut self, process_ref: ProcessRef) -> Self {
        self.enclosing_process = Some(process_ref);
        self
    }

    /// Binds the durable turn-cancellation authority that may cancel this child.
    ///
    /// Left unset only for a locally participating opener; see the field.
    #[must_use]
    pub fn with_cancellation_authority(mut self, binding_id: TurnControlBindingId) -> Self {
        self.cancellation_authority = Some(binding_id);
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
        self.scope.validate()?;
        match (&self.scope.opener, self.enclosing_process.as_ref()) {
            (EffectOpener::Process { process_ref }, Some(enclosing))
                if process_ref == enclosing => {}
            (EffectOpener::Process { process_ref }, enclosing) => {
                return Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectToolChildRequestOpener,
                    format!(
                        "retained tool-child request opens under process incarnation \
                         `{process_ref}` but records {enclosing} as its enclosing process; \
                         a process opener's child executes inside the opener's own \
                         incarnation, so the two are one fact",
                        enclosing = enclosing
                            .map(|process_ref| format!("`{process_ref}`"))
                            .unwrap_or_else(|| "no incarnation".to_string()),
                    ),
                ));
            }
            (_, Some(enclosing)) => {
                return Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectToolChildRequestOpener,
                    format!(
                        "retained tool-child request opens under `{}` but records enclosing \
                         process `{enclosing}`; only a process opener encloses a process",
                        self.scope.opener.render(),
                    ),
                ));
            }
            (_, None) => {}
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
