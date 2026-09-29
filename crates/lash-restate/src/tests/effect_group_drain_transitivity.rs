//! The §5 drain barrier is transitive (ADR 0065 §5, FIG-4088).
//!
//! The index names a drain only its last-committed unseated blocker, and the
//! drain parks on that one sibling's drained wake. That is sound only if the
//! wake resolves when the whole barrier has lifted: that sibling seats after
//! every lower-committed sibling has seated, or the group retires, which lifts
//! every barrier. This law drives the index's own protocol — `commit_child`,
//! `drain_blockers`, the drained wakes, `record_settlement` and retirement —
//! under seeded random interleavings and fails when a drain is released while
//! a lower-committed sibling has not begun to seat.
//!
//! Each round opens a fresh group and interleaves its children's commits with
//! their drains. A child seats only once its own drain is released, as the
//! protocol orders it, after a random delay, so lower blockers are routinely
//! still in flight when a later one seats. A round may also:
//!
//! - retire the group with members unseated: every parked drain must be
//!   released, by the retirement;
//! - crash a drain while it is parked and redrive it: the redrive commits again
//!   (`AlreadyCommitted`) and reads the barrier afresh;
//! - redrive a seat: the repeated `record_settlement` answers `Duplicate`.
//!
//! The seed is printed; `LASH_DRAIN_LAW_SEED` replays one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::RestateIngressClient;
use crate::effect_group::{
    EffectGroupCommitChildRequest, EffectGroupCommitChildResponse, EffectGroupDrainBlockersRequest,
    EffectGroupDrainBlockersResponse, EffectGroupOpenRequest, EffectGroupOpenResponse,
    EffectGroupRecordSettlementRequest, EffectGroupRecordSettlementResponse,
    EffectGroupSettlementTerminal, EffectGroupWaitResolution, drained_wait_request,
};

use super::effect_group_conformance::{
    await_group_wait, witness_child, witness_key, witness_membership, witness_shape,
};

/// A round that has not finished in this long has a drain the barrier never
/// released.
const ROUND_BUDGET: Duration = Duration::from_secs(90);

/// A small seeded generator: the law's interleavings replay from its seed.
#[derive(Clone)]
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pause(&mut self, max_ms: u64) -> Duration {
        Duration::from_millis(self.below(max_ms + 1))
    }
}

/// The law's seed: `LASH_DRAIN_LAW_SEED`, or one drawn from the clock.
#[expect(
    clippy::disallowed_methods,
    reason = "a test seed is read from the environment and the clock, and printed to replay"
)]
pub(super) fn drain_law_seed() -> u64 {
    std::env::var("LASH_DRAIN_LAW_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or_else(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default();
            u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or(1) | 1
        })
}

/// What a round has observed, under one lock so a release is judged against
/// the seats begun before it.
#[derive(Default)]
struct Observed {
    commit_seqs: BTreeMap<usize, u64>,
    seating: BTreeSet<usize>,
    released: BTreeSet<usize>,
    retiring: bool,
    violations: Vec<String>,
}

impl Observed {
    /// A drain of `position` was released, by drained wakes alone or with a
    /// retirement among them: every lower-committed sibling must have begun
    /// to seat, unless a retirement released it.
    fn release(&mut self, position: usize, by_retirement: bool) {
        let seq = self.commit_seqs[&position];
        if by_retirement && !self.retiring {
            self.violations.push(format!(
                "child {position} (commit {seq}) was released by a retirement nobody began"
            ));
        }
        if !by_retirement {
            let unseated = self
                .commit_seqs
                .iter()
                .filter(|(other, other_seq)| **other_seq < seq && !self.seating.contains(other))
                .map(|(other, other_seq)| format!("{other} (commit {other_seq})"))
                .collect::<Vec<_>>();
            if !unseated.is_empty() {
                self.violations.push(format!(
                    "child {position} (commit {seq}) was released while lower-committed \
                     siblings had not begun to seat: {unseated:?}"
                ));
            }
        }
        self.released.insert(position);
    }
}

