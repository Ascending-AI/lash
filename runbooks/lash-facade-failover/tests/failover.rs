//! The lash-facade-failover runbook's cases (FIG-5193; the facade-host
//! version of FIG-5199's, ADR 0132 §3 and §13): lash cores over one
//! PostgreSQL store, each serving its own node as any facade host's core
//! does, lose a node at a chosen point, and the work finishes on another
//! node.
//!
//! Every case boots its own server and two node processes, `a` and `b`, and
//! judges three things from evidence the nodes cannot rewrite:
//!
//! - **the work finishes on another node**: the store holds no unfinished
//!   turn and exactly one `turn.commit` committed, or the process holds its
//!   terminal, and the survivor did the work after the fault;
//! - **no zombie commits**: the store holds only the new owner's outcome
//!   (its `written_epoch`), and nothing the old owner wrote after the fault;
//! - **no `Once` body ran twice**: the witness ledger holds at most one
//!   `entered` row per `Once` call.
//!
//! A killed node cannot commit at all, and a partitioned node stops itself,
//! dropping its activations, before its lease lapses, and its host learns
//! it from `LashCore::node_stopped`. Each case prints a `case=` line with
//! what it measured; the failover cases check the failover bound of L8
//! (FIG-5178).

// Test code: the harness reads its environment and panics on a broken law.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

mod support;

use std::time::{Duration, Instant};

use lash_durable::{CommitLabel, LeaseSettings, Notifier};
use lash_facade_failover::events::{Command, Event};
use lash_facade_failover::turn::{self, TOOL};
use lash_facade_failover::witness::Hold;

use support::cluster::{Cluster, Entry};

/// The node binary the cases boot.
const NODE_BIN: &str = env!("CARGO_BIN_EXE_lash-facade-failover-node");
const NODES: [&str; 2] = ["a", "b"];
/// How long any one step of a case may take before the case fails.
const STEP: Duration = Duration::from_secs(60);
/// The slack a measured bound allows for scheduling in a loaded action.
const SLACK: Duration = Duration::from_secs(1);

fn lease() -> LeaseSettings {
    LeaseSettings::default()
}

fn other(node: &str) -> &'static str {
    if node == NODES[0] { NODES[1] } else { NODES[0] }
}

fn session_actor() -> String {
    turn::actor().to_string()
}

fn ms(duration: Duration) -> u128 {
    duration.as_millis()
}

fn is_model_attempt(entry: &Entry, call: u32, attempt: u32) -> bool {
    entry.event == Event::ModelAttempt { call, attempt }
}

fn is_body(entry: &Entry, tool: &str, phase: &str) -> bool {
    matches!(&entry.event, Event::Body { tool: t, phase: p, .. } if t == tool && p == phase)
}

fn is_session_claim(entry: &Entry, node: &str) -> bool {
    entry.node == node
        && matches!(&entry.event, Event::Claimed { actor, .. } if *actor == session_actor())
}

fn is_reap_of(entry: &Entry, reaper: &str, dead: &str, actor: &str) -> bool {
    entry.node == reaper
        && matches!(&entry.event, Event::Reaped { from, actor: a, .. } if from == dead && a == actor)
}

fn is_turn_commit(entry: &Entry) -> bool {
    matches!(&entry.event, Event::Commit { label, outcome, .. }
        if label == CommitLabel::TURN_COMMIT.as_str() && outcome == "committed")
}

/// Wait for the turn's one committed `turn.commit`; check the store holds
/// no unfinished turn and that nothing else committed one. Answers the
/// committing node.
async fn turn_finished(cluster: &Cluster) -> String {
    let commit = cluster.wait(STEP, "the turn commits", is_turn_commit).await;
    let open = cluster
        .durable()
        .turn(&turn::session())
        .await
        .expect("read the turn");
    assert!(
        open.is_none(),
        "the turn committed but is still open: {open:?}"
    );
    let end = cluster
        .durable()
        .turn_end(&turn::session(), &turn::run())
        .await
        .expect("read the turn's end")
        .expect("the turn has an end");
    assert!(
        end.head_revision.is_some(),
        "the turn ended without publishing a head: {end:?}"
    );
    // Give a racing second commit a claim poll to show itself.
    tokio::time::sleep(lease().claim_poll * 2).await;
    let commits: Vec<String> = cluster
        .reports()
        .iter()
        .filter(|entry| is_turn_commit(entry))
        .map(|entry| entry.node.clone())
        .collect();
    assert_eq!(
        commits.len(),
        1,
        "the turn committed {} times: {commits:?}",
        commits.len()
    );
    commit.node
}

