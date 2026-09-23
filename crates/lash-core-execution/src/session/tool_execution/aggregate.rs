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

use super::batch::{PreparedToolLeafEntry, ToolLeafPreparation};
use super::group::{GroupChildSettled, PreparedGroupChild, ToolAggregateConsumer};

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
    /// The site that formed the aggregate — a language runtime passes the
    /// aggregate's instruction; host code with no program passes `0`. With
    /// the occurrence it names an aggregate whose leaves carry no identity of
    /// their own: timers (§11 clause 4).
    pub site: u64,
    pub occurrence: crate::session::ToolGroupOccurrence,
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
}

/// Where the immediate prefix decided the aggregate, if it did.
enum PrefixDecision {
    Leaf(usize),
    Value,
}

impl RuntimeExecutionContext<'_> {
    /// Forms one aggregate as a durable effect group and consumes it under
    /// its consumer mode (ADR 0099 §10, §11). See the module documentation.
    pub async fn call_tool_aggregate(&self, request: ToolAggregateRequest) -> ToolAggregateOutcome {
        let ToolAggregateRequest {
            leaves,
            consumer,
            settled_value_after,
            site,
            occurrence,
        } = request;
        let leaf_count = leaves.len();
        let calls = leaves
            .iter()
            .filter_map(|leaf| match leaf {
                ToolAggregateLeaf::Tool(call) => Some(call.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let timer_identities = leaves
            .iter()
            .enumerate()
            .filter_map(|(position, leaf)| match leaf {
                ToolAggregateLeaf::Timer { duration_ms } => Some((position, *duration_ms)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let batch_id = aggregate_batch_id(&calls, occurrence, site, &timer_identities);

        // Prepare in leaf order. A call settled during preparation joins the
        // immediate prefix with the caller's own settled leaves.
        let mut replies: Vec<Option<ToolAggregateLeafReply>> =
            (0..leaf_count).map(|_| None).collect();
        let mut prefix: Vec<Option<bool>> = vec![None; leaf_count];
        let mut entries: Vec<PreparedToolLeafEntry> = Vec::new();
        let mut timers: Vec<(usize, u64)> = Vec::new();
        for (index, leaf) in leaves.into_iter().enumerate() {
            match leaf {
                ToolAggregateLeaf::Tool(call) => match self.prepare_tool_leaf(index, call).await {
                    ToolLeafPreparation::Prepared(entry) => entries.push(*entry),
                    ToolLeafPreparation::Completed(reply) => {
                        let reply = ToolAggregateLeafReply::Tool(reply);
                        prefix[index] = Some(reply.fulfilled());
                        replies[index] = Some(reply);
                    }
                },
                ToolAggregateLeaf::Timer { duration_ms } => timers.push((index, duration_ms)),
                ToolAggregateLeaf::Settled { fulfilled } => prefix[index] = Some(fulfilled),
            }
        }
        let decision = prefix_decision(consumer, &prefix, settled_value_after);

        // Admit every pending leaf before the prefix may answer (§11 clause 3).
        let mut children: Vec<(usize, PreparedGroupChild)> = Vec::new();
        if !timers.is_empty() {
            // The timers' start point is this admission: one journaled clock
            // sample fixes every timer's deadline, so a replay forms the same
            // group and a recovery never starts a fresh duration (§11 clause 4).
            let sample = match self
                .journaled_language_runtime_value(
                    format!("{}:timers-admitted", self.tool_child_group_key(&batch_id)),
                    "now".to_string(),
                )
                .await
            {
                Ok(sample) => sample,
                Err(error) => return self.aggregate_host_control(error),
            };
            let Some(admitted_at) = sample.as_u64() else {
                return self.aggregate_host_control(crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
                    format!("the aggregate's admission clock sample {sample} is not a timestamp"),
                ));
            };
            for (index, duration_ms) in timers {
                children.push((
                    index,
                    PreparedGroupChild::Timer {
                        deadline_ms: admitted_at.saturating_add(duration_ms),
                    },
                ));
            }
        }
        children.extend(
            self.tool_child_leaves(&batch_id, entries)
                .into_iter()
                .map(|leaf| (leaf.input_index, PreparedGroupChild::Tool(Box::new(leaf)))),
        );
        // Group positions follow leaf order, so a replay admits every child at
        // the position it had.
        children.sort_by_key(|(index, _)| *index);
        let (child_leaves, children): (Vec<usize>, Vec<PreparedGroupChild>) =
            children.into_iter().unzip();

        let settled = if children.is_empty() {
            None
        } else {
            let group_invocation = self.tool_batch_invocation(&batch_id);
            let handle = match self
                .open_tool_child_group(group_invocation, &batch_id, &children, consumer.wake())
                .await
            {
                Ok(handle) => handle,
                Err(error) => return self.aggregate_host_control(error),
            };
            if decision.is_some() {
                // The prefix answers; the admitted children are losers from
                // the moment they are admitted, and the opener holds them.
                self.retain_outstanding_group(handle);
                None
            } else {
                match self
                    .consume_tool_child_group(handle, &children, consumer)
                    .await
                {
                    Ok(settled) => Some(settled),
                    Err(error) => return self.aggregate_host_control(error),
                }
            }
        };

        match decision {
            Some(PrefixDecision::Leaf(leaf)) => {
                return ToolAggregateOutcome::Selected {
                    leaf,
                    reply: replies[leaf].take(),
                };
            }
            Some(PrefixDecision::Value) => return ToolAggregateOutcome::SettledValue,
            None => {}
        }
        if let Some(mut settled) = settled {
            if settled.cancelled {
                return ToolAggregateOutcome::HostControl(
                    "the aggregate's await was cancelled with its turn".to_string(),
                );
            }
            for (position, leaf) in child_leaves.iter().enumerate() {
                if let Some(child) = settled.settled[position].take() {
                    replies[*leaf] = Some(match child {
                        GroupChildSettled::Tool(completed) => {
                            let completed = *completed;
                            ToolAggregateLeafReply::Tool(Box::new(
                                ToolInvocationReply::from_output(completed.completed.output)
                                    .with_record(completed.record),
                            ))
                        }
                        GroupChildSettled::Timer => ToolAggregateLeafReply::Timer,
                    });
                }
            }
            if let Some(position) = settled.decided {
                let leaf = child_leaves[position];
                return ToolAggregateOutcome::Selected {
                    leaf,
                    reply: replies[leaf].take(),
                };
            }
        }
        match consumer {
            ToolAggregateConsumer::Any => ToolAggregateOutcome::ExhaustedRejections(replies),
            ToolAggregateConsumer::All => {
                // Exhausted with no dispatched rejection: a prefix rejection
                // would have decided, so every leaf fulfilled.
                ToolAggregateOutcome::AllResults(replies)
            }
            ToolAggregateConsumer::AllSettled | ToolAggregateConsumer::Race => {
                ToolAggregateOutcome::AllResults(replies)
            }
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
        if !error.journaled {
            self.record_nested_effect_error(error.clone());
        }
        ToolAggregateOutcome::HostControl(error.to_string())
    }
}

/// One aggregate's group identity.
///
/// A tool call carries its own identity — the bridge mints it from the call's
/// site — so a tool-only aggregate keeps the batch identity the tool path has
/// always had. A timer has none: two timer aggregates reached once at two
/// sites would otherwise share one group, and the second await would reopen
/// the first — answering from its recorded settlement, or failing the reopen
/// fence when the durations differ (§11 clause 4). So an aggregate that holds
/// timers folds every timer's position and duration, and the site that formed
/// it, into its identity.
fn aggregate_batch_id(
    calls: &[ToolInvocation],
    occurrence: crate::session::ToolGroupOccurrence,
    site: u64,
    timers: &[(usize, u64)],
) -> String {
    if timers.is_empty() {
        return deterministic_tool_invocation_batch_id(calls, occurrence);
    }
    let mut identity =
        crate::stable_identity::IdentityEncoder::new("lash.aggregate-with-timers", 1);
    identity.bytes(&tool_invocation_batch_preimage(calls, occurrence));
    identity.u64(site);
    identity.sequence(timers, |identity, (position, duration_ms)| {
        identity.u64(*position as u64);
        identity.u64(*duration_ms);
    });
    crate::stable_identity::rendered_hash(
        "tool-batch",
        TOOL_BATCH_FAMILY_VERSION,
        &identity.finish(),
    )
}

/// Scans the immediate prefix in source order (ADR 0099 §10 L5): the caller's
/// first plain operand sits after `settled_value_after` leaves, and a leaf
/// settled before the aggregate formed sits at its own position.
fn prefix_decision(
    consumer: ToolAggregateConsumer,
    prefix: &[Option<bool>],
    settled_value_after: Option<usize>,
) -> Option<PrefixDecision> {
    for (index, settled) in prefix.iter().enumerate() {
        if settled_value_after == Some(index) && consumer.decides(true) {
            return Some(PrefixDecision::Value);
        }
        if let Some(fulfilled) = settled
            && consumer.decides(*fulfilled)
        {
            return Some(PrefixDecision::Leaf(index));
        }
    }
    (settled_value_after == Some(prefix.len()) && consumer.decides(true))
        .then_some(PrefixDecision::Value)
}
