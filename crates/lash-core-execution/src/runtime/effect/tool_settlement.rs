//! What a tool child settled, carried on its outcome for the opener to
//! incorporate (ADR 0099 §6, §13, FIG-2266).
//!
//! # Why anything travels at all
//!
//! A tool child of an effect group runs against a
//! [`ToolDispatchContext`](crate::tool_dispatch::ToolDispatchContext) the driver
//! rebound from its recorded request. Three of that context's fields are
//! *child-local buffers* rather than wiring: the checkpoint-message queue and
//! the trigger-outcome queue are fresh per child. A child that dropped them
//! would take its semantic facts with it. What a child spends is not among
//! them: each of its `ToolAttempt` effects is a spending effect whose usage run
//! delivers its own facts (ADR 0125), so no settlement carries usage and no
//! incorporation charges it.
//!
//! On the in-process tiers the loss is already visible.
//! `crates/lash-core-execution/src/session/process_handles.rs` explains what
//! possession costs when it is not carried: "A start declared as a tool intent
//! is realized in `tool_dispatch`, which holds no runtime execution context, so
//! nothing recorded it and the child was unreachable to the very run that
//! started it — `await handle` refused with `ProcessNotVisible`". ADR 0099 §3
//! puts the driver in exactly that position on purpose (`ToolContext`'s
//! `runtime_execution_context` is `None` for a group child), so the facts are
//! captured *into the child's outcome* instead of into a context the child does
//! not have.
//!
//! # Two journaled shapes, not one
//!
//! Facts are captured at **both** durable boundaries:
//!
//! * [`ToolAttemptCapture`] rides
//!   [`RuntimeEffectOutcome::ToolAttempt`](super::envelope::RuntimeEffectOutcome::ToolAttempt):
//!   the `EnqueueMessages` facts one atomic attempt produced are journaled
//!   with the attempt itself, and restored into the
//!   child's buffers when the attempt replays. Without this, a crash after an
//!   attempt committed but before the invocation settled would replay the
//!   attempt's terminal without re-running its producers, and the settlement
//!   would silently lack the attempt's facts.
//! * [`ToolSettlement`] rides
//!   [`RuntimeEffectOutcome::ToolInvocation`](super::envelope::RuntimeEffectOutcome::ToolInvocation):
//!   the child's complete semantic record — realized intent outcomes, realized
//!   started-process identities, trigger receipts, the aggregated
//!   `EnqueueMessages` facts and the resolved `ModelToolReturn` — which the
//!   opener incorporates as evidence and never
//!   re-executes.
//!
//! Both carry their own format version, for the same reason
//! `TOOL_CHILD_REQUEST_VERSION` versions the request: a build that cannot
//! reconstruct a fact completely must refuse it rather than silently serve an
//! opener a prefix of what its child produced. They are **separate** constants,
//! not two guards on one shape: the attempt carrier also rides the ungrouped
//! `ToolAttempt` arm, while the settlement exists only for group children, and
//! the two move for different reasons.
//!
//! # What FIG-3411 must persist
//!
//! FIG-3411 owns carriage and retention across recovery (ADR 0099 §6, §8, §12,
//! §13), and the single incorporation operation that applies these recorded
//! semantic deltas without re-executing any of them. This module owes it a
//! shape that is complete and stated:
//!
//! * **[`intent_outcomes`](ToolSettlement::intent_outcomes) are the realized
//!   evidence, including refusals.** The child realized its declared intents
//!   itself, after its final attempt committed; nothing on the opener side ever
//!   executes a declaration.
//! * **[`possession`](ToolSettlement::possession) must survive a handover whole
//!   and be incorporated before the opener takes any externally effective step
//!   that could read it.** §6 makes incorporation an ordered, recorded mapping
//!   precisely because a possession granted *earlier than the original
//!   execution granted it* changes authorization.
//! * **[`triggers`](ToolSettlement::triggers) are receipts to incorporate, not
//!   work to redo.** Occurrence and reservation writes belonged to the child;
//!   the opener records their inclusion and never re-emits.
//! * **[`checkpoint_messages`](ToolSettlement::checkpoint_messages) must be
//!   incorporated exactly once per child.** They are committed messages; a
//!   redrive that re-delivers them duplicates turn content, and one that drops
//!   them loses a commitment the tool already made.
//! * **[`model_return`](ToolSettlement::model_return) is a recorded
//!   presentation, not an instruction to re-project.** The plugin projector is
//!   a singleton; the child ran it once at its own presentation boundary and
//!   journaled the resolved return — including any fallback or error text and
//!   the attachment-materialization notices computed under the child's
//!   *recorded* environment. Incorporation consumes the record; it never
//!   re-runs the projector.
//!
//! # Cancellation does not empty it
//!
//! Facts are journaled per attempt and accumulated as the child runs, so a
//! child cancelled before settlement leaves its attempts' captures in the
//! journal for close to incorporate. Its spend never depended on settlement:
//! each attempt's usage run delivered it when the attempt was recorded.

