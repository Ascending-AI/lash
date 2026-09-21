//! What a tool child accumulated that its opener cannot read out of its own
//! address space (ADR 0099 §6, §13, FIG-2266).
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
//! # This is a journaled shape
//!
//! [`ToolChildCapture`] is a field of
//! [`RuntimeEffectOutcome::ToolInvocation`](super::envelope::RuntimeEffectOutcome::ToolInvocation),
//! so it is written into the journal and read back by a replay. It carries its
//! own format version and is guarded by `scripts/versioned-surfaces.toml`
//! through [`TOOL_CHILD_CAPTURE_VERSION`], for the same reason
//! `TOOL_CHILD_REQUEST_VERSION` guards the request: a build that cannot
//! reconstruct a capture completely must refuse it rather than silently serve
//! an opener a prefix of what its child produced.
//!
//! It is a **separate** constant from the request's, not a second guard on it.
//! The request is the child's admitted authority and the capture is what the
//! child produced; they move for different reasons, on different lanes, and a
//! shared constant would force a version bump on one lane every time the other
//! learned a field.
//!
//! # What FIG-3411 must persist
//!
//! FIG-3411 owns carriage and retention across recovery (ADR 0099 §6, §8, §12,
//! §13). This module owes it a shape that is complete and stated, and the
//! statement is:
//!
//! * **[`possession`](ToolChildCapture::possession) must survive a handover
//!   whole and be incorporated before the opener takes any externally effective
//!   step that could read it.** §6 makes incorporation an ordered, recorded
//!   mapping precisely because a possession granted *earlier than the original
//!   execution granted it* changes authorization. A retention format that
//!   merges possession sets without preserving which child produced which id
//!   cannot honour that.
//! * **[`checkpoint_messages`](ToolChildCapture::checkpoint_messages) must be
//!   incorporated exactly once per child.** They are committed messages; a
//!   redrive that re-delivers them duplicates turn content, and one that drops
//!   them loses a commitment the tool already made.
//! * **[`usage`](ToolChildCapture::usage) must be incorporated idempotently and
//!   stay attributed to its original opener** (§13: "A late known fact stays
//!   attributed to its original opener"; "Retained facts are incorporated
//!   idempotently before final accounting"). The
//!   [`llm_call_id`](ToolChildUsageFact::llm_call_id) and
//!   [`provider_attempts`](ToolChildUsageFact::provider_attempts) pair is
//!   carried for exactly that: it is ADR 0032's provider-transport attempt
//!   identity, and it is what lets a second attach of the same fact be
//!   recognised rather than double-counted.
//! * **Absence is not zero.** An empty [`usage`](ToolChildCapture::usage) means
//!   "this child is not known to have spent anything", never "this child spent
//!   nothing"; §13 and ADR 0032 both refuse the zero-fill. A retention format
//!   that materializes a zero row for an uncaptured child publishes a false
//!   fact.
//!
//! # Cancellation does not empty it
//!
//! §13 is explicit: "Cancellation refuses semantic results and new work; it does
//! not discard known usage attributable to the admitted child." The capture is
//! therefore accumulated as the child runs and read at the child's *exit*,
//! whatever that exit is — a success terminal, a failure terminal, or a
//! cancelled attempt whose value is never returned. Nothing on this type is
//! conditioned on the child having produced a result.

use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};

use super::executor::RuntimeEffectControllerError;
use crate::{LlmCallId, PluginMessage, ProcessId, TokenUsage};

/// The durable format version of a tool child's captured facts.
///
/// Guarded by `scripts/versioned-surfaces.toml` over [`ToolChildCapture`] and
/// [`ToolChildUsageFact`] — the two named types and every type their payloads
/// reach that this crate owns — so a field added, retired or retyped fails the
/// repository gate rather than a production replay.
///
/// Version 1 is the shape FIG-2266 minted.
pub const TOOL_CHILD_CAPTURE_VERSION: u16 = 1;

/// One provider spend attributable to a tool child.
///
/// ADR 0099 §13 identifies a nested LLM call by its `(LlmCallId,
/// provider-attempt ordinal)` pair from ADR 0032, "and is **not** by itself a
/// tool driver's lineage, which is why the opener and invocation are named
/// beside it" — the opener and invocation are the child's own, which the
/// envelope this rides in already names, so they are not repeated here.
///
/// The ordinal is carried as a **count of provider attempts** rather than as a
/// list of per-attempt usages, because the sealed
/// [`LlmCallRecord`](crate::LlmCallRecord) already totals the call's usage and
/// a second per-attempt breakdown here would be a copy free to disagree with
/// the record the trace sink keeps.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolChildUsageFact {
    /// The nested call this spend belongs to.
    pub llm_call_id: LlmCallId,
    /// How many provider attempts that call made, so a re-attached fact can be
    /// recognised as the same one rather than counted again.
    pub provider_attempts: u32,
    /// What the call is known to have spent. Never zero-filled: a call with no
    /// known usage contributes no fact at all (§13, ADR 0032).
    pub usage: TokenUsage,
}