/// No `Once` body ran twice: at most one `entered` row per call. Answers
/// the effects of `tool`.
async fn once_law(cluster: &Cluster, tool: &str) -> Vec<support::cluster::Effect> {
    let effects = cluster.effects().await;
    let mut entered = std::collections::BTreeMap::<&str, Vec<&str>>::new();
    for effect in effects.iter().filter(|effect| effect.phase == "entered") {
        entered
            .entry(effect.call_id.as_str())
            .or_default()
            .push(effect.node.as_str());
    }
    for (call, nodes) in &entered {
        assert!(
            nodes.len() <= 1,
            "Once: call {call} was entered {} times, on {nodes:?}",
            nodes.len()
        );
    }
    effects
        .into_iter()
        .filter(|effect| effect.tool == tool)
        .collect()
}

/// How the cell's `ext_write` call settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CellOutcome {
    /// Its body's outcome committed (`round.outcome`).
    Completed,
    /// A started `Once` whose owner was lost settled `Interrupted` when the
    /// next owner resumed the cell (`cell.inject`).
    Interrupted,
}

/// The cell's `ext_write` outcome, with the epoch of the commit that wrote
/// it, from the commits the store acknowledged. The cell's run records do
/// not outlive it: its last snapshot prunes every run no snapshot can feed
/// back, so the store's acknowledgements of the two commits that settle an
/// operation are the evidence. The operation settles exactly once.
fn cell_outcome(cluster: &Cluster) -> (CellOutcome, i64) {
    let settled: Vec<(CellOutcome, i64)> = cluster
        .reports()
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Commit {
                label,
                outcome,
                epoch,
                ..
            } if outcome == "committed" => {
                if label == CommitLabel::ROUND_OUTCOME.as_str() {
                    Some((CellOutcome::Completed, *epoch))
                } else if label == CommitLabel::CELL_INJECT.as_str() {
                    Some((CellOutcome::Interrupted, *epoch))
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();
    let [outcome] = settled.as_slice() else {
        panic!("the operation settled {} times: {settled:?}", settled.len());
    };
    *outcome
}

/// The epoch `node` claimed the session under, last.
fn claim_epoch(cluster: &Cluster, node: &str) -> i64 {
    cluster
        .reports()
        .iter()
        .rev()
        .find_map(|entry| match &entry.event {
            Event::Claimed { actor, epoch } if entry.node == node && *actor == session_actor() => {
                Some(*epoch)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("{node} never claimed the session"))
}

/// A turn whose node is killed mid-model-call finishes on the other node,
/// which re-sends the pinned call; returns (detect, resume) as measured.
async fn kill_mid_turn(notifier: Notifier, by: &str) -> (Duration, Duration) {
    let mut cluster = Cluster::start(&NODES, notifier, Hold::Model).await;
    cluster.admit_turn().await;
    let held = cluster
        .wait(STEP, "the first model attempt", |entry| {
            is_model_attempt(entry, 1, 1)
        })
        .await;
    let victim = held.node.clone();
    let survivor = other(&victim);
    cluster.seen_alive(survivor, &victim).await;
    let killed = cluster.kill(&victim).await;
    let reap = cluster
        .wait_after(
            Some(killed),
            STEP,
            "the survivor reaps the victim",
            |entry| is_reap_of(entry, survivor, &victim, &session_actor()),
        )
        .await;
    assert!(
        matches!(&reap.event, Event::Reaped { by: b, .. } if b == by),
        "the reap went by {:?}, expected {by}",
        reap.event
    );
    let claim = cluster
        .wait_after(Some(reap.at), STEP, "the survivor claims", |entry| {
            is_session_claim(entry, survivor)
        })
        .await;
    let resent = cluster
        .wait_after(Some(reap.at), STEP, "the call is re-sent", |entry| {
            entry.node == survivor && is_model_attempt(entry, 1, 2)
        })
        .await;
    let finisher = turn_finished(&cluster).await;
    assert_eq!(finisher, survivor, "the turn finished on {finisher}");

    let effects = once_law(&cluster, TOOL).await;
    let entered: Vec<_> = effects.iter().filter(|e| e.phase == "entered").collect();
    assert!(
        entered.len() == 1 && entered[0].node == survivor,
        "ext.write ran {entered:?}, expected once on {survivor}"
    );
    let (recovery, _) = cell_outcome(&cluster);
    assert_eq!(recovery, CellOutcome::Completed, "ext_write settled");
    let attempts = cluster.model_attempts().await;
    let first: Vec<(i32, &str)> = attempts
        .iter()
        .filter(|attempt| attempt.call_index == 1)
        .map(|attempt| (attempt.attempt, attempt.node.as_str()))
        .collect();
    assert_eq!(
        first,
        vec![(1, victim.as_str()), (2, survivor)],
        "the first call's attempts"
    );

    let detect = reap.at.saturating_duration_since(killed);
    let resume = claim.at.saturating_duration_since(reap.at);
    eprintln!(
        "case=kill-mid-turn notifier={by} victim={victim} survivor={survivor} detect_ms={} \
         resume_ms={} resend_ms={}",
        ms(detect),
        ms(resume),
        ms(resent.at.saturating_duration_since(killed)),
    );
    (detect, resume)
}

/// Kill -9 mid-turn, with the liveness lock: the crash is detected within
/// about the lock's probe and the work resumes within a claim poll of the
/// reap (L8's failover bound).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_killed_mid_model_call_finishes_on_another_node() {
    let (detect, resume) = kill_mid_turn(Notifier::AfterCommit, "lock").await;
    let lease = lease();
    assert!(
        detect <= lease.claim_poll + SLACK,
        "detection took {detect:?}, over the lock bound {:?}",
        lease.claim_poll + SLACK
    );
    assert!(
        resume <= lease.claim_poll + SLACK,
        "the claim came {resume:?} after the reap, over {:?}",
        lease.claim_poll + SLACK
    );
}

/// Kill -9 mid-turn, without the liveness lock: the crash is found by the
/// lease, within `ttl` + `reap_every`, and the work resumes within a claim
/// poll of the reap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_killed_mid_model_call_without_the_liveness_lock_finishes_within_the_lease_bound() {
    let (detect, resume) = kill_mid_turn(Notifier::PollOnly, "lease").await;
    let lease = lease();
    let bound = lease.ttl + lease.reap_every + SLACK;
    assert!(
        detect <= bound,
        "detection took {detect:?}, over the lease bound {bound:?}"
    );
    assert!(
        resume <= lease.claim_poll + SLACK,
        "the claim came {resume:?} after the reap, over {:?}",
        lease.claim_poll + SLACK
    );
}

/// Kill -9 mid-step: the node dies inside `ext.write`'s body, after its
/// external work began. The other node records the started `Once` as
/// `Interrupted` and never enters the body; the turn finishes there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_once_step_killed_mid_body_settles_interrupted_on_another_node() {
    let mut cluster = Cluster::start(&NODES, Notifier::AfterCommit, Hold::Step).await;
    cluster.admit_turn().await;
    let entered = cluster
        .wait(STEP, "ext.write is entered", |entry| {
            is_body(entry, TOOL, "entered")
        })
        .await;
    let victim = entered.node.clone();
    let survivor = other(&victim);
    cluster.seen_alive(survivor, &victim).await;
    let killed = cluster.kill(&victim).await;
    let reap = cluster
        .wait_after(
            Some(killed),
            STEP,
            "the survivor reaps the victim",
            |entry| is_reap_of(entry, survivor, &victim, &session_actor()),
        )
        .await;
    let finisher = turn_finished(&cluster).await;
    assert_eq!(finisher, survivor, "the turn finished on {finisher}");

    let effects = once_law(&cluster, TOOL).await;
    let phases: Vec<(&str, &str)> = effects
        .iter()
        .map(|effect| (effect.phase.as_str(), effect.node.as_str()))
        .collect();
    assert_eq!(
        phases,
        vec![("entered", victim.as_str())],
        "ext.write's witness"
    );
    let (recovery, epoch) = cell_outcome(&cluster);
    assert_eq!(
        recovery,
        CellOutcome::Interrupted,
        "a started Once whose node died settles Interrupted"
    );
    let claimed = claim_epoch(&cluster, survivor);
    assert_eq!(
        epoch, claimed,
        "the outcome was written under the survivor's epoch"
    );
    eprintln!(
        "case=kill-mid-step victim={victim} survivor={survivor} detect_ms={} outcome=interrupted \
         outcome_epoch={epoch}",
        ms(reap.at.saturating_duration_since(killed)),
    );
}

/// Partition: one node's heartbeat is held while its `Once` body keeps
/// running. The node stops itself, dropping the body, within
/// `self_stop_after` and one poll of its last renewal, before its lease
/// lapses; the other node reaps it and finishes the turn with the operation
/// `Interrupted`, and the body is never entered again (FIG-5178).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_whose_heartbeat_is_held_stops_itself_before_its_lease_lapses() {
    let mut cluster = Cluster::start(&NODES, Notifier::AfterCommit, Hold::StepUntilRelease).await;
    cluster.admit_turn().await;
    let entered = cluster
        .wait(STEP, "ext.write is entered", |entry| {
            is_body(entry, TOOL, "entered")
        })
        .await;
    let zombie = entered.node.clone();
    let survivor = other(&zombie);
    cluster.send(&zombie, Command::BlockHeartbeat).await;
    cluster.nemesis("partition", Some(&zombie)).await;
    let partitioned = Instant::now();
    // Its lease was last extended by its registration or its last renewal.
    let last_renewed = cluster
        .reports()
        .iter()
        .rev()
        .find(|entry| {
            entry.node == zombie
                && match &entry.event {
                    Event::Registered { .. } => true,
                    Event::Heartbeat { outcome } => outcome == "renewed",
                    _ => false,
                }
        })
        .map(|entry| entry.at)
        .expect("the zombie registered");
    let lease = lease();
    // The runner reports its stop only once every activation it ran is
    // dropped, the held body among them.
    let stopped = cluster
        .wait_after(
            Some(partitioned),
            lease.self_stop_after + STEP,
            "the zombie stops itself",
            |entry| entry.node == zombie && matches!(entry.event, Event::Stopped { .. }),
        )
        .await;
    assert_eq!(
        stopped.event,
        Event::Stopped {
            why: "unrenewed".to_owned()
        },
        "the zombie stopped"
    );
    let served = stopped.at.saturating_duration_since(last_renewed);
    assert!(
        served <= lease.self_stop_after + lease.claim_poll,
        "the zombie served {} ms past its last renewal, past self_stop_after ({} ms) and one poll ({} ms)",
        ms(served),
        ms(lease.self_stop_after),
        ms(lease.claim_poll),
    );
    cluster.exited(&zombie, STEP).await;
    let reap = cluster
        .wait_after(
            Some(partitioned),
            lease.ttl + lease.reap_every + STEP,
            "the survivor reaps the zombie",
            |entry| is_reap_of(entry, survivor, &zombie, &session_actor()),
        )
        .await;
    let finisher = turn_finished(&cluster).await;
    assert_eq!(finisher, survivor, "the turn finished on {finisher}");
    let late: Vec<Entry> = cluster
        .reports()
        .into_iter()
        .filter(|entry| entry.node == zombie && entry.at >= partitioned)
        .filter(
            |entry| matches!(&entry.event, Event::Commit { outcome, .. } if outcome == "committed"),
        )
        .collect();
    assert!(
        late.is_empty(),
        "the zombie committed while partitioned: {late:?}"
    );

    // The operation's one outcome is the survivor's `Interrupted`, and the
    // dropped body neither returned nor ran again.
    let (recovery, epoch) = cell_outcome(&cluster);
    assert_eq!(recovery, CellOutcome::Interrupted, "the outcome");
    assert_eq!(
        epoch,
        claim_epoch(&cluster, survivor),
        "the outcome's epoch"
    );
    let effects = once_law(&cluster, TOOL).await;
    assert_eq!(
        effects
            .iter()
            .map(|effect| (effect.phase.as_str(), effect.node.as_str()))
            .collect::<Vec<_>>(),
        vec![("entered", zombie.as_str())],
        "ext.write ran once, on the zombie, and was dropped there"
    );
    eprintln!(
        "case=partition zombie={zombie} survivor={survivor} reap_after_partition_ms={} \
         zombie_served_past_last_renewal_ms={} self_stop_after_ms={} outcome=interrupted",
        ms(reap.at.saturating_duration_since(partitioned)),
        ms(served),
        ms(lease.self_stop_after),
    );
}