use serde::{Deserialize, Serialize};

use super::executor::RuntimeEffectControllerError;
use crate::tool_dispatch::ToolTriggerEffectOutcome;
use crate::{PluginMessage, ProcessId};

/// The durable format version of a tool child's settlement.
///
/// Version 1 is the shape FIG-2266 minted. Version 2 renames
/// the usage delta's `provider_attempt` from a count to the sealed provider
/// attempt's own ordinal — usage is journaled one fact per provider attempt,
/// so a billed failed attempt and the retry that replaced it each carry their
/// own spend. Version 3 adds
/// `ToolIntentRefusalReason::MintingGroupChildCancelled`, the §4 refusal an
/// intent minted by a cancel-decided group child carries. Version 4 is the
/// FIG-3411 incorporation change: the usage delta gains `source` and
/// `model` so a settlement delta charges the session token ledger under
/// exactly the `(source, model)` the live path would have used, and
/// [`ToolDispatchOutcome`](crate::tool_dispatch::ToolDispatchOutcome) —
/// guarded here because it rides the same journaled `ToolInvocation` outcome —
/// carries the aggregated attempt captures and trigger outcomes the
/// applicator incorporates.
/// Version 5 carries one ordered parts body without part lifecycle fields.
/// Version 6 (FIG-3515) answers a tool call with one tool-result part whose
/// content is ordered text and attachment blocks; a v5 settlement's
/// text-only results and call-bound attachment parts are refused.
/// Version 7 (FIG-3712) carries the stream events of a child that ran with no
/// live opener ([`ToolSettlement::stream`]); a v6 settlement is refused.
/// Version 8 (FIG-3607) carries process handles and possessions by minted
/// process id alone; a v7 settlement's incarnation-qualified process
/// references are refused.
/// Version 8 changed in place under the pre-1.0 version freeze (FIG-4236):
/// a settlement carries no usage, because each attempt's usage run delivers
/// its own facts (ADR 0125).
///
/// version_guard(
///     roots(ToolSettlement),
///     roots(
///         path = "crates/lash-core-execution/src/tool_dispatch/context.rs", ToolDispatchOutcome,
///         PendingToolDispatchOutcome,
///     ),
///     roots(path = "crates/lash-sansio/src/llm/types.rs", LlmCallId),
///     roots(path = "crates/lash-sansio/src/session_model/message.rs", FlatPart, FlatPartRef),
///     items(
///         path = "crates/lash-core-execution/src/runtime/effect/tool_settlement.rs",
///         path = "crates/lash-core-execution/src/tool_dispatch/context.rs",
///         path = "crates/lash-core-execution/src/triggers.rs",
///         path = "crates/lash-sansio/src/plugin.rs",
///         path = "crates/lash-sansio/src/session_model/message.rs",
///         path = "crates/lash-sansio/src/session_model/mod.rs",
///         path = "crates/lash-sansio/src/llm/types.rs",
///         path = "crates/lash-sansio/src/attachment.rs",
///         path = "crates/lash-sansio/src/tool_output.rs",
///         path = "crates/lash-sansio/src/effect_identity.rs",
///         path = "crates/lash-sansio/src/causal.rs",
///         path = "crates/lash-core-execution/src/runtime/effect/recorded_stream.rs",
///         ToolIntentKind,
///     ),
///     file(
///         path = "crates/lash-sansio/src/identity.rs",
///         cover("string_identity!", SessionId, ProcessId, TurnId, InputId),
///     ),
///     file(path = "crates/lash-sansio/src/tool_intents.rs", cover(tool_intent_variants)),
/// )
pub const TOOL_SETTLEMENT_VERSION: u16 = 1;

