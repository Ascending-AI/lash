use std::future::Future;

use futures_util::stream::{FuturesUnordered, StreamExt};

/// Run a batch concurrently, reporting outcomes in input order alongside the
/// order in which they settled.
pub(crate) async fn schedule_tool_batch<T, O, IndexOf, Run, Fut>(
    items: Vec<T>,
    index_of: IndexOf,
    run: Run,
) -> ScheduledToolBatch<O>
where
    T: Send + 'static,
    O: Send + 'static,
    IndexOf: Fn(&T) -> usize,
    Run: Fn(T) -> Fut,
    Fut: Future<Output = O> + Send,
{
    let mut pending = FuturesUnordered::new();
    for item in items {
        let index = index_of(&item);
        let future = run(item);
        pending.push(async move { (index, future.await) });
    }

    let mut outcomes = Vec::new();
    while let Some(outcome) = pending.next().await {
        outcomes.push(outcome);
    }

    // `FuturesUnordered` yields in completion order, which is the only place the
    // settlement order of a batch exists. Record it before sorting the outcomes
    // back into the caller's input order: an aggregate that must reject with the
    // first *settled* rejection cannot recover this afterwards.
    let settlement_order = outcomes.iter().map(|(index, _)| *index).collect();
    outcomes.sort_by_key(|(index, _)| *index);
    ScheduledToolBatch {
        outcomes: outcomes.into_iter().map(|(_, outcome)| outcome).collect(),
        settlement_order,
    }
}

/// A scheduled batch: outcomes in input order, plus the order they settled in.
pub(crate) struct ScheduledToolBatch<O> {
    /// One outcome per input, in input order.
    pub(crate) outcomes: Vec<O>,
    /// Input indices in the order their futures completed.
    pub(crate) settlement_order: Vec<usize>,
}