/// Clean shutdown: a node told to stop releases its actors, and the other
/// node takes the turn over within a claim poll, with no reap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cleanly_stopped_node_hands_its_turn_over_at_once() {
    let mut cluster = Cluster::start(&NODES, Notifier::AfterCommit, Hold::Model).await;
    cluster.admit_turn().await;
    let held = cluster
        .wait(STEP, "the first model attempt", |entry| {
            is_model_attempt(entry, 1, 1)
        })
        .await;
    let leaving = held.node.clone();
    let survivor = other(&leaving);
    cluster.send(&leaving, Command::Stop).await;
    let asked = Instant::now();
    cluster.nemesis("stop", Some(&leaving)).await;
    let released = cluster
        .wait_after(Some(asked), STEP, "the node releases its actors", |entry| {
            entry.node == leaving
                && matches!(&entry.event, Event::Released { actors } if actors.contains(&session_actor()))
        })
        .await;
    let claim = cluster
        .wait_after(Some(asked), STEP, "the survivor claims", |entry| {
            is_session_claim(entry, survivor)
        })
        .await;
    let finisher = turn_finished(&cluster).await;
    assert_eq!(finisher, survivor, "the turn finished on {finisher}");
    cluster.exited(&leaving, STEP).await;
    let stopped: Vec<Event> = cluster
        .reports()
        .into_iter()
        .filter(|entry| entry.node == leaving && matches!(entry.event, Event::Stopped { .. }))
        .map(|entry| entry.event)
        .collect();
    assert_eq!(
        stopped,
        vec![Event::Stopped {
            why: "requested".to_owned()
        }],
        "the node stopped on request"
    );
    let reaps: Vec<Entry> = cluster
        .reports()
        .into_iter()
        .filter(|entry| matches!(&entry.event, Event::Reaped { from, .. } if *from == leaving))
        .collect();
    assert!(
        reaps.is_empty(),
        "a cleanly stopped node was reaped: {reaps:?}"
    );
    let effects = once_law(&cluster, TOOL).await;
    assert_eq!(
        effects.iter().filter(|e| e.phase == "entered").count(),
        1,
        "ext.write ran once: {effects:?}"
    );
    let handover = claim.at.saturating_duration_since(released.at);
    let bound = lease().claim_poll + SLACK;
    assert!(
        handover <= bound,
        "the handover took {handover:?}, over {bound:?}"
    );
    eprintln!(
        "case=clean-shutdown leaving={leaving} survivor={survivor} release_ms={} handover_ms={}",
        ms(released.at.saturating_duration_since(asked)),
        ms(handover),
    );
}

