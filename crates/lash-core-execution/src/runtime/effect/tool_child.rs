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
//! replayable *invocation driver*: retry, completion-key derivation and
//! deferred await are coordination that runs at handler level, and only the
//! atomic attempt runs inside a recorded body.
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
//! **Lineage is carried once**, in [`lineage`](ToolChildRequest::lineage):
//! the parent `RuntimeInvocation` the child's attempts descend from. The
//! child's identity is its call's [`ToolCallId`](crate::ToolCallId), which
//! every attempt and retry key derives from (ADR 0117 §6).
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
//! # Turn context is never recorded
//!
//! `TurnContext` is `#[derive(Clone)]` with no `Serialize`: it holds a turn's
//! prompt layer and its live runtime correlation (a child session turn's
//! process correlation and lineage). ADR 0099 §3 already forbids carrying it:
//! "`RuntimeExecutionContext` is never serialized and there is no second
//! environment store … Semantic completion facts travel; live channels do
//! not." A tool reads nothing from it: a send refuses a live per-turn prompt,
//! and the runtime correlation is the runtime's own. Recording "no turn
//! context" would be recording a fact that has no other representable value.

use serde::{Deserialize, Serialize};

use crate::tool_dispatch::ToolAttemptLineage;
use crate::{
    AdmittedScope, EffectOpener, PreparedToolCall, ProcessExecutionEnvRef, ProcessId,
    ToolExecutionGrant, ToolManifest, ToolRetryPolicy, TurnControlBindingId,
};

use super::executor::RuntimeEffectControllerError;

/// The durable format version of a retained tool-child request.
///
/// Version 1 is the shape FIG-3408 froze. A reader refuses any other value
/// rather than defaulting: a request it cannot fully reconstruct is a child it
/// would run under partial authority, which is worse than refusing to run it.
///
/// Version 2 adds the session-operation arm to [`EffectOpener`] (FIG-3394, ADR 0099
/// §1), which widens `scope.opener`. Nothing persists this shape in production
/// yet, so the move costs nothing at run time — but the constant moves anyway,
/// because a version that did not would let a v1 reader decode a v2 request,
/// find an opener arm it has no branch for, and fail somewhere other than the
/// boundary. The v1 refusal is kept as its own test.
///
/// Version 3 adds the issuing-authority field to the process-lifetime
/// completion route: a process-lifetime key can only be resolved by the
/// registry identity that minted it, so the request records *who* issued it,
/// not just *that* it was process-lifetime. Version 8 retires that route.
///
/// Version 4 retires the bare claim scope: `scope.admitted_scope` is now an
/// [`AdmittedScope`], the checked scope/incarnation pair controller
/// construction takes (FIG-3430, ADR 0099 §1). A v3 journal could pair a
/// process claim with no pin — or with a pin the scope never agreed to — and
/// only the driver's post-admission pin block caught it; the checked pair
/// makes that shape unrepresentable, on the wire as everywhere else, because
/// decoding runs `AdmittedScope::new` rather than trusting the bytes.
///
/// Version 6 moves a parked child's §4 commit from its finalize to its
/// completion resolution (FIG-3609, ADR 0099 §5): the child now commits
/// before its presentation boundary, which adds journaled steps between the
/// await and the presentation on every tier. The request's fields are
/// unchanged. The version moves because a child journaled by an older build
/// would replay against a different step order, so it is refused, typed and
/// before any effect, at [`ToolChildRequest::validate`].
///
/// Version 7 (FIG-3586) adds the `command` attempt identity: a lashlang
/// command's tool attempts key under the command's issue-ordinal key
/// (`{command}:attempt:{a}`), never the call id, and a v6 reader has no such
/// identity to rebuild a child's attempts under.
///
/// Version 8 (FIG-3585) retires the process-lifetime completion route and
/// makes the cancellation authority required: every host journals its effects
/// (ADR 0102, D1), so every child records the durable binding its opener's
/// cooperative signal is fenced on, and no key lives only as long as the
/// process that issued it. A v7 request is refused, typed and before any
/// effect, at [`ToolChildRequest::validate`].
///
/// Version 9 (FIG-3712) records the child's session facts at group open — the
/// tool surface, tool access and subagent context it runs under, and which
/// unrecordable sources its opener had — so a child runs under the same
/// authority whether its opener lends its context or the deployment builds
/// one. A v8 request is refused.
///
/// Version 10 (FIG-3607) names an enclosing or opening process by its minted
/// id alone: the incarnation-qualified process reference is retired, so a v9
/// request is refused, typed and before any effect, at
/// [`ToolChildRequest::validate`].
///
/// The claim scope and the enclosing process are no longer recorded beside the
/// opener (FIG-4665): both are the opener's own, derived by
/// [`ToolChildScope::claim_scope`] and [`ToolChildRequest::enclosing_process`].
/// The shape changed in place under the pre-1.0 version freeze.
///
/// version_guard(
///     roots(ToolChildRequest),
///     roots(path = "crates/lash-core-store/src/session_identity.rs", SessionToolAccessWire),
///     file(
///         path = "crates/lash-sansio/src/identity.rs",
///         cover("string_identity!", SessionId, ProcessId, TurnId),
///     ),
/// )
pub const TOOL_CHILD_REQUEST_VERSION: u16 = 10;

