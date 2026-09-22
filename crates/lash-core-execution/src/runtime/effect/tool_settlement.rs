//! What a tool child settled, carried on its outcome for the opener to
//! incorporate (ADR 0099 §6, §13, FIG-2266).
//!
//! # Why anything travels at all
//!
//! A tool child of an effect group runs against a
//! [`ToolDispatchContext`](crate::tool_dispatch::ToolDispatchContext) the driver
//! rebound from its recorded request. Three of that context's fields are
//! *child-local buffers* rather than wiring: the checkpoint-message queue and
//! the trigger-outcome queue are fresh per child, and the usage a child spends
//! is attributable to the child before it is attributable to anything else. A
//! child that dropped them would take its semantic facts with it.
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
//!   the `EnqueueMessages` facts and managed LLM usage one atomic attempt
//!   produced are journaled with the attempt itself, and restored into the
//!   child's buffers when the attempt replays. Without this, a crash after an
//!   attempt committed but before the invocation settled would replay the
//!   attempt's terminal without re-running its producers, and the settlement
//!   would silently lack the attempt's facts.
//! * [`ToolSettlement`] rides
//!   [`RuntimeEffectOutcome::ToolInvocation`](super::envelope::RuntimeEffectOutcome::ToolInvocation):
//!   the child's complete semantic record — realized intent outcomes, realized
//!   started-process identities, trigger receipts, the aggregated
//!   `EnqueueMessages` facts, per-attempt usage deltas and the resolved
//!   `ModelToolReturn` — which the opener incorporates as evidence and never
//!   re-executes.
//!
//! Both carry their own format version and are guarded by
//! `scripts/versioned-surfaces.toml` through [`TOOL_SETTLEMENT_VERSION`] and
//! [`TOOL_ATTEMPT_CAPTURE_VERSION`], for the same reason
//! `TOOL_CHILD_REQUEST_VERSION` guards the request: a build that cannot
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
//! * **[`usage`](ToolSettlement::usage) must be incorporated idempotently and
//!   stay attributed to its original opener** (§13: "A late known fact stays
//!   attributed to its original opener"; "Retained facts are incorporated
//!   idempotently before final accounting"). Each delta's full identity is
//!   `(opener, invocation, attempt, llm_call_id, provider_attempts)` — the
//!   opener and the child's invocation are named by the envelope this rides in,
//!   and [`attempt`](ToolUsageDelta::attempt) resolves to the attempt's
//!   invocation — which is what lets a second attach of the same fact be
//!   recognised rather than double-counted.
//! * **[`model_return`](ToolSettlement::model_return) is a recorded
//!   presentation, not an instruction to re-project.** The plugin projector is
//!   a singleton; the child ran it once at its own presentation boundary and
//!   journaled the resolved return — including any fallback or error text and
//!   the attachment-materialization notices computed under the child's
//!   *recorded* environment. Incorporation consumes the record; it never
//!   re-runs the projector.
//! * **Absence is not zero.** An empty [`usage`](ToolSettlement::usage) means
//!   "this child is not known to have spent anything", never "this child spent
//!   nothing"; §13 and ADR 0032 both refuse the zero-fill.
//!
//! # Cancellation does not empty it
//!
//! §13 is explicit: "Cancellation refuses semantic results and new work; it
//! does not discard known usage attributable to the admitted child." Facts are
//! journaled per attempt and accumulated as the child runs, so an attempt that
//! settled with a cancelled output still carries its spend, and a child
//! cancelled before settlement leaves its attempts' captures in the journal
//! for close to incorporate.

use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};

use super::executor::RuntimeEffectControllerError;
use crate::tool_dispatch::ToolTriggerEffectOutcome;
use crate::{LlmCallId, PluginMessage, ProcessId, TokenUsage};

/// The durable format version of a tool child's settlement.
///
/// Guarded by `scripts/versioned-surfaces.toml` over [`ToolSettlement`] and
/// [`ToolUsageDelta`] — the named types and every type their payloads reach
/// that this crate owns — so a field added, retired or retyped fails the
/// repository gate rather than a production replay.
///
/// Version 1 is the shape FIG-2266 minted. Version 2 renames
/// [`ToolUsageDelta::provider_attempt`] from a count to the sealed provider
/// attempt's own ordinal — usage is journaled one fact per provider attempt,
/// so a billed failed attempt and the retry that replaced it each carry their
/// own spend. Version 3 adds
/// `ToolIntentRefusalReason::MintingGroupChildCancelled`, the §4 refusal an
/// intent minted by a cancel-decided group child carries. Version 4 is the
/// FIG-3411 incorporation change: [`ToolUsageDelta`] gains `source` and
/// `model` so a settlement delta charges the session token ledger under
/// exactly the `(source, model)` the live path would have used, and
/// [`ToolDispatchOutcome`](crate::tool_dispatch::ToolDispatchOutcome) —
/// guarded here because it rides the same journaled `ToolInvocation` outcome —
/// carries the aggregated attempt captures and trigger outcomes the
/// applicator incorporates.
pub const TOOL_SETTLEMENT_VERSION: u16 = 4;