/// The durable format version of one atomic attempt's captured facts.
///
/// Version 1 is the shape FIG-2266 minted; version 2 is the same
/// usage-delta rename [`TOOL_SETTLEMENT_VERSION`] records; version 3 is
/// the same `source`/`model` addition [`TOOL_SETTLEMENT_VERSION`] 4 records —
/// a captured delta is only chargeable at incorporation when it carries the
/// labels the session ledger keys on.
/// Version 4 carries the same message cutover as settlement version 5.
/// Version 5 carries the same one-result-per-call cutover as settlement
/// version 6.
/// Version 6 carries the same minted-process-id cutover as settlement
/// version 7.
/// Version 6 changed in place under the pre-1.0 version freeze (FIG-4236):
/// a capture carries no usage (ADR 0125).
///
/// version_guard(
///     roots(ToolAttemptCapture),
///     roots(path = "crates/lash-sansio/src/llm/types.rs", LlmCallId),
///     roots(path = "crates/lash-sansio/src/session_model/message.rs", FlatPart, FlatPartRef),
///     roots(path = "crates/lash-sansio/src/session_model/mod.rs", TokenUsage),
///     file(
///         path = "crates/lash-sansio/src/identity.rs",
///         cover("string_identity!", SessionId, ProcessId, TurnId, InputId),
///     ),
/// )
pub const TOOL_ATTEMPT_CAPTURE_VERSION: u16 = 1;

/// The semantic facts one atomic `ToolAttempt` produced, journaled with it.
///
/// Carried by
/// [`RuntimeEffectOutcome::ToolAttempt`](super::envelope::RuntimeEffectOutcome::ToolAttempt)
/// so the attempt's committed result is self-describing: a replay that serves
/// this outcome restores these facts into the dispatch buffers instead of
/// re-running their producers. Empty captures are skipped on the wire, so an
/// attempt that committed no message and is not known to have spent anything
/// writes the same bytes it wrote before this field existed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolAttemptCapture {
    /// The durable format version, refused rather than defaulted.
    pub version: u16,
    /// The concrete `EnqueueMessages` facts after-tool directives committed
    /// during this attempt, in the order they were enqueued.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<PluginMessage>,
}

impl Default for ToolAttemptCapture {
    fn default() -> Self {
        Self {
            version: TOOL_ATTEMPT_CAPTURE_VERSION,
            messages: Vec::new(),
        }
    }
}

impl ToolAttemptCapture {
    /// Whether this attempt captured nothing at all.
    ///
    /// Read by the outcome's `skip_serializing_if`, so an attempt that produced
    /// no message adds no bytes to the journal and leaves the ungrouped
    /// outcome corpus byte-identical. What the attempt spent is not a
    /// capture: the attempt's usage run carries it (ADR 0125).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Refuses a capture this build cannot read completely.
    ///
    /// Called by outcome validation, so a capture from a format this build does
    /// not reconstruct is refused where it is decoded rather than replayed as a
    /// partial restore of what the attempt produced.
    pub fn validate(&self) -> Result<(), RuntimeEffectControllerError> {
        if self.version != TOOL_ATTEMPT_CAPTURE_VERSION {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectToolAttemptCaptureVersion,
                format!(
                    "tool-attempt capture records format version {}, and this build reads version \
                     {TOOL_ATTEMPT_CAPTURE_VERSION}; a capture that cannot be read completely is \
                     refused rather than restored as a prefix of what the attempt produced",
                    self.version
                ),
            ));
        }
        Ok(())
    }
}

