//! A logical Run's aggregates in memory: the calls one invocation owns, the
//! aggregates its program forms over them, and the consumers that take
//! their answers.
//!
//! Every call runs to its end on its own, beside the program; a consumer
//! answers from the order in which leaves settled. Leaves settled when the
//! aggregate forms (a refused call, a call a before-check decided, a plain
//! operand) form a prefix in source order ahead of every dispatched
//! settlement. A loser keeps running and is still owned by the Run, which
//! drains it when it closes.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use tokio_util::sync::CancellationToken;

use super::{CallEnd, run_call};
use crate::tool_dispatch::singleton_run::{
    SingletonRunError, SingletonToolCall, SingletonToolHandlers,
};
use crate::tool_run::CallDecision;
use crate::{RuntimeEffectControllerError, ToolCallId};

/// How a consumer takes an aggregate's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Consumer {
    /// The first rejection, or every fulfilment.
    All,
    /// Every settlement.
    AllSettled,
    /// The first settlement.
    Race,
    /// The first fulfilment, or every rejection.
    Any,
}

/// One leaf of an aggregate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Leaf {
    /// A call the Run owns.
    Call(ToolCallId),
    /// A timer due `duration_ms` after the aggregate formed.
    Timer {
        /// Its duration.
        duration_ms: u64,
    },
    /// A leaf settled before the aggregate formed.
    Settled {
        /// Whether it was fulfilled.
        fulfilled: bool,
    },
}

/// An aggregate's answer, by operand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The operand that decided it.
    Selected(usize),
    /// Every operand settled: `all`, `allSettled`.
    All,
    /// Every operand rejected: `any`.
    Exhausted,
    /// A call's end is host control, never an operand's rejection: the Run
    /// cancelled it, or a check aborted the Run.
    HostControl {
        /// The call.
        call_id: ToolCallId,
        /// How it ended.
        decision: CallDecision,
    },
}

#[derive(Clone, Copy, Debug)]
struct Settlement {
    fulfilled: bool,
    /// Immediate leaves precede every dispatched settlement, in source
    /// order; dispatched ones follow by when they settled, and in the order
    /// the Run saw them at one instant.
    order: (bool, u64, u64),
}

struct Aggregate {
    leaves: Vec<Leaf>,
    /// The leaf each operand reads, in source order.
    operands: Vec<usize>,
    formed_at_ms: u64,
}

/// A call's handlers and its cancel policy.
type CallOwner<'a> = (
    Arc<dyn SingletonToolHandlers + 'a>,
    crate::tool_run::ExternalCancelPolicy,
);

/// A call's end, and when it settled: the clock's instant and the Run's
/// count.
type Ended = (Result<CallEnd, String>, (u64, u64));

type Running<'a> = Pin<Box<dyn Future<Output = (ToolCallId, Result<CallEnd, String>)> + Send + 'a>>;

/// The Run of one invocation: its calls and aggregates, in memory.
pub struct ToolRun<'a> {
    scope: crate::ExecutionScope,
    clock: Arc<dyn crate::Clock>,
    cancel: CancellationToken,
    /// Every call admitted, with its end and when it settled (the clock's
    /// instant and the Run's count) once it ended.
    calls: BTreeMap<ToolCallId, Option<Ended>>,
    /// Each call's handlers and cancel policy, for its discharge at close.
    owners: BTreeMap<ToolCallId, CallOwner<'a>>,
    /// Calls a before-check or refusal settled as the aggregate formed.
    immediate: std::collections::BTreeSet<ToolCallId>,
    aggregates: BTreeMap<String, Aggregate>,
    running: FuturesUnordered<Running<'a>>,
    settled: u64,
    /// The rounds a process holds at once: each holds all its calls until
    /// every one of them ended.
    held: Vec<Vec<ToolCallId>>,
    /// The calls each cell made, for the cell's whole life.
    cells: BTreeMap<String, usize>,
}

/// The Run's control over a consumer: `call_id` ended `decision`, the Run's
/// cancel or a check's AbortRun, which the consumer raises on its host
/// channel and never takes as an operand's rejection.
pub(crate) fn run_control(
    call_id: ToolCallId,
    decision: &CallDecision,
    cause: Option<Box<crate::tool_run::HookCause>>,
) -> SingletonRunError {
    let mut error = RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::RuntimeToolRunAwaitCancelled,
        format!("tool {call_id}: {decision:?}"),
    );
    error.cause = Some(crate::RuntimeErrorCause::ToolRunControl {
        cause,
        call_id: Box::new(call_id),
        aborted: *decision == CallDecision::Aborted,
    });
    error.into()
}

fn run_fault(message: impl Into<String>) -> SingletonRunError {
    RuntimeEffectControllerError::new(crate::RuntimeErrorCode::RuntimeToolRunShape, message).into()
}

