//! Settlement races: siblings settling together, and a close racing its own
//! children. Each is a stress on the one rank allocator every host owes. A
//! child of [`effect_group_host`](super), whose helpers these laws share.

use super::*;
use futures_util::StreamExt;
use pretty_assertions::assert_eq;

/// Siblings that settle in the same instant each take their own sequence: no
/// two children share a rank, and delivering by rank walks the sequences in
/// order.
///
/// Written at a width where the settlements really do land together, behind a
/// barrier every child reaches before any is released. A read-then-max rank
/// allocator passes [`every_child_is_delivered_once_in_rank_order`], whose
/// children settle one at a time, and fails here (ADR 0065).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn siblings_settling_together_get_distinct_sequences<F: Fn() -> Host>(
    make: &F,
    prefix: &str,
) {
    let host = make();
    let scoped = host
        .scoped(admit(scope(prefix, "together")))
        .expect("a scope binds");
    let key = group_key(prefix, "together");
    let width = 16;
    let start = Arc::new(tokio::sync::Barrier::new(width));
    let executors = (0..width)
        .map(|position| {
            let start = Arc::clone(&start);
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                start.wait().await;
                Ok(outcome_of(position))
            })
        })
        .collect::<Vec<_>>();
    let mut handle = open(&scoped, &key, width, GroupWakePolicy::All, RUN, executors).await;

    let mut sequences = Vec::new();
    let mut positions = Vec::new();
    while !handle.is_exhausted() {
        let settlement = next(&scoped, &mut handle)
            .await
            .expect("every child settles");
        sequences.push(settlement.sequence);
        positions.push(settlement.position);
    }
    assert_eq!(sequences.len(), width);
    assert!(
        sequences.windows(2).all(|pair| pair[0] < pair[1]),
        "siblings settling together must still yield strictly increasing \
         sequences, not {sequences:?}"
    );
    positions.sort_unstable();
    positions.dedup();
    assert_eq!(
        positions,
        (0..width).collect::<Vec<_>>(),
        "each child settles exactly once, so no position is delivered twice"
    );
    close(&scoped, handle, RUN).await.expect("the group closes");
}

/// A close racing its own children's settlements seats exactly one terminal
/// per child.
///
/// `close(Cancel)` writes a cancellation terminal for every child that has not
/// settled *yet*, and a child already inside its executor can come back with a
/// real outcome a moment later. A host that lets that child take a rank as
/// well reports more terminals than children: the defect a durable host
/// commits by writing a settlement row twice. The window is narrow, so the law
/// is a stress: eight children released together, the close aimed at the
/// moment they settle, over several iterations. The ranks are read back
/// through a second host instance, so what is asserted is what the substrate
/// recorded.
///
/// The iterations are independent — distinct keys, barriers and hosts — so
/// the law overlaps them instead of paying each one's serial substrate calls
/// in turn; the race inside each group is unchanged and the overlap only
/// presses the substrate harder. On a live engine the serial form runs past
/// half the suite's per-law bound, so a slow runner can push it over while
/// every assertion still holds.
pub async fn a_close_racing_its_children_seats_one_terminal_per_child<F: Fn() -> Host>(
    make: &F,
    prefix: &str,
) {
    // Every host an iteration touches is minted before the first group opens:
    // a factory whose mint mutates shared endpoint state — the Restate legs
    // re-install the suite resolver on each mint — must not interleave with
    // another iteration's in-flight children.
    let hosts = (0..CLOSE_RACE_ITERATIONS)
        .map(|_| (make(), make()))
        .collect::<Vec<_>>();
    futures_util::stream::iter(0..CLOSE_RACE_ITERATIONS)
        .for_each_concurrent(CLOSE_RACE_CONCURRENCY, |iteration| {
            let (writer, reader) = &hosts[iteration];
            close_race_iteration(writer, reader, prefix, iteration)
        })
        .await;
}

/// The race is played this many times so the close lands at a different point
/// of the settlement burst across iterations.
const CLOSE_RACE_ITERATIONS: usize = 24;
/// How many iterations run at once: bounded so a shard's substrate sees a
/// wave of racing groups rather than every iteration at once, while the law's
/// wall time stays a small fraction of the suite's per-law bound.
const CLOSE_RACE_CONCURRENCY: usize = 6;

/// One close race: `writer` opens the group and closes it across its
/// children's settlement burst; `reader` reads the recorded ranks back.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn close_race_iteration(writer: &Host, reader: &Host, prefix: &str, iteration: usize) {
    let width = 8;
    let label = format!("race-close-{iteration}");
    let scoped = writer
        .scoped(admit(scope(prefix, &label)))
        .expect("a scope binds");
    let key = group_key(prefix, &label);
    // Every child waits at the barrier and is released together, so the
    // settlements land in the same instant the caller closes.
    let start = Arc::new(tokio::sync::Barrier::new(width));
    let executors = (0..width)
        .map(|position| {
            let start = Arc::clone(&start);
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                start.wait().await;
                Ok(outcome_of(position))
            })
        })
        .collect::<Vec<_>>();
    let handle = open(
        &scoped,
        &key,
        width,
        GroupWakePolicy::All,
        LoserPolicy::Cancel,
        executors,
    )
    .await;
    // A varying number of yields moves the close across the children's
    // settlement from one iteration to the next.
    for _ in 0..(iteration % 6) {
        tokio::task::yield_now().await;
    }
    close(&scoped, handle, LoserPolicy::Cancel)
        .await
        .expect("the caller closes under the declared disposition");

    let recorded = read_back_ranks(
        reader,
        prefix,
        &label,
        &key,
        width,
        GroupWakePolicy::All,
        LoserPolicy::Cancel,
    )
    .await;
    let sequences = recorded
        .iter()
        .map(|settlement| settlement.sequence)
        .collect::<Vec<_>>();
    assert_eq!(
        sequences,
        (1..=width as u64).collect::<Vec<u64>>(),
        "a group of {width} children holds {width} terminals at distinct,              ordered sequences (iteration {iteration})"
    );
    let mut positions = recorded
        .iter()
        .map(|settlement| settlement.position)
        .collect::<Vec<_>>();
    positions.sort_unstable();
    assert_eq!(
        positions,
        (0..width).collect::<Vec<_>>(),
        "a child that settled inside the cancellation window must not also              take a cancellation rank (iteration {iteration})"
    );
    for settlement in &recorded {
        match &settlement.outcome {
            Ok(outcome) => assert!(
                matches!(
                    outcome,
                    RuntimeEffectOutcome::LanguageRuntimeValue { value }
                        if value == &serde_json::json!({ "position": settlement.position })
                ),
                "a settled child's terminal is its own outcome: {outcome:?}"
            ),
            Err(error) => assert_eq!(
                error.code,
                crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelled,
                "an unsettled child's terminal is its cancellation"
            ),
        }
    }
}