/// A PostgreSQL restart: the server stops while a turn's model call is in
/// flight and starts again. No node is reaped, no node stops, and the turn
/// finishes with its `Once` operation run once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_postgres_restart_strands_no_work_and_reaps_no_node() {
    let mut cluster = Cluster::start(&NODES, Notifier::AfterCommit, Hold::ModelUntilRelease).await;
    cluster.admit_turn().await;
    let held = cluster
        .wait(STEP, "the first model attempt", |entry| {
            is_model_attempt(entry, 1, 1)
        })
        .await;
    cluster.nemesis("restart-begin", None).await;
    let down = Instant::now();
    cluster.server().stop().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    cluster.server().restart().await;
    let up = Instant::now();
    cluster.nemesis("restart-complete", None).await;
    cluster.nemesis("release", None).await;
    let finisher = turn_finished(&cluster).await;
    let finished = Instant::now();

    let reaps: Vec<Entry> = cluster
        .reports()
        .into_iter()
        .filter(|entry| matches!(entry.event, Event::Reaped { .. }))
        .collect();
    assert!(reaps.is_empty(), "the restart reaped a node: {reaps:?}");
    let stops: Vec<Entry> = cluster
        .reports()
        .into_iter()
        .filter(|entry| matches!(entry.event, Event::Stopped { .. }))
        .collect();
    assert!(stops.is_empty(), "the restart stopped a node: {stops:?}");
    let effects = once_law(&cluster, TOOL).await;
    assert_eq!(
        effects.iter().filter(|e| e.phase == "entered").count(),
        1,
        "ext.write ran once: {effects:?}"
    );
    let (recovery, _) = cell_outcome(&cluster);
    assert_eq!(recovery, CellOutcome::Completed, "ext_write settled");
    let attempts = cluster.model_attempts().await;
    eprintln!(
        "case=postgres-restart held_on={} finished_on={finisher} outage_ms={} \
         finish_after_restart_ms={} model_attempts={:?}",
        held.node,
        ms(up.saturating_duration_since(down)),
        ms(finished.saturating_duration_since(up)),
        attempts
            .iter()
            .map(|attempt| (attempt.call_index, attempt.attempt, attempt.node.clone()))
            .collect::<Vec<_>>(),
    );
}