impl<'a> ToolRun<'a> {
    /// An empty Run in `scope`, timing on `clock`.
    #[must_use]
    pub fn new(scope: crate::ExecutionScope, clock: Arc<dyn crate::Clock>) -> Self {
        Self {
            scope,
            clock,
            cancel: CancellationToken::new(),
            calls: BTreeMap::new(),
            owners: BTreeMap::new(),
            immediate: std::collections::BTreeSet::new(),
            aggregates: BTreeMap::new(),
            running: FuturesUnordered::new(),
            settled: 0,
            held: Vec::new(),
            cells: BTreeMap::new(),
        }
    }

    /// Whether the Run already owns `call_id`.
    #[must_use]
    pub fn contains_call(&self, call_id: &ToolCallId) -> bool {
        self.calls.contains_key(call_id)
    }

    /// Admit the round of new calls `calls` under `scope`'s capacity, or
    /// refuse it whole past `limit`. A process holds a round whole until
    /// every member ended; a cell counts every call it ever made.
    ///
    /// # Errors
    ///
    /// The typed refusal naming the limit, the count and the request.
    pub fn admit_capacity(
        &mut self,
        scope: &crate::tool_run::CapacityScope,
        calls: Vec<ToolCallId>,
        limit: crate::MaxToolCalls,
    ) -> Result<(), crate::ToolCallLimitExceeded> {
        let requested = calls.len();
        let counted = self.counted(scope);
        if counted.saturating_add(requested) > limit.get() {
            return Err(crate::ToolCallLimitExceeded {
                scope: match scope {
                    crate::tool_run::CapacityScope::Held => crate::ToolCallLimitScope::Process,
                    crate::tool_run::CapacityScope::Cell { .. } => crate::ToolCallLimitScope::Cell,
                },
                limit,
                counted,
                requested,
            });
        }
        match scope {
            crate::tool_run::CapacityScope::Held => self.held.push(calls),
            crate::tool_run::CapacityScope::Cell { key } => {
                *self.cells.entry(key.clone()).or_default() += requested;
            }
        }
        Ok(())
    }

    /// The calls `scope` holds now: a held round counts whole while any of
    /// its members runs.
    #[must_use]
    pub fn counted(&self, scope: &crate::tool_run::CapacityScope) -> usize {
        match scope {
            crate::tool_run::CapacityScope::Held => self
                .held
                .iter()
                .filter(|round| {
                    round
                        .iter()
                        .any(|call_id| matches!(self.calls.get(call_id), Some(None)))
                })
                .map(Vec::len)
                .sum(),
            crate::tool_run::CapacityScope::Cell { key } => {
                self.cells.get(key).copied().unwrap_or(0)
            }
        }
    }

