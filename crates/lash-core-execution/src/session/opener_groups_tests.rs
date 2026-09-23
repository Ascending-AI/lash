//! The per-opener retained-work bound (ADR 0099 §9).

use super::*;

fn bound(children: usize) -> OpenerWorkBound {
    OpenerWorkBound::new(std::num::NonZeroUsize::new(children).expect("a nonzero bound"))
}

fn opener(children: usize) -> RuntimeExecutionContext<'static> {
    crate::testing::code_execution_context().with_opener_state(OpenerState::new(bound(children)))
}

/// Unique children are reserved at formation; a reservation is reused, not
/// counted twice, and a fresh group that does not fit is refused whole with
/// the typed code.
#[tokio::test]
async fn a_group_past_the_bound_is_refused_whole_and_a_reservation_is_reused() {
    let context = opener(4);
    context
        .reserve_group_work("g1", 3)
        .await
        .expect("three of four fit");
    context
        .reserve_group_work("g1", 3)
        .await
        .expect("a replayed formation reuses its reservation");
    let refused = context
        .reserve_group_work("g2", 2)
        .await
        .expect_err("five of four do not fit");
    assert_eq!(
        refused.code,
        crate::RuntimeErrorCode::EffectGroupOpenerBoundExceeded
    );
    context
        .reserve_group_work("g3", 1)
        .await
        .expect("exactly four fit");
}

/// Releasing a group the opener no longer depends on frees its units.
#[tokio::test]
async fn releasing_a_group_frees_its_units() {
    let context = opener(2);
    context.reserve_group_work("g1", 2).await.expect("fits");
    assert!(context.reserve_group_work("g2", 1).await.is_err());
    context.release_group_work("g1");
    context
        .reserve_group_work("g2", 2)
        .await
        .expect("the released units are available again");
}

/// A successor segment is the same opener: the groups it reattaches keep the
/// units their predecessor reserved.
#[tokio::test]
async fn reattached_groups_keep_their_reservation() {
    let predecessor = opener(4);
    predecessor.reserve_group_work("g1", 3).await.expect("fits");
    predecessor.retain_outstanding_group(
        crate::EffectGroupHandle::restored("g1", 3, 1).expect("a valid cursor"),
    );
    let handover = predecessor.outstanding_groups_snapshot();

    let successor = opener(4);
    successor.restore_outstanding_groups(handover);
    assert!(
        successor.reserve_group_work("g2", 2).await.is_err(),
        "the reattached group still holds three of four units"
    );
    successor
        .reserve_group_work("g2", 1)
        .await
        .expect("the remaining unit is available");
}
