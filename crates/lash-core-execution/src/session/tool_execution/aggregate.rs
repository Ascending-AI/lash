//! Aggregates on the product path: `Promise.all`, `allSettled`, `race` and
//! `any` over tool calls and timers (ADR 0099 §10, §11; FIG-3397).
//!
//! One aggregate is one durable effect group. The caller hands over its
//! **unique** leaves in first-appearance order — a pending operation written
//! at two positions is one leaf, and the caller expands the outcome back to
//! positions (§10 L4) — and a consumer mode, which is never journaled; the
//! journaled wake policy is derived from it (L1).
//!
//! The answer is the total response algebra of L2: every result, one
//! selected settlement, a settled plain value, or `any`'s exhausted
//! rejections. Infrastructure failure and host cancellation are not in it:
//! they are [`ToolAggregateOutcome::HostControl`], which the caller raises on
//! its host-control channel and never turns into a leaf rejection (L3).
//!
//! # The immediate prefix
//!
//! Leaves already settled when the aggregate forms — a call refused or
//! completed during preparation, a leaf the caller resolved itself — and the
//! caller's first plain operand form a source-ordered prefix ahead of every
//! dispatched settlement (L5). The prefix may decide the aggregate, but only
//! after every pending leaf has been admitted: a group is opened first, and
//! its losers belong to the opener from that moment (§11 clause 3). Nothing
//! about a loser's value is ever synthesized (L6).

use super::*;

pub(crate) use super::group::ToolAggregateConsumer;

/// One unique leaf of an aggregate, in first-appearance order.
pub enum ToolAggregateLeaf {
    /// A tool call to admit as a group child.
    Tool(ToolInvocation),
    /// A timer from an unawaited `sleep(ms)`. Its deadline is recorded once,
    /// when the aggregate admits it (§11 clause 4).
    Timer { duration_ms: u64 },
    /// A leaf the caller settled before the aggregate formed. It takes its
    /// place in the immediate prefix; its value stays with the caller.
    Settled { fulfilled: bool },
}

/// One aggregate request.
pub struct ToolAggregateRequest {
    pub leaves: Vec<ToolAggregateLeaf>,
    pub consumer: ToolAggregateConsumer,
    /// For `race` and `any`: how many leaves precede the caller's first plain
    /// operand in source order. That operand decides the aggregate unless an
    /// earlier prefix leaf does.
    pub settled_value_after: Option<usize>,
    /// The aggregate's replay address (FIG-3586): the command key the
    /// issuing language runtime minted from the aggregate's issue ordinal. It
    /// is the group key and the group invocation's replay key, so every row
    /// the aggregate writes — the group, its children, its tool children's
    /// attempts, the timers' admission sample — lives under it. The leaves'
    /// content is never key material; it is checked at the group head.
    pub command: crate::CommandReplayKey,
}

/// One leaf's settled reply.
pub enum ToolAggregateLeafReply {
    Tool(Box<ToolInvocationReply>),
    /// A timer elapsed; its fulfilment value is `undefined`.
    Timer,
}

impl ToolAggregateLeafReply {
    fn fulfilled(&self) -> bool {
        match self {
            Self::Tool(reply) => matches!(reply.output.outcome, crate::ToolCallOutcome::Success(_)),
            Self::Timer => true,
        }
    }
}

/// The aggregate's answer (ADR 0099 §10 L2), per leaf where it carries
/// replies. A [`ToolAggregateLeaf::Settled`] leaf's slot is always `None`:
/// its value is the caller's.
pub enum ToolAggregateOutcome {
    /// Every leaf's reply: `allSettled`, a successful `all`.
    AllResults(Vec<Option<ToolAggregateLeafReply>>),
    /// The settlement that decided the aggregate.
    Selected {
        leaf: usize,
        reply: Option<ToolAggregateLeafReply>,
    },
    /// The caller's plain operand decided a `race` or an `any`.
    SettledValue,
    /// `any` with no fulfilment: every leaf's rejection, in leaf order.
    ExhaustedRejections(Vec<Option<ToolAggregateLeafReply>>),
    /// Infrastructure failure or host cancellation (§10 L3): raised on the
    /// caller's host-control channel, never as a leaf rejection.
    HostControl(String),
    /// The aggregate's tool calls would pass the session's recorded
    /// `max_tool_calls` (FIG-4546). The whole aggregate is refused before
    /// anything of it is journaled or dispatched. The program's failure, not
    /// the host's: a replay refuses the same aggregate.
    ToolCallLimitExceeded(crate::ToolCallLimitExceeded),
}