    /// Start `call` on its own, under `handlers`: it runs to its end beside
    /// the program, whichever aggregates read it.
    ///
    /// # Errors
    ///
    /// A call the Run already owns.
    pub fn start(
        &mut self,
        call: SingletonToolCall,
        handlers: Arc<dyn SingletonToolHandlers + 'a>,
    ) -> Result<(), SingletonRunError> {
        self.refuse_after_abort()?;
        let call_id = call.call_id.clone();
        match self.calls.entry(call_id.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(None);
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                return Err(run_fault(format!("call {call_id} is admitted twice")));
            }
        }
        self.owners
            .insert(call_id.clone(), (Arc::clone(&handlers), call.cancel));
        let scope = self.scope.clone();
        let clock = Arc::clone(&self.clock);
        let cancel = self.cancel.clone();
        self.running.push(Box::pin(async move {
            let end = run_call(handlers, call, &scope, clock.as_ref(), &cancel)
                .await
                .map_err(|error| error.to_string());
            (call_id, end)
        }));
        Ok(())
    }

    /// Form the aggregate `key` over `leaves`, read by `operands` in source
    /// order. Its calls are ones the Run started.
    ///
    /// # Errors
    ///
    /// A mapping refusal: an unknown call, an operand past the leaves, or a
    /// key formed twice.
    pub fn form(
        &mut self,
        key: String,
        leaves: Vec<Leaf>,
        operands: Vec<usize>,
    ) -> Result<(), SingletonRunError> {
        self.refuse_after_abort()?;
        if self.aggregates.contains_key(&key) || operands.iter().any(|leaf| *leaf >= leaves.len()) {
            return Err(run_fault(format!(
                "aggregate {key} does not map its leaves"
            )));
        }
        if let Some(unknown) = leaves.iter().find_map(|leaf| match leaf {
            Leaf::Call(call_id) if !self.calls.contains_key(call_id) => Some(call_id),
            _ => None,
        }) {
            return Err(run_fault(format!(
                "aggregate {key} names unknown call {unknown}"
            )));
        }
        self.aggregates.insert(
            key,
            Aggregate {
                leaves,
                operands,
                formed_at_ms: self.clock.timestamp_ms(),
            },
        );
        Ok(())
    }

    /// Mark `call_id` as settled when its aggregate formed: a before-check
    /// decided it, so it sits in the immediate prefix.
    pub fn settled_at_formation(&mut self, call_id: ToolCallId) {
        self.immediate.insert(call_id);
    }

    /// Progress the Run's calls until one ends, and record its end. Never
    /// completes while no call runs.
    pub async fn next_end(&mut self) {
        match self.running.next().await {
            Some((call_id, end)) => {
                self.settled += 1;
                let at = (self.clock.timestamp_ms(), self.settled);
                self.calls.insert(call_id, Some((end, at)));
            }
            None => std::future::pending().await,
        }
    }

    /// Await `work` while the Run's calls progress beside it. Whatever the
    /// Run's owner awaits (a store write, an observation) never stops a call
    /// mid-transaction, so no call holds the store's connections while its
    /// owner waits on one (FIG-5237).
    pub async fn alongside<F: Future>(&mut self, work: F) -> F::Output {
        let mut work = std::pin::pin!(work);
        loop {
            tokio::select! {
                output = &mut work => return output,
                () = self.next_end() => {}
            }
        }
    }

    /// The end of `call_id`, once it ended.
    ///
    /// # Errors
    ///
    /// The call's fault, which faults its consumer.
    pub fn end(&self, call_id: &ToolCallId) -> Result<Option<&CallEnd>, SingletonRunError> {
        match self.calls.get(call_id) {
            Some(Some((Ok(end), _))) => Ok(Some(end)),
            Some(Some((Err(message), _))) => Err(run_fault(message.clone())),
            Some(None) => Ok(None),
            None => Err(run_fault(format!("call {call_id} is not this Run's"))),
        }
    }

    fn settlements(
        &self,
        aggregate: &Aggregate,
        host_control: bool,
    ) -> Result<(Vec<Option<Settlement>>, Option<Answer>), SingletonRunError> {
        let now = self.clock.timestamp_ms();
        let mut settlements = vec![None; aggregate.leaves.len()];
        for (index, leaf) in aggregate.leaves.iter().enumerate() {
            let position = aggregate
                .operands
                .iter()
                .position(|operand| *operand == index)
                .unwrap_or(index) as u64;
            settlements[index] = match leaf {
                Leaf::Settled { fulfilled } => Some(Settlement {
                    fulfilled: *fulfilled,
                    order: (false, position, 0),
                }),
                Leaf::Timer { duration_ms } => {
                    let due = aggregate.formed_at_ms.saturating_add(*duration_ms);
                    (due <= now).then_some(Settlement {
                        fulfilled: true,
                        order: (true, due, u64::MAX),
                    })
                }
                Leaf::Call(call_id) => match self.calls.get(call_id) {
                    Some(Some((end, settled))) => {
                        let end = end.as_ref().map_err(|message| run_fault(message.clone()))?;
                        if let CallEnd::Withheld { decision, .. } = end
                            && host_control
                            && matches!(decision, CallDecision::Cancelled | CallDecision::Aborted)
                        {
                            return Ok((
                                settlements,
                                Some(Answer::HostControl {
                                    call_id: call_id.clone(),
                                    decision: decision.clone(),
                                }),
                            ));
                        }
                        Some(Settlement {
                            fulfilled: end.fulfilled(),
                            order: if self.immediate.contains(call_id) {
                                (false, position, 0)
                            } else {
                                (true, settled.0, settled.1)
                            },
                        })
                    }
                    _ => None,
                },
            };
        }
        Ok((settlements, None))
    }

    /// The answer `consumer` takes from aggregate `key` now, if it has one.
    ///
    /// # Errors
    ///
    /// An unknown aggregate, or a call's fault.
    pub fn answer(
        &self,
        key: &str,
        consumer: Consumer,
        host_control: bool,
    ) -> Result<Option<Answer>, SingletonRunError> {
        let aggregate = self.aggregate(key)?;
        let (settlements, control) = self.settlements(aggregate, host_control)?;
        if control.is_some() {
            return Ok(control);
        }
        let decides = |settled: &Settlement| match consumer {
            Consumer::Race => true,
            Consumer::Any => settled.fulfilled,
            Consumer::All => !settled.fulfilled,
            Consumer::AllSettled => false,
        };
        let selected = aggregate
            .operands
            .iter()
            .enumerate()
            .filter_map(|(position, leaf)| {
                let settled = settlements[*leaf].as_ref()?;
                decides(settled).then_some((settled.order, position))
            })
            .min();
        if let Some((_, position)) = selected {
            return Ok(Some(Answer::Selected(position)));
        }
        if !aggregate.operands.is_empty() && settlements.iter().all(Option::is_some) {
            return Ok(match consumer {
                Consumer::Race => None,
                Consumer::Any => Some(Answer::Exhausted),
                Consumer::All | Consumer::AllSettled => Some(Answer::All),
            });
        }
        Ok(None)
    }

    /// The operands of `key` in the order they settled.
    ///
    /// # Errors
    ///
    /// An unknown aggregate, or a call's fault.
    pub fn settlement_order(&self, key: &str) -> Result<Vec<usize>, SingletonRunError> {
        let aggregate = self.aggregate(key)?;
        let (settlements, _) = self.settlements(aggregate, false)?;
        let mut order: Vec<_> = aggregate
            .operands
            .iter()
            .enumerate()
            .filter_map(|(position, leaf)| {
                settlements[*leaf]
                    .as_ref()
                    .map(|settled| (settled.order, position))
            })
            .collect();
        order.sort_unstable();
        Ok(order.into_iter().map(|(_, position)| position).collect())
    }

    /// Wait until `consumer` has its answer from aggregate `key`: calls
    /// progress, and a timer leaf elapses at its due time.
    ///
    /// # Errors
    ///
    /// An unknown aggregate, or a call's fault.
    pub async fn consume(
        &mut self,
        key: &str,
        consumer: Consumer,
        host_control: bool,
    ) -> Result<Answer, SingletonRunError> {
        loop {
            if let Some(answer) = self.answer(key, consumer, host_control)? {
                return Ok(answer);
            }
            let aggregate = self.aggregate(key)?;
            let now = self.clock.timestamp_ms();
            let next_timer = aggregate
                .leaves
                .iter()
                .filter_map(|leaf| match leaf {
                    Leaf::Timer { duration_ms } => {
                        Some(aggregate.formed_at_ms.saturating_add(*duration_ms))
                    }
                    _ => None,
                })
                .filter(|due| *due > now)
                .min();
            if self.running.is_empty() && next_timer.is_none() {
                return Err(run_fault(format!(
                    "aggregate {key} can never answer its consumer"
                )));
            }
            let clock = Arc::clone(&self.clock);
            let sleep = async move {
                match next_timer {
                    Some(due) => {
                        clock
                            .sleep(std::time::Duration::from_millis(due.saturating_sub(now)))
                            .await;
                    }
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = self.next_end() => {}
                () = sleep => {}
            }
        }
    }

    /// The leaf operand `operand` of aggregate `key` reads.
    ///
    /// # Errors
    ///
    /// An unknown aggregate.
    pub fn leaf(&self, key: &str, operand: usize) -> Result<Option<&Leaf>, SingletonRunError> {
        let aggregate = self.aggregate(key)?;
        Ok(aggregate
            .operands
            .get(operand)
            .and_then(|leaf| aggregate.leaves.get(*leaf)))
    }

    /// A Run a check aborted admits nothing more.
    fn refuse_after_abort(&self) -> Result<(), SingletonRunError> {
        let aborted = self.calls.iter().find_map(|(call_id, end)| match end {
            Some((
                Ok(CallEnd::Withheld {
                    decision: CallDecision::Aborted,
                    cause,
                }),
                _,
            )) => Some((
                call_id.clone(),
                cause.as_ref().map(|cause| Box::new(cause.verdict.clone())),
            )),
            _ => None,
        });
        match aborted {
            Some((call_id, cause)) => Err(run_control(call_id, &CallDecision::Aborted, cause)),
            None => Ok(()),
        }
    }

    fn aggregate(&self, key: &str) -> Result<&Aggregate, SingletonRunError> {
        self.aggregates
            .get(key)
            .ok_or_else(|| run_fault(format!("aggregate {key} was never formed")))
    }

    /// End the Run: its cancel fires, so every unfinished call is decided
    /// `Cancelled` at its next decision, and every call is driven to its end.
    ///
    /// # Errors
    ///
    /// A call's fault.
    pub async fn close(&mut self) -> Result<(), SingletonRunError> {
        self.cancel.cancel();
        for (call_id, (handlers, policy)) in &self.owners {
            if matches!(self.calls.get(call_id), Some(None))
                && *policy == crate::tool_run::ExternalCancelPolicy::CancelExternalWork
            {
                handlers.cancel_call(call_id).await.map_err(run_fault)?;
            }
        }
        while !self.running.is_empty() {
            self.next_end().await;
        }
        for end in self.calls.values() {
            if let Some((Err(message), _)) = end {
                return Err(run_fault(message.clone()));
            }
        }
        Ok(())
    }
}