/// The durable format version of one atomic attempt's captured facts.
///
/// Guarded by `scripts/versioned-surfaces.toml` over [`ToolAttemptCapture`]
/// and [`ToolUsageDelta`], which the capture's `usage` list is made of.
/// Version 1 is the shape FIG-2266 minted; version 2 is the same
/// [`ToolUsageDelta`] rename [`TOOL_SETTLEMENT_VERSION`] records; version 3 is
/// the same `source`/`model` addition [`TOOL_SETTLEMENT_VERSION`] 4 records —
/// a captured delta is only chargeable at incorporation when it carries the
/// labels the session ledger keys on.
pub const TOOL_ATTEMPT_CAPTURE_VERSION: u16 = 3;

/// One provider spend attributable to one attempt of a tool child.
///
/// ADR 0099 §13 identifies a nested LLM call by its `(LlmCallId,
/// provider-attempt ordinal)` pair from ADR 0032, "and is **not** by itself a
/// tool driver's lineage, which is why the opener and invocation are named
/// beside it". The full deduplication identity is therefore `(opener,
/// invocation, attempt, llm_call_id, provider_attempt)`: the opener and the
/// child's invocation are the envelope's, [`attempt`](Self::attempt) resolves
/// to the attempt invocation it was spent under, and the ADR 0032 pair is the
/// provider's own record identity.
///
/// One delta per sealed provider attempt — not one per call — because a call
/// that failed after the provider billed it and a retry that then succeeded
/// are two spends, and a per-call fact would either lose the first or
/// double-count the second. The ordinal is the sealed
/// [`AttemptRecord`](crate::LlmCallRecord)'s own, so the fact names the same
/// attempt the trace sink does.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolUsageDelta {
    /// Which attempt of the invocation made the spend. Attempts are numbered
    /// from 1; `0` names a spend made outside any attempt frame (the
    /// orchestrating lane, which is coordination and has no `ToolAttempt`).
    pub attempt: u32,
    /// The nested call this spend belongs to.
    pub llm_call_id: LlmCallId,
    /// Which sealed provider attempt inside that call the spend belongs to —
    /// the `AttemptRecord`'s own ordinal, so a billed failure and its retry
    /// are distinct facts rather than a summed or lost one.
    pub provider_attempt: u32,
    /// The usage-source label the live path would have charged under
    /// (`usage_capability.record_token_usage(source, model, usage)`). Carried
    /// because incorporation charges the opener's session ledger and a delta
    /// without its labels could only charge under an invented identity.
    pub source: String,
    /// The model label the live path would have charged under, paired with
    /// [`source`](Self::source).
    pub model: String,
    /// What the call is known to have spent. Never zero-filled: a provider
    /// attempt reporting no usage contributes no delta at all (§13, ADR 0032).
    pub usage: TokenUsage,
}

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
    /// Managed LLM usage this attempt is known to have spent, including a
    /// spend made by an attempt that was cancelled before it returned a value
    /// (§13). Each delta's `attempt` is this attempt's ordinal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usage: Vec<ToolUsageDelta>,
}

impl Default for ToolAttemptCapture {
    fn default() -> Self {
        Self {
            version: TOOL_ATTEMPT_CAPTURE_VERSION,
            messages: Vec::new(),
            usage: Vec::new(),
        }
    }
}

impl ToolAttemptCapture {
    /// Whether this attempt captured nothing at all.
    ///
    /// Read by the outcome's `skip_serializing_if`, so an attempt that produced
    /// no message and is not known to have spent anything adds no bytes to the
    /// journal and leaves the ungrouped outcome corpus byte-identical.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty() && self.usage.is_empty()
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
    /// Provider spend attributable to this child, aggregated across its
    /// attempts and including a spend made by an attempt that was cancelled
    /// before it returned a value (§13).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usage: Vec<ToolUsageDelta>,
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
    /// process the tool's own result never bound. `checkpoint_messages` and
    /// `usage` are the aggregated per-attempt captures in attempt order;
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
            usage: outcome
                .captures
                .iter()
                .flat_map(|capture| capture.usage.iter().cloned())
                .collect(),
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
                crate::ProcessRef::from_handle_json(result)
                    .ok()
                    .map(|process_ref| process_ref.process_id)
            }
            _ => None,
        })
        .collect()
}

