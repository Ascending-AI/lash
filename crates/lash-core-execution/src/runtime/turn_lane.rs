//! The session's turn lane: which item it admits next and the run that
//! item opens (ADR 0101 §4, §5). The session mail drain admits in this
//! order.

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