/// Runs `rounds` randomised rounds of the law from `seed` against the server
/// behind `ingress`.
pub(super) async fn drain_barrier_is_transitive(
    ingress: RestateIngressClient,
    seed: u64,
    rounds: usize,
) {
    println!("drain-barrier transitivity law: seed {seed} (LASH_DRAIN_LAW_SEED), {rounds} rounds");
    let mut rng = Rng(seed);
    for round in 0..rounds {
        let round_rng = Rng(rng.next() | 1);
        let retire = round % 3 == 1;
        let outcome = tokio::time::timeout(
            ROUND_BUDGET,
            run_round(ingress.clone(), round, round_rng, retire),
        )
        .await;
        let observed = outcome.unwrap_or_else(|_| {
            panic!(
                "round {round} (seed {seed}): a drain was never released within {ROUND_BUDGET:?}"
            )
        });
        assert!(
            observed.violations.is_empty(),
            "round {round} (seed {seed}): {:#?}",
            observed.violations
        );
    }
}

async fn run_round(
    ingress: RestateIngressClient,
    round: usize,
    mut rng: Rng,
    retire: bool,
) -> Observed {
    let width = 3 + rng.below(6) as usize;
    let group_key = witness_key(&format!("drain-transitive-{round}"));
    let children = (0..width)
        .map(|position| witness_child(&group_key, position))
        .collect::<Vec<_>>();
    let shape = witness_shape(&group_key, &children);
    let opened: EffectGroupOpenResponse = ingress
        .call_lash_object(
            "EffectGroupIndex",
            &group_key,
            "open",
            &EffectGroupOpenRequest {
                shape: shape.clone(),
                membership: witness_membership(&children),
                dispatch_route: "EffectGroupDispatch".to_string(),
                content_checked: false,
            },
        )
        .await
        .expect("the law's group opens");
    assert!(
        matches!(opened, EffectGroupOpenResponse::OpenedFresh { .. }),
        "the law's group opens fresh: {opened:?}"
    );
    let observed = Arc::new(Mutex::new(Observed::default()));

    // Commits interleave with the drains of the children already committed.
    let mut order = (0..width).collect::<Vec<_>>();
    for index in (1..order.len()).rev() {
        order.swap(index, rng.below(index as u64 + 1) as usize);
    }
    let mut drains = Vec::new();
    for position in order {
        let seq = commit(&ingress, &group_key, &shape, position)
            .await
            .expect("the law's group is live while its children commit");
        observed
            .lock()
            .expect("the law's observations")
            .commit_seqs
            .insert(position, seq);
        drains.push(tokio::spawn(drain(
            ingress.clone(),
            group_key.clone(),
            shape.clone(),
            position,
            seq,
            Rng(rng.next() | 1),
            Arc::clone(&observed),
        )));
        tokio::time::sleep(rng.pause(15)).await;
    }

    // Seat released children in a random order after random delays; a
    // retiring round stops after a random prefix and retires the group.
    let seat_before_retiring = retire.then(|| rng.below(width as u64) as usize);
    let mut seated = 0;
    while seated < width {
        if seat_before_retiring == Some(seated) {
            observed.lock().expect("the law's observations").retiring = true;
            ingress
                .call_lash_workflow::<_, ()>(
                    "EffectGroupDispatch",
                    &group_key,
                    "retire",
                    &group_key,
                )
                .await
                .expect("the law's retirement completes");
            break;
        }
        let ready = {
            let observed = observed.lock().expect("the law's observations");
            observed
                .released
                .difference(&observed.seating)
                .copied()
                .collect::<Vec<_>>()
        };
        if ready.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
            continue;
        }
        let position = ready[rng.below(ready.len() as u64) as usize];
        tokio::time::sleep(rng.pause(20)).await;
        observed
            .lock()
            .expect("the law's observations")
            .seating
            .insert(position);
        let recorded = seat(&ingress, &group_key, position).await;
        assert!(
            matches!(
                recorded,
                EffectGroupRecordSettlementResponse::Recorded { .. }
            ),
            "child {position} seats: {recorded:?}"
        );
        if rng.chance(25) {
            let redriven = seat(&ingress, &group_key, position).await;
            assert!(
                matches!(
                    redriven,
                    EffectGroupRecordSettlementResponse::Duplicate { .. }
                ),
                "a redriven seat of child {position} is a duplicate: {redriven:?}"
            );
        }
        seated += 1;
    }
    for drain in drains {
        drain.await.expect("a drain task of the law");
    }
    Arc::try_unwrap(observed)
        .ok()
        .expect("every drain task has ended")
        .into_inner()
        .expect("the law's observations")
}