/// The live accumulator a driver installs on one child's rebound context.
///
/// Not journaled and not part of either guarded shape: it is the in-memory
/// place a tool's spends land while it runs, and
/// [`take`](ToolUsageLedger::take) turns it into the journaled
/// [`ToolSettlement::usage`] or [`ToolAttemptCapture::usage`] at the boundary
/// that owns it.
///
/// Shared by `Arc` because the child's direct-completion client is cloned into
/// every nested future that might spend, and a ledger that was moved instead of
/// shared would record only the first one's spends.
///
/// # Two roles, one shape
///
/// * A **child aggregate** (`new`) accepts already-stamped deltas through
///   [`extend`](ToolUsageLedger::extend) as each journaled attempt outcome is
///   consumed, and also records spends made outside any attempt frame — the
///   orchestrating lane has no `ToolAttempt`, so its `record` stamps attempt
///   `0`.
/// * An **attempt sink** (`for_attempt`) is installed on the dispatch context's
///   direct-completion client for one `ToolAttempt` and stamps every spend it
///   sees with that attempt's ordinal, so the journaled capture attributes the
///   delta to the attempt that made it rather than to the whole invocation.
///
/// # The capture, never a second charge
///
/// While a sink is installed, the live path does not charge the session
/// token ledger at all: `direct_outcome.rs` records the sealed record's spend
/// into this sink *instead* of merging it live (FIG-3411). The journaled
/// delta is then the only carrier, and the opener charges the session ledger
/// exactly once at settlement incorporation — a remote child's ledger is not
/// the parent's ledger (§13), and on an in-process tier the suppression is
/// what keeps the same spend from being billed twice.
#[derive(Clone)]
pub struct ToolUsageLedger {
    facts: Arc<Mutex<Vec<ToolUsageDelta>>>,
    /// The attempt ordinal every recorded spend is stamped with. `0` for an
    /// aggregate or a spend made outside any attempt frame; attempts are
    /// numbered from 1.
    attempt: u32,
}

impl ToolUsageLedger {
    /// An empty aggregate ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::for_attempt(0)
    }

    /// An empty sink stamping every spend with one attempt's ordinal.
    #[must_use]
    pub fn for_attempt(attempt: u32) -> Self {
        Self {
            facts: Arc::new(Mutex::new(Vec::new())),
            attempt,
        }
    }

    /// Records every known spend a sealed call record carries.
    ///
    /// One delta per provider attempt that reports usage, stamped with the
    /// attempt's own ordinal: a billed failed attempt and the retry that
    /// succeeded are two facts, never a summed or a lost one. An attempt
    /// whose `usage` is absent — unreported by the provider, aborted before
    /// the response, or failed before billing — records nothing; an attempt
    /// that reports *zero* is a fact, not a hole: billed-at-zero is a
    /// statement the provider made, and ADR 0032's `Some(0)` is not `None`
    /// (§13).
    ///
    /// `source` and `model` are the labels the live path would have charged
    /// the session token ledger under; the delta carries them so the opener's
    /// incorporation charges under exactly that identity rather than an
    /// invented one.
    pub fn record(&self, call_record: &crate::LlmCallRecord, source: &str, model: &str) {
        let mut facts = self.facts.lock_recover();
        for attempt in &call_record.attempts {
            let Some(usage) = attempt.usage.as_ref() else {
                continue;
            };
            facts.push(ToolUsageDelta {
                attempt: self.attempt,
                llm_call_id: call_record.call_id.clone(),
                provider_attempt: attempt.ordinal,
                source: source.to_string(),
                model: model.to_string(),
                usage: super::outcome::token_usage_from_llm(usage),
            });
        }
    }

    /// Merges deltas already journaled by an attempt's capture.
    ///
    /// The restore half of attempt-boundary capture: whether the outcome was
    /// just executed or served by replay, its recorded deltas land here exactly
    /// once per consumption.
    pub fn extend(&self, deltas: impl IntoIterator<Item = ToolUsageDelta>) {
        self.facts.lock_recover().extend(deltas);
    }

    /// Takes everything recorded so far, leaving the ledger empty.
    #[must_use]
    pub fn take(&self) -> Vec<ToolUsageDelta> {
        std::mem::take(&mut *self.facts.lock_recover())
    }
}

impl Default for ToolUsageLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ToolUsageLedger {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolUsageLedger")
            .field("facts", &self.facts.lock_recover().len())
            .field("attempt", &self.attempt)
            .finish()
    }
}

#[cfg(test)]
#[path = "tool_settlement/tests.rs"]
mod tests;