/// A continuation-safe address for one admitted aggregate. It contains no
/// prepared input, result payload, controller or issued future.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRunAggregateCursor {
    pub(crate) owner: crate::EffectOpener,
    pub(crate) key: String,
    pub(crate) positions: Vec<Option<usize>>,
    pub(crate) calls: std::collections::BTreeMap<usize, crate::ToolCallId>,
}

impl ToolRunAggregateCursor {
    pub fn key(&self) -> &str {
        &self.key
    }
}

pub enum ToolRunAggregatePoll {
    Pending,
    Ready {
        outcome: ToolAggregateOutcome,
        settlement_order: Vec<usize>,
    },
}

impl RuntimeExecutionContext<'_> {
    pub async fn admit_tool_run_aggregate(
        &self,
        request: ToolAggregateRequest,
    ) -> Result<ToolRunAggregateCursor, crate::RuntimeEffectControllerError> {
        let calls = request
            .leaves
            .iter()
            .filter(|leaf| matches!(leaf, ToolAggregateLeaf::Tool(_)))
            .count();
        self.reserve_tool_calls(&self.command_group_key(&request.command), calls)
            .await?;
        self.tool_run
            .as_ref()
            .ok_or_else(|| {
                crate::RuntimeEffectControllerError::from(
                    crate::tool_run::ContinuationRefusal::NotQuiescent,
                )
            })?
            .admit(request, self.parent_invocation.clone())
            .await
    }
    pub async fn consume_tool_run_aggregate(
        &self,
        cursor: &ToolRunAggregateCursor,
        consumer: ToolAggregateConsumer,
    ) -> Result<ToolRunAggregatePoll, crate::RuntimeEffectControllerError> {
        self.tool_run
            .as_ref()
            .ok_or_else(|| {
                crate::RuntimeEffectControllerError::from(
                    crate::tool_run::ContinuationRefusal::NotQuiescent,
                )
            })?
            .consume(cursor.clone(), consumer, false)
            .await
    }
    pub async fn await_tool_run_aggregate(
        &self,
        cursor: &ToolRunAggregateCursor,
        consumer: ToolAggregateConsumer,
    ) -> Result<ToolRunAggregatePoll, crate::RuntimeEffectControllerError> {
        let result = self
            .tool_run
            .as_ref()
            .ok_or_else(|| {
                crate::RuntimeEffectControllerError::from(
                    crate::tool_run::ContinuationRefusal::NotQuiescent,
                )
            })?
            .consume(cursor.clone(), consumer, true)
            .await;
        if result
            .as_ref()
            .is_err_and(|error| error.code == crate::RuntimeErrorCode::TurnWaitHandedOver)
        {
            self.record_wait_handed_over();
        }
        result
    }
    pub async fn call_tool_aggregate(&self, request: ToolAggregateRequest) -> ToolAggregateOutcome {
        let consumer = request.consumer;
        let cursor = match self.admit_tool_run_aggregate(request).await {
            Ok(cursor) => cursor,
            Err(error) => return self.aggregate_host_control(error),
        };
        match self.await_tool_run_aggregate(&cursor, consumer).await {
            Ok(ToolRunAggregatePoll::Ready { outcome, .. }) => outcome,
            Ok(ToolRunAggregatePoll::Pending) => {
                unreachable!("the combined entry waits for its result")
            }
            Err(error) => self.aggregate_host_control(error),
        }
    }
}

impl RuntimeExecutionContext<'_> {
    /// An aggregate's infrastructure failure, answered on the host-control
    /// channel (ADR 0099 §10 L3). A live controller error recorded nothing
    /// durable, so it is also recorded as the enclosing execution's nested
    /// effect error: the cell or segment aborts the way a crash does and is
    /// redriven, rather than committing an outcome whose aggregate never
    /// answered (FIG-3528's rule for the batch surface). A journaled error is a
    /// recorded terminal replaying, and answers on the channel alone.
    fn aggregate_host_control(
        &self,
        error: crate::RuntimeEffectControllerError,
    ) -> ToolAggregateOutcome {
        if let Some(exceeded) = error.tool_call_limit_exceeded() {
            return ToolAggregateOutcome::ToolCallLimitExceeded(exceeded);
        }
        if !error.journaled {
            self.record_nested_effect_error(error.clone());
        }
        ToolAggregateOutcome::HostControl(error.to_string())
    }
}
