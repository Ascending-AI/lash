//! The session's turn lane: which item it admits next and the run that
//! item opens (ADR 0101 §4, §5). The session mail drain admits in this
//! order.

/// What the session's turn lane admits next once no session command is open
/// (ADR 0101 §5): the host input and
/// the queued work pending in the two admission tables take one per-session
/// `enqueue_seq`, and the earlier of the head next-turn input and the
/// earliest pending queued turn work goes first. There is no kind priority.
#[derive(Clone, Copy, Debug)]
pub enum TurnLaneHead<'a> {
    /// The head of the accepted next-turn input.
    Input(&'a crate::PendingTurnInputRead),
    /// The earliest pending queued turn work, which heads a queued run.
    Queued(&'a crate::QueuedWorkBatch),
}

/// The turn lane's next item among the session's `open` inputs and pending
/// `queued` batches (see [`TurnLaneHead`]); `None` when both are empty. Only
/// turn work heads the lane: session commands drain before it (ADR 0101 §4).
#[must_use]
pub fn turn_lane_head<'a>(
    open: &'a [crate::PendingTurnInputRead],
    queued: &'a [crate::QueuedWorkBatch],
) -> Option<TurnLaneHead<'a>> {
    let earliest_queued = queued
        .iter()
        .filter(|batch| batch.work_class() == crate::store::QueuedWorkClass::TurnWork)
        .min_by_key(|batch| batch.enqueue_seq);
    match (head_input(open), earliest_queued) {
        (Some(head), Some(queued)) if queued.enqueue_seq < head.input.enqueue_seq => {
            Some(TurnLaneHead::Queued(queued))
        }
        (Some(head), _) => Some(TurnLaneHead::Input(head)),
        (None, Some(queued)) => Some(TurnLaneHead::Queued(queued)),
        (None, None) => None,
    }
}

/// The head of the session's accepted next-turn input, among its `open`
/// inputs at idle: the oldest one. With no turn running, every open input is
/// next-turn input, whatever turn its submitted delivery addresses (ADR 0101
/// §5.1).
#[must_use]
pub fn head_input(open: &[crate::PendingTurnInputRead]) -> Option<&crate::PendingTurnInputRead> {
    open.iter()
        .filter(|read| read.input.state.is_next_turn_input(None))
        .min_by_key(|read| read.input.enqueue_seq)
}

/// The run the head input `head` runs under, given the run its store
/// binding names (`bound`): that run (the run whose admission took it, or the
/// new run a fork bound it to, FIG-3600 S7), else [`input_run`].
#[must_use]
pub fn head_input_run(
    head: &crate::PendingTurnInputRead,
    bound: Option<crate::TurnId>,
) -> crate::TurnId {
    bound.unwrap_or_else(|| input_run(&head.input))
}

/// The run of a shift that starts with `input`: the host's id for it (its
/// source key) when it has one, else its input id (FIG-3600, ruling Q4).
#[must_use]
pub fn input_run(input: &crate::PendingTurnInput) -> crate::TurnId {
    input
        .source_key
        .as_deref()
        .and_then(|source_key| crate::TurnId::parse(source_key).ok())
        .unwrap_or_else(|| crate::TurnId::from(&input.input_id))
}
