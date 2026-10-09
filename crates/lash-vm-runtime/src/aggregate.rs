//! The answer of a Lash VM aggregate (ADR 0099 §10, §11; FIG-3397), a
//! pure function of where its leaves stand.
//!
//! A bridge resolves each of its leaves in its own way — a value it
//! settled before the aggregate formed, an admitted tool call, a timer —
//! and says where each stands now as a [`LeafStanding`]. Leaves settled
//! when the aggregate formed, and the caller's first plain operand, form a
//! source-ordered prefix ahead of every later settlement (§10 L5); later
//! settlements follow in the order the bridge reports. Host control (a call
//! the Run cancelled, or a check that aborted it) is never a leaf's
//! rejection (L3).

use lash_vm::ExecutionHostError;

/// How a consumer takes an aggregate's answer.
pub use lash_vm::AggregateConsumer;

/// Where one leaf of an aggregate stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeafStanding {
    /// Not settled yet.
    Open,
    /// Settled when the aggregate formed: in the immediate prefix, in
    /// source order.
    Immediate {
        /// Whether it was fulfilled.
        fulfilled: bool,
    },
    /// Settled since, at `order` among the aggregate's later settlements.
    Settled {
        /// Whether it was fulfilled.
        fulfilled: bool,
        /// Its place among the later settlements: lower settled first.
        order: (u64, u64),
    },
    /// Ended by host control: never an operand's rejection.
    HostControl,
}

/// What an aggregate answers its consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateAnswer {
    /// The leaf that decided it.
    Leaf(usize),
    /// The caller's plain operand decided a `race` or an `any`.
    SettledValue,
    /// Every leaf settled: `all`, `allSettled`.
    All,
    /// Every leaf rejected: `any`.
    Exhausted,
    /// A leaf ended by host control: the consumer raises it on its host
    /// channel.
    HostControl(usize),
}

/// The answer `consumer` takes from an aggregate whose leaves stand as
/// `leaves`, with the caller's plain operand after `settled_value_after` of
/// them; `None` until it has one.
#[must_use]
pub fn aggregate_answer(
    consumer: AggregateConsumer,
    settled_value_after: Option<usize>,
    leaves: &[LeafStanding],
) -> Option<AggregateAnswer> {
    if let Some(leaf) = leaves
        .iter()
        .position(|standing| *standing == LeafStanding::HostControl)
    {
        return Some(AggregateAnswer::HostControl(leaf));
    }
    // Every operand in source order: the leaves, with the plain operand at
    // its place.
    let mut operands: Vec<(Option<usize>, LeafStanding)> = leaves
        .iter()
        .copied()
        .enumerate()
        .map(|(leaf, standing)| (Some(leaf), standing))
        .collect();
    if let Some(after) = settled_value_after {
        operands.insert(
            after.min(operands.len()),
            (None, LeafStanding::Immediate { fulfilled: true }),
        );
    }
    let decides = |fulfilled: bool| match consumer {
        AggregateConsumer::Race => true,
        AggregateConsumer::Any => fulfilled,
        AggregateConsumer::All => !fulfilled,
        AggregateConsumer::AllSettled => false,
    };
    let selected = operands
        .iter()
        .enumerate()
        .filter_map(|(position, (leaf, standing))| {
            let order = match *standing {
                LeafStanding::Immediate { fulfilled } if decides(fulfilled) => {
                    (false, position as u64, 0)
                }
                LeafStanding::Settled { fulfilled, order } if decides(fulfilled) => {
                    (true, order.0, order.1)
                }
                _ => return None,
            };
            Some((order, *leaf))
        })
        .min();
    if let Some((_, leaf)) = selected {
        return Some(leaf.map_or(AggregateAnswer::SettledValue, AggregateAnswer::Leaf));
    }
    let all_settled = operands
        .iter()
        .all(|(_, standing)| *standing != LeafStanding::Open);
    if operands.is_empty() || !all_settled {
        return None;
    }
    match consumer {
        AggregateConsumer::Race => None,
        AggregateConsumer::Any => Some(AggregateAnswer::Exhausted),
        AggregateConsumer::All | AggregateConsumer::AllSettled => Some(AggregateAnswer::All),
    }
}

/// The tool failure a `max_tool_calls` refusal carries: typed by its class
/// and code, never retried, and worded by the refusal so the limit is named
/// in what the model reads (FIG-4546).
pub fn tool_call_limit_failure(
    exceeded: lash_core::ToolCallLimitExceeded,
) -> lash_core::ToolFailure {
    let mut failure = lash_core::ToolFailure::runtime(
        lash_core::ToolFailureClass::ResourceLimit,
        lash_core::ToolCallLimitExceeded::CODE,
        exceeded.to_string(),
    );
    failure.raw = Some(lash_core::ToolValue::untrusted_json(
        serde_json::json!({ "tool_call_limit": exceeded }),
    ));
    failure
}

/// Whether a VM terminal is the session's `max_tool_calls` refusing an
/// aggregate: the program's failure, carried on the aggregate's error
/// channel with the refusal's typed class and code.
pub fn is_tool_call_limit_failure(error: &lash_vm::RuntimeError) -> bool {
    matches!(
        error,
        lash_vm::RuntimeError::AggregateHostControl { source }
            if source.tool_failure_code() == Some(lash_core::ToolCallLimitExceeded::CODE)
    )
}

/// The message for the VM terminal that ends an execution awaiting an
/// aggregate nothing can settle: the typed
/// [`lash_core::RuntimeErrorCode::AggregateAwaitUnsettled`], the cause, and
/// the guard the empty aggregate needs. The host ends the execution
/// uncatchably (ADR 0099 §11 clause 5), but the defect is the program's and
/// the feedback says so (FIG-4547). `None` for every other failure.
pub fn host_lifetime_failure_message(error: &lash_vm::RuntimeError) -> Option<String> {
    matches!(error, lash_vm::RuntimeError::AggregateAwaitUnsettled { .. }).then(|| {
        lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::AggregateAwaitUnsettled,
            format!("{error}; guard the empty case before awaiting it"),
        )
        .to_string()
    })
}

/// A timer leaf's duration in whole milliseconds: the same numbers and
/// duration strings `await sleep(ms)` accepts. A timer leaf is always relative
/// — its start point is its admission (ADR 0099 §11 clause 4).
pub fn timer_duration_ms(sleep: &lash_vm::Sleep) -> Result<u64, ExecutionHostError> {
    match crate::bridge::process_sleep(sleep.kind, &sleep.value)? {
        lash_core::SleepSpec::For { duration_ms } => Ok(duration_ms),
        lash_core::SleepSpec::Until { .. } => Err(ExecutionHostError::new(
            "an aggregate timer is relative to its admission and cannot name a deadline",
        )),
    }
}