mod session_facts;
pub use session_facts::{
    ToolChildOpenerContext, ToolChildRebuildRefusal, ToolChildSessionFacts,
    UnrecordedSessionSources,
};

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
    /// Valid only when the child's controller names its durable await-event
    /// authority; the driver refuses a durable routing request whose scoped
    /// controller names none.
    Durable,
}

/// Where a tool child runs and whose work it is.
///
/// Two facts that always travel together and are never independently
/// meaningful: the opener the child was admitted under, and the owner its
/// work runs for. The child's claim scope and enclosing process are the
/// opener's own ([`claim_scope`](Self::claim_scope),
/// [`ToolChildRequest::enclosing_process`]), derived rather than recorded
/// beside it, so no request can pair an opener with another scope.
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
    /// fences. The scope is the child's *claim address*, which
    /// [`claim_scope`](Self::claim_scope) derives; the opener is who owns it.
    pub opener: EffectOpener,
    /// Who the child's work runs for: the session and agent frame of a turn
    /// opener, or the process of a process opener.
    ///
    /// Carried beside the opener rather than derived from it because one
    /// session holds many frames (ADR 0092): a recovered child that re-derived
    /// a frame from its session would attribute its work to the wrong one.
    pub owner: crate::ExecutionOwner,
}

impl ToolChildScope {
    /// The scope this child is admitted and claimed under: its opener's own.
    ///
    /// Group formation derives the opener from the admitted scope
    /// ([`EffectOpener::for_scope`]), so this is that scope read back — the
    /// one the child's controller is constructed from and its envelope is
    /// addressed under.
    #[must_use]
    pub fn claim_scope(&self) -> AdmittedScope {
        self.opener.admitted_scope()
    }

