use lash_protocol_types::TurnId;

use crate::support::{
    Arc, ToolCallRecord, TurnActivity, TurnActivitySink, TurnOutcome, async_trait,
};
use crate::transcript::CommittedTurnWithCalls;

pub use lash_core::facade_support::{TurnIssue, TurnIssueSeverity};
/// Typed turn-failure vocabulary carried on [`TurnIssue`] and on session error
/// envelopes. A host branches on these instead of matching the display string.
/// The namespaced [`FailureCode`](crate::provider::FailureCode) on `code`
/// fields lives in [`crate::provider`].
pub use lash_core::{TurnFailureCode, TurnFailureKind};

/// A tool call addressed through the turn whose ledger contains it.
/// Ids are opaque; readers never recover a turn by searching session history.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct TurnToolCallRef {
    pub turn_id: TurnId,
    pub call_id: crate::ToolCallId,
}

pub use lash_core::model_calls::{ModelCallEnd, ModelCallEndReason, ModelCallRecord};

/// A model call addressed through the turn that owns its record.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnModelCallRef {
    pub turn_id: TurnId,
    pub call_id: crate::LlmCallId,
}

/// An unsettled call is known to its open turn but has no sealed record yet.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ModelCallRead {
    Settled { record: Box<ModelCallRecord> },
    NotYetSettled { call_id: crate::LlmCallId },
    NotFound { call_id: crate::LlmCallId },
}

/// A committed-turn page that opted into decoding each returned turn's calls.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CommittedTurnsWithCallsPage {
    pub turns: Vec<CommittedTurnWithCalls>,
    /// Persist after applying every turn; continues the same committed feed.
    pub next: crate::persistence::CommittedTurnCursor,
}

/// A typed read of one session's recorded call. A body is exposed only
/// after its outcome commits; an admitted call without an outcome is pending.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolCallRead {
    Settled {
        #[serde(serialize_with = "crate::persistence::serialize_durable")]
        record: Box<ToolCallRecord>,
    },
    NotFound {
        call_id: crate::ToolCallId,
    },
    NotYetSettled {
        call_id: crate::ToolCallId,
    },
}

pub(crate) fn fresh_turn_id() -> TurnId {
    TurnId::from_uuid(uuid::Uuid::new_v4().as_u128())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct TurnOutput {
    /// The committed terminal outcome.
    pub outcome: TurnOutcome,
    /// Ordered activities observed while executing the turn.
    pub activities: Vec<TurnActivity>,
}

impl TurnOutput {
    /// Combine a committed outcome with the activities collected by its observer.
    ///
    /// The activities describe that observer's view; they need not include
    /// every activity the run emitted. The outcome is independent of that observation.
    pub fn new(outcome: TurnOutcome, activities: Vec<TurnActivity>) -> Self {
        Self {
            outcome,
            activities,
        }
    }

    /// Classify the committed outcome.
    pub fn status(&self) -> crate::TurnStatus {
        crate::TurnStatus::of_outcome(&self.outcome)
    }

    pub fn assistant_message(&self) -> Option<&str> {
        self.outcome.assistant_message()
    }

    /// See [`TurnOutcome::finished`].
    pub fn finished(&self) -> Option<(&str, &serde_json::Value)> {
        self.outcome.finished()
    }

    pub fn is_success(&self) -> bool {
        self.outcome.is_success()
    }

    /// See [`TurnOutcome::is_context_overflow`].
    pub fn is_context_overflow(&self) -> bool {
        self.outcome.is_context_overflow()
    }
}

/// Fans a turn's activity stream out to multiple consumers.
pub struct TurnActivityFanout {
    sinks: Vec<Arc<dyn TurnActivitySink>>,
}

impl TurnActivityFanout {
    pub fn new(sinks: impl IntoIterator<Item = Arc<dyn TurnActivitySink>>) -> Self {
        Self {
            sinks: sinks.into_iter().collect(),
        }
    }
}

#[async_trait]
impl TurnActivitySink for TurnActivityFanout {
    async fn emit(&self, activity: TurnActivity) {
        for sink in &self.sinks {
            sink.emit(activity.clone()).await;
        }
    }
}