/// The semantic facts one tool child accumulated, carried on its outcome.
///
/// Default-constructed at the current [`TOOL_CHILD_CAPTURE_VERSION`], so a
/// capture that carried nothing and was therefore skipped on the wire decodes
/// back into a capture this build can read rather than into version `0`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolChildCapture {
    /// The durable format version, refused rather than defaulted.
    pub version: u16,
    /// Processes this child started, in the order its realized intent outcomes
    /// reported them (ADR 0099 §6).
    ///
    /// Read out of the same realized outcome the bound value's projection is
    /// taken from, exactly as `record_processes_started_by_intents` does for an
    /// in-turn call, because a possession set assembled from anywhere else
    /// could name a process the child's own result never bound.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub possession: Vec<ProcessId>,
    /// Messages an after-tool directive committed to this child's turn.
    ///
    /// Child-local: the driver rebinds a fresh
    /// [`CheckpointMessageBuffer`](crate::tool_dispatch::CheckpointMessageBuffer)
    /// so the child cannot write into an opener buffer it does not own, and
    /// what the buffer holds at the child's exit rides here instead.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoint_messages: Vec<PluginMessage>,
    /// Provider spend attributable to this child, including a spend made by an
    /// attempt that was cancelled before it returned a value (§13).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usage: Vec<ToolChildUsageFact>,
}

impl Default for ToolChildCapture {
    fn default() -> Self {
        Self {
            version: TOOL_CHILD_CAPTURE_VERSION,
            possession: Vec::new(),
            checkpoint_messages: Vec::new(),
            usage: Vec::new(),
        }
    }
}

impl ToolChildCapture {
    /// Whether this child captured nothing at all.
    ///
    /// Read by the outcome's `skip_serializing_if`, so a child that produced no
    /// possession, no committed message and no known spend adds no bytes to the
    /// journal and leaves the ungrouped outcome corpus byte-identical.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.possession.is_empty() && self.checkpoint_messages.is_empty() && self.usage.is_empty()
    }

    /// Refuses a capture this build cannot read completely.
    ///
    /// Called by envelope validation, so a capture from a format this build
    /// does not reconstruct is refused where it is decoded rather than served
    /// to an opener as a partial set of its child's facts.
    pub fn validate(&self) -> Result<(), RuntimeEffectControllerError> {
        if self.version != TOOL_CHILD_CAPTURE_VERSION {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectToolChildCaptureVersion,
                format!(
                    "tool-child capture records format version {}, and this build reads version \
                     {TOOL_CHILD_CAPTURE_VERSION}; a capture that cannot be read completely is \
                     refused rather than served as a prefix of what the child produced",
                    self.version
                ),
            ));
        }
        Ok(())
    }
}

/// The live accumulator a driver installs on one child's rebound context.
///
/// Not journaled and not part of the guarded shape: it is the in-memory place
/// the child's spends land while it runs, and
/// [`take`](ToolChildUsageLedger::take) turns it into the journaled
/// [`ToolChildCapture::usage`] at the child's exit.
///
/// Shared by `Arc` because the child's direct-completion client is cloned into
/// every nested future that might spend, and a ledger that was moved instead of
/// shared would record only the first one's spends.
///
/// # This is a second reader, never a second ledger
///
/// The opener's own token ledger is untouched. On an in-process tier a child's
/// nested call already merges into it exactly once —
/// `crates/lash-core/src/runtime/session_manager/direct_outcome.rs` records
/// "into the shared token ledger only … persisted exactly once by the final
/// turn commit" — and this accumulator only *also* names the spend as the
/// child's, which is what §13's cross-boundary attribution needs and what an
/// address space that is not the opener's has no other way to report.
#[derive(Clone, Default)]
pub struct ToolChildUsageLedger {
    facts: Arc<Mutex<Vec<ToolChildUsageFact>>>,
}

impl ToolChildUsageLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one nested call's known spend.
    ///
    /// A call with no known usage records nothing: unknown is a value and zero
    /// is a false fact (§13, ADR 0032).
    pub fn record(&self, call_record: &crate::LlmCallRecord, usage: &TokenUsage) {
        if usage == &TokenUsage::default() {
            return;
        }
        self.facts.lock_recover().push(ToolChildUsageFact {
            llm_call_id: call_record.call_id.clone(),
            provider_attempts: u32::try_from(call_record.attempts.len()).unwrap_or(u32::MAX),
            usage: usage.clone(),
        });
    }

    /// Takes everything recorded so far, leaving the ledger empty.
    #[must_use]
    pub fn take(&self) -> Vec<ToolChildUsageFact> {
        std::mem::take(&mut *self.facts.lock_recover())
    }
}

impl std::fmt::Debug for ToolChildUsageLedger {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolChildUsageLedger")
            .field("facts", &self.facts.lock_recover().len())
            .finish()
    }
}

#[cfg(test)]
#[path = "tool_child_capture/tests.rs"]
mod tests;