async fn commit(
    ingress: &RestateIngressClient,
    group_key: &str,
    shape: &crate::effect_group::EffectGroupShape,
    position: usize,
) -> Option<u64> {
    let committed: EffectGroupCommitChildResponse = ingress
        .call_lash_object(
            "EffectGroupIndex",
            group_key,
            "commit_child",
            &EffectGroupCommitChildRequest {
                replay_key: shape.replay_keys[position].clone(),
            },
        )
        .await
        .expect("a child of the law commits");
    match committed {
        EffectGroupCommitChildResponse::Committed { commit_seq, .. }
        | EffectGroupCommitChildResponse::AlreadyCommitted { commit_seq, .. } => Some(commit_seq),
        EffectGroupCommitChildResponse::Retired => None,
        other => panic!("child {position} of the law commits, got {other:?}"),
    }
}

async fn seat(
    ingress: &RestateIngressClient,
    group_key: &str,
    position: usize,
) -> EffectGroupRecordSettlementResponse {
    ingress
        .call_lash_object(
            "EffectGroupIndex",
            group_key,
            "record_settlement",
            &EffectGroupRecordSettlementRequest {
                position,
                terminal: EffectGroupSettlementTerminal::Cancelled,
            },
        )
        .await
        .expect("a child of the law seats")
}

/// One child's drain, as the host's drain admission runs it: read the
/// barrier, park on each named sibling's drained wake, then go. It may crash
/// once while parked and redrive: commit again and read the barrier afresh.
async fn drain(
    ingress: RestateIngressClient,
    group_key: String,
    shape: crate::effect_group::EffectGroupShape,
    position: usize,
    seq: u64,
    mut rng: Rng,
    observed: Arc<Mutex<Observed>>,
) {
    let mut crash = rng.chance(30);
    loop {
        let blockers: EffectGroupDrainBlockersResponse = ingress
            .call_lash_object(
                "EffectGroupIndex",
                &group_key,
                "drain_blockers",
                &EffectGroupDrainBlockersRequest { commit_seq: seq },
            )
            .await
            .expect("a drain of the law reads its barrier");
        let (wait_scope, positions) = match blockers {
            EffectGroupDrainBlockersResponse::Admitted => {
                let retired = observed.lock().expect("the law's observations").retiring;
                // An admitted drain is released by the index's own reading:
                // after a retirement the index holds no live barrier.
                observed
                    .lock()
                    .expect("the law's observations")
                    .release(position, retired);
                return;
            }
            EffectGroupDrainBlockersResponse::Blocked {
                wait_scope,
                positions,
            } => (wait_scope, positions),
        };
        if crash {
            crash = false;
            let first = drained_wait_request(&wait_scope, &group_key, positions[0])
                .expect("the law's drained wake");
            // The crash drops the parked await after a moment, whether or not
            // it resolved; the redrive commits again and re-reads.
            let _ = tokio::time::timeout(rng.pause(30), await_group_wait(&ingress, first)).await;
            // A redrive that finds the group retired is released by the
            // retirement, as every parked drain is.
            let Some(redriven) = commit(&ingress, &group_key, &shape, position).await else {
                observed
                    .lock()
                    .expect("the law's observations")
                    .release(position, true);
                return;
            };
            assert_eq!(redriven, seq, "a redriven commit keeps its commit sequence");
            continue;
        }
        let mut by_retirement = false;
        for blocker in positions {
            let request = drained_wait_request(&wait_scope, &group_key, blocker)
                .expect("the law's drained wake");
            match await_group_wait(&ingress, request).await {
                EffectGroupWaitResolution::Drained => {}
                EffectGroupWaitResolution::Retired => by_retirement = true,
                other => panic!("child {blocker}'s drained wake resolved as {other:?}"),
            }
        }
        observed
            .lock()
            .expect("the law's observations")
            .release(position, by_retirement);
        return;
    }
}