/// The complete semantic record one tool child of an effect group settled on.
///
/// This is the unit FIG-3411's incorporation consumes: every channel the
/// opener owes the child's facts is here, each as recorded evidence rather
/// than a declaration to execute. It is deliberately *not* optional on its
/// outcome arm: a child that reached a terminal always produced a settled
/// presentation, so a settlement is always journaled.
///
/// `model_return` has no serde default on purpose — a settlement without one
/// means the presentation boundary was never reached, which is a refusal the
/// reader must see, not a hole to fill.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSettlement {
    /// The durable format version, refused rather than defaulted.
    pub version: u16,
    /// The realized outcomes of the intents the child's final attempt
    /// declared — executions and refusals alike, realized by the child after
    /// its attempt committed (ADR 0042), never to be re-executed by the
    /// opener.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intent_outcomes: Vec<crate::ToolIntentExecutionOutcome>,
    /// Processes this child started, in the order its realized intent outcomes
    /// reported them (ADR 0099 §6).
    ///
    /// Read out of the same realized outcome the bound value's projection is
    /// taken from, exactly as `record_processes_started_by_intents` does for an
    /// in-turn call, because a possession set assembled from anywhere else
    /// could name a process the child's own result never bound.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub possession: Vec<ProcessId>,
    /// Trigger receipts the child emitted, carried exactly as a
    /// [`ToolAttempt`](super::envelope::RuntimeEffectOutcome::ToolAttempt)
    /// outcome carries them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<ToolTriggerEffectOutcome>,
    /// The `EnqueueMessages` facts committed while the child ran, aggregated
    /// across its attempts in attempt order.
    ///
    /// Child-local: the driver rebinds a fresh
    /// [`CheckpointMessageBuffer`](crate::tool_dispatch::CheckpointMessageBuffer)
    /// so the child cannot write into an opener buffer it does not own, and
    /// each attempt's own capture is restored into that buffer as its outcome
    /// is consumed, so what the buffer holds at the child's exit is the journaled
    /// aggregate rather than a live-only view.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoint_messages: Vec<PluginMessage>,
    /// The stream events a child emitted while no opener was live where it
    /// ran (FIG-3712), in the journal's own bounded shape.
    ///
    /// A child that runs beside its live opener streams its events to the
    /// opener as they happen and records none here. A child that built its
    /// own context has no stream to reach, so its events are recorded, and
    /// the opener emits them when it incorporates this settlement. See
    /// [`RecordedChildStream`](super::RecordedChildStream).
    #[serde(default, skip_serializing_if = "super::RecordedChildStream::is_empty")]
    pub stream: super::RecordedChildStream,
    /// The resolved model-facing return: the session's singleton plugin
    /// projector run once at this child's presentation boundary, or its
    /// recorded fallback on projector error, plus the
    /// attachment-materialization notices computed under the child's recorded
    /// environment. Incorporation consumes this record verbatim; it never
    /// re-projects.
    pub model_return: crate::ModelToolReturn,
}

impl ToolSettlement {
    /// Refuses a settlement this build cannot read completely.
    ///
    /// Called by envelope validation, so a settlement from a format this build
    /// does not reconstruct is refused where it is decoded rather than served
    /// to an opener as a partial set of its child's facts.
    pub fn validate(&self) -> Result<(), RuntimeEffectControllerError> {
        if self.version != TOOL_SETTLEMENT_VERSION {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectToolSettlementVersion,
                format!(
                    "tool settlement records format version {}, and this build reads version \
                     {TOOL_SETTLEMENT_VERSION}; a settlement that cannot be read completely is \
                     refused rather than served as a prefix of what the child produced",
                    self.version
                ),
            ));
        }
        Ok(())
    }

    /// Aggregates a dispatch outcome into the settlement it settles on.
    ///
    /// The one constructor for every caller that owns a terminal
    /// [`ToolDispatchOutcome`](crate::tool_dispatch::ToolDispatchOutcome):
    /// the scalar/batch completion path and the group-child driver alike.
    /// `possession` is read out of the same realized intent outcomes the bound
    /// value's projection is taken from — the derivation
    /// `record_processes_started_by_intents` used before the applicator owned
    /// it — so a possession set assembled from anywhere else could name a
    /// process the tool's own result never bound. `checkpoint_messages` are
    /// the aggregated per-attempt captures in attempt order;
    /// `triggers` are the receipts the outcome journaled. `model_return` is
    /// supplied by the caller because the presentation boundary owns it.
    pub fn from_dispatch(
        outcome: &crate::tool_dispatch::ToolDispatchOutcome,
        model_return: crate::ModelToolReturn,
    ) -> Self {
        Self {
            version: TOOL_SETTLEMENT_VERSION,
            intent_outcomes: outcome.intent_outcomes.clone(),
            possession: settlement_possession(&outcome.intent_outcomes),
            triggers: outcome.triggers.clone(),
            checkpoint_messages: outcome
                .captures
                .iter()
                .flat_map(|capture| capture.messages.iter().cloned())
                .collect(),
            stream: super::RecordedChildStream::default(),
            model_return,
        }
    }
}

/// The started-process identities a settlement carries: read out of the
/// realized `StartProcess` intent outcomes exactly as
/// `record_processes_started_by_intents` read them, so the opener's
/// incorporation grants possession of nothing the tool's own result did not
/// bind (ADR 0099 §6).
pub(crate) fn settlement_possession(
    intent_outcomes: &[crate::ToolIntentExecutionOutcome],
) -> Vec<ProcessId> {
    intent_outcomes
        .iter()
        .filter_map(|intent| match intent {
            crate::ToolIntentExecutionOutcome::Executed { kind, result, .. }
                if *kind == crate::ToolIntentKind::StartProcess =>
            {
                crate::process_id_from_handle_json(result).ok()
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
#[path = "tool_settlement/tests.rs"]
mod tests;