    /// Refuses a binding whose opener and owner disagree.
    ///
    /// A turn opener already names its session and a process opener its
    /// process, so the pair is representable and invalid in exactly those
    /// ways. Refused at the boundary, as `EffectGroupShape::validate_wire`
    /// refuses its own two-halves mismatch, rather than left for a recovered
    /// child to attribute to the wrong owner.
    pub fn validate(&self) -> Result<(), RuntimeEffectControllerError> {
        let agrees = match (&self.owner, self.opener.process_id()) {
            (crate::ExecutionOwner::Process { process_id }, Some(opener_process)) => {
                process_id == opener_process
            }
            (crate::ExecutionOwner::Process { .. }, None) => false,
            (crate::ExecutionOwner::SessionFrame { session_id, .. }, _) => self
                .opener
                .session_id()
                .is_none_or(|opener_session| opener_session == session_id),
        };
        if !agrees {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectToolChildRequestOpener,
                format!(
                    "retained tool-child request binds opener {:?} but attributes its work to \
                     `{}`; a turn opener and a session-operation opener each name their own session, \
                     and a process opener its own process, so the two are one fact",
                    self.opener,
                    self.owner.runtime_owner()
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
/// | [`lineage`](Self::lineage) | group formation, as the parent the leaf's attempts descend from | the driver, as each attempt's causal parent |
/// | [`scope`](Self::scope) | group formation, as the opener and the owner | recovery (FIG-3396 §1) validates the opener; the driver reconstructs the admitted controller from its claim scope, sets the call's enclosing process from it, and binds its session-scoped services |
/// | [`cancellation_authority`](Self::cancellation_authority) | group formation, from the opener's turn-control binding | the cooperative cancel path (FIG-2266) and the cancel disposition (FIG-3409) |
/// | [`execution_env`](Self::execution_env) | group formation, from `captured_process_execution_env_ref` (required) | the driver, to resolve the captured environment; held by its `ArtifactReferrer::Execution` edge until the journal settles |
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
    /// The parent invocation this child's attempts descend from.
    pub lineage: ToolAttemptLineage,
    /// Where this child runs and whose work it is.
    pub scope: ToolChildScope,
    /// The turn-cancellation authority that may cancel this child.
    ///
    /// This is the identity `turn_control_binding_id_for_scope` mints and
    /// `binding_id_admits_scope` checks — the address the cooperative cancel
    /// path (FIG-2266) signals and the one FIG-3409's cancel disposition is
    /// fenced on. Typed rather than a bare string because a frozen durable
    /// shape may not carry an unvalidated identity. Required: every opener
    /// journals through a durable authority, so every child has one.
    pub cancellation_authority: TurnControlBindingId,
    /// The captured process-execution environment this child resolves, retained
    /// under its `ArtifactReferrer::Execution` edge until the journal settles.
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
    /// The session facts the child's authority is bound from on every path
    /// (FIG-3712).
    pub session: ToolChildSessionFacts,
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
            ToolChildCompletionRouting::Durable => Some((
                self.scope.claim_scope().into_scope(),
                crate::AwaitEventWaitIdentity::tool_completion(self.call.call_id.clone()),
            )),
        }
    }

    /// Assembles a request at the current format version.
    ///
    /// The facts with no sensible absence are taken here, so a caller cannot
    /// omit an opener, an admission or a cancellation authority by forgetting
    /// a field.
    #[must_use]
    #[expect(
        clippy::too_many_arguments,
        reason = "each argument is a recorded fact with no sensible absence; a builder would let a caller omit one"
    )]
    pub fn new(
        call: PreparedToolCall,
        admission: ToolChildAdmission,
        lineage: ToolAttemptLineage,
        scope: ToolChildScope,
        cancellation_authority: TurnControlBindingId,
        execution_env: ProcessExecutionEnvRef,
        completion_routing: ToolChildCompletionRouting,
        session: ToolChildSessionFacts,
    ) -> Self {
        Self {
            version: TOOL_CHILD_REQUEST_VERSION,
            call,
            admission,
            lineage,
            scope,
            cancellation_authority,
            execution_env,
            completion_routing,
            session,
        }
    }

    /// The process this call executes inside: the opener itself when the
    /// opener is a process, and none otherwise.
    #[must_use]
    pub fn enclosing_process(&self) -> Option<&ProcessId> {
        self.scope.opener.process_id()
    }

    /// Refuses an envelope address that is not this child's claim scope.
    ///
    /// The child's controller is constructed under its opener's scope, so an
    /// envelope addressed anywhere else would be claimed under one scope and
    /// run under another.
    pub fn validate_address(
        &self,
        address: &crate::EffectAddress,
    ) -> Result<(), RuntimeEffectControllerError> {
        let claim_scope = self.scope.claim_scope();
        if &address.execution_scope == claim_scope.scope() {
            return Ok(());
        }
        Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectToolChildRequestOpener,
            format!(
                "tool-child envelope is addressed under {:?} but its retained request opens \
                 under `{}`; a child is claimed under its opener's own scope, so the two are \
                 one fact",
                address.execution_scope,
                self.scope.opener.render(),
            ),
        ))
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
        self.scope.validate()?;
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
