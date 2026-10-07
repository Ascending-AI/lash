//! Turn scenarios: L12a's round and concurrency shapes, the cold resume of
//! a turn at different sizes and after different prior-turn counts (H2),
//! and the RLM cell's snapshot per block (H5).

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use futures_util::future::try_join_all;
use lash_core::runtime::durable::session_close::request_session_close;
use lash_durable::{ActorKey, CommitLabel};
use lash_sansio::{SessionId, TurnId};
use serde::Serialize;

use crate::deploy::{Deployment, micros};
use crate::recorder::Transaction;
use crate::support::{Counters, Distribution, Snapshot, by_label};
use crate::turn::{Script, admit, create_session, session_actor, session_id};
use crate::{Case, Run};

/// How long a turn may take before the bench gives up on it.
const TURN_LIMIT: Duration = Duration::from_secs(600);

fn turn_id(name: &str) -> Result<TurnId> {
    TurnId::try_from(name.to_owned()).map_err(|error| anyhow::anyhow!("{error}"))
}

/// One answered turn, L12a's fields.
#[derive(Serialize)]
struct TurnSample {
    case: String,
    dialect: &'static str,
    nodes: usize,
    batch: usize,
    session: String,
    rounds: usize,
    tools_per_round: usize,
    /// Admission commit start to `turn.commit`, microseconds.
    total_us: u64,
    /// Admission commit start to the first model entry.
    first_model_us: u64,
    /// Successive model entries: one per tool round.
    round_us: Vec<u64>,
    /// Last model entry to `turn.commit`.
    final_tail_us: u64,
}

/// One batch's writes and window.
#[derive(Serialize)]
struct BatchRecord<'a> {
    case: &'a str,
    dialect: &'static str,
    nodes: usize,
    batch: usize,
    sessions: usize,
    window_us: u64,
    transactions: std::collections::BTreeMap<&'static str, usize>,
    durable_transactions: usize,
    checkpoint_bytes: usize,
    snapshot_bytes: usize,
    counters: Snapshot,
}

/// Open the turn of `run` on `session` through producer `index`, and wait
/// for its `turn.commit`; returns (admission start, commit).
async fn one_turn(
    deployment: &Deployment,
    index: usize,
    session: &SessionId,
    run: &TurnId,
) -> Result<(Instant, Instant)> {
    let actor = session_actor(session)?;
    let committed = deployment.recorder.watch(&actor, CommitLabel::TURN_COMMIT);
    let started = Instant::now();
    admit(deployment.producer(index), session, run)
        .await
        .with_context(|| format!("admit {run}"))?;
    let done = tokio::time::timeout(TURN_LIMIT, committed)
        .await
        .with_context(|| format!("{run} did not commit"))?
        .context("the recorder dropped the watch")?;
    Ok((started, done))
}

fn sample(
    run: &Run<'_>,
    case: &Case,
    batch: usize,
    session: &SessionId,
    entries: &[Instant],
    (started, done): (Instant, Instant),
) -> Result<TurnSample> {
    ensure!(
        entries.len() == case.script.rounds + 1,
        "{session}: {} model calls for {} rounds",
        entries.len(),
        case.script.rounds
    );
    Ok(TurnSample {
        case: case.name.clone(),
        dialect: run.database.dialect(),
        nodes: run.nodes,
        batch,
        session: session.to_string(),
        rounds: case.script.rounds,
        tools_per_round: case.script.tools_per_round,
        total_us: micros(started, done),
        first_model_us: micros(started, entries[0]),
        round_us: entries
            .windows(2)
            .map(|pair| micros(pair[0], pair[1]))
            .collect(),
        final_tail_us: micros(entries[entries.len() - 1], done),
    })
}

fn batch_record<'a>(
    run: &Run<'_>,
    case: &'a Case,
    batch: usize,
    sessions: usize,
    window_us: u64,
    transactions: &[Transaction],
    counters: Snapshot,
) -> BatchRecord<'a> {
    BatchRecord {
        case: &case.name,
        dialect: run.database.dialect(),
        nodes: run.nodes,
        batch,
        sessions,
        window_us,
        transactions: by_label(transactions),
        durable_transactions: transactions.iter().filter(|t| t.ok).count(),
        checkpoint_bytes: transactions.iter().map(|t| t.checkpoint_bytes).sum(),
        snapshot_bytes: transactions.iter().map(|t| t.snapshot_bytes).sum(),
        counters,
    }
}

/// Close `sessions` and wait for each actor's end: L12a's teardown,
/// outside every measured window. A closed session frees its node's slot.
async fn close(deployment: &Deployment, sessions: &[SessionId]) -> Result<()> {
    try_join_all(
        sessions
            .iter()
            .enumerate()
            .map(|(index, session)| async move {
                let actor = session_actor(session)?;
                let ended = deployment
                    .recorder
                    .watch(&actor, CommitLabel::SESSION_CLOSE_TOMBSTONE);
                request_session_close(deployment.producer(index), session)
                    .await
                    .map_err(|error| anyhow::anyhow!("close {session}: {error}"))?;
                tokio::time::timeout(TURN_LIMIT, ended)
                    .await
                    .with_context(|| format!("{session} did not close"))?
                    .context("the recorder dropped the watch")?;
                anyhow::Ok(())
            }),
    )
    .await?;
    Ok(())
}

/// L12a's rounds and concurrency shapes: each batch opens `sessions`
/// fresh sessions and admits one turn on each at once.
pub async fn rounds(run: &Run<'_>, case: &Case, sessions: usize) -> Result<()> {
    let mut deployment = run.deployment();
    deployment.boot_many(run.nodes).await?;
    let counters = Counters::open(&run.database).await?;
    let mut rounds = Vec::new();
    let mut totals = Vec::new();
    let mut window = 0;
    for batch in 0..run.samples {
        let names: Vec<SessionId> = (0..sessions)
            .map(|index| session_id(&format!("{}-b{batch}-s{index}", case.name)))
            .collect::<Result<_>>()?;
        for (index, session) in names.iter().enumerate() {
            create_session(deployment.producer(index), session).await?;
            deployment.scripts.set(session, case.script);
        }
        let before = counters.read().await?;
        let from = deployment.recorder.now_us();
        let started = Instant::now();
        let turns = try_join_all(names.iter().enumerate().map(|(index, session)| {
            let deployment = &deployment;
            async move {
                let run_id = turn_id(&format!("{session}-t0"))?;
                one_turn(deployment, index, session, &run_id).await
            }
        }))
        .await?;
        let finished = turns.iter().map(|(_, done)| *done).max().unwrap_or(started);
        // Let trailing writes of the turns' tails land in the window.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let after = counters.read().await?;
        let transactions = deployment.recorder.transactions_since(from);
        let window_us = micros(started, finished);
        window += window_us;
        for (session, timing) in names.iter().zip(turns) {
            let record = sample(
                run,
                case,
                batch,
                session,
                &deployment.recorder.model_calls(session),
                timing,
            )?;
            rounds.extend(record.round_us.iter().copied());
            totals.push(record.total_us);
            run.report.write("turn", &record)?;
        }
        run.report.write(
            "batch",
            &batch_record(
                run,
                case,
                batch,
                sessions,
                window_us,
                &transactions,
                after.since(&before),
            ),
        )?;
        close(&deployment, &names).await?;
    }
    let completed = totals.len();
    run.report.write(
        "summary",
        &serde_json::json!({
            "case": case.name,
            "dialect": run.database.dialect(),
            "nodes": run.nodes,
            "turns": completed,
            "round": Distribution::of(&rounds).ok(),
            "total": Distribution::of(&totals)?,
            "turns_per_s": completed as f64 / (window as f64 / 1e6),
        }),
    )?;
    counters.close().await;
    deployment.shutdown().await
}

#[derive(Serialize)]
struct ResumeSample {
    case: String,
    dialect: &'static str,
    prior_turns: usize,
    rounds_before_hold: usize,
    /// Bytes of the turn checkpoint the held call's `model.start` wrote.
    checkpoint_bytes: usize,
    /// Node stop (with release) to the node's exit.
    stop_us: u64,
    /// Fresh node boot to `turn.commit`.
    boot_to_commit_us: u64,
    /// The fresh node's claim of the session to `turn.commit`.
    claim_to_commit_us: u64,
    /// The fresh node's `turn.commit` transaction.
    commit_us: u64,
}

fn held_checkpoint(transactions: &[Transaction], actor: &ActorKey) -> usize {
    let actor = actor.to_string();
    transactions
        .iter()
        .rev()
        .find(|t| t.actor == actor && t.checkpoint_bytes > 0)
        .map_or(0, |t| t.checkpoint_bytes)
}

/// A turn's cold resume: the turn holds at model call `rounds` (after
/// `rounds` tool rounds) on one node, which stops cleanly after a second;
/// a fresh node, with nothing cached, claims the session, restores the turn
/// from its committed checkpoint and finishes it. The session has
/// `prior_turns` committed turns before it.
pub async fn resume(run: &Run<'_>, case: &Case, prior_turns: usize) -> Result<()> {
    let mut deployment = run.deployment();
    deployment.boot_many(1).await?;
    let session = session_id(&case.name)?;
    let actor = session_actor(&session)?;
    create_session(deployment.producer(0), &session).await?;
    deployment.scripts.set(
        &session,
        Script {
            rounds: 1,
            tools_per_round: 1,
            ..Script::default()
        },
    );
    for turn in 0..prior_turns {
        one_turn(
            &deployment,
            0,
            &session,
            &turn_id(&format!("prior-{turn}"))?,
        )
        .await?;
    }
    let mut boots = Vec::new();
    for sample_index in 0..run.samples {
        let held = Script {
            hold_call: Some(case.script.rounds),
            ..case.script
        };
        deployment.scripts.set(&session, held);
        let reached = deployment.scripts.on_hold(&session);
        let run_id = turn_id(&format!("held-{sample_index}"))?;
        let committed = deployment.recorder.watch(&actor, CommitLabel::TURN_COMMIT);
        let from = deployment.recorder.now_us();
        admit(deployment.producer(0), &session, &run_id).await?;
        tokio::time::timeout(TURN_LIMIT, reached)
            .await
            .context("the held call was never reached")??;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let checkpoint_bytes =
            held_checkpoint(&deployment.recorder.transactions_since(from), &actor);
        let node = deployment.nodes.remove(0);
        let stopping = Instant::now();
        node.stop().await?;
        let stopped = Instant::now();
        let booted = Instant::now();
        deployment.boot().await?;
        let done = tokio::time::timeout(TURN_LIMIT, committed)
            .await
            .context("the resumed turn did not commit")??;
        let claim = deployment
            .recorder
            .claims_of(&actor)
            .into_iter()
            .filter(|(at, _)| *at >= booted)
            .map(|(at, _)| at)
            .min()
            .unwrap_or(booted);
        let commit_us = deployment
            .recorder
            .transactions_since(from)
            .iter()
            .rev()
            .find(|t| t.actor == actor.to_string() && t.label == "turn.commit")
            .map_or(0, |t| t.micros);
        let record = ResumeSample {
            case: case.name.clone(),
            dialect: run.database.dialect(),
            prior_turns: prior_turns + sample_index,
            rounds_before_hold: case.script.rounds,
            checkpoint_bytes,
            stop_us: micros(stopping, stopped),
            boot_to_commit_us: micros(booted, done),
            claim_to_commit_us: micros(claim, done),
            commit_us,
        };
        boots.push(record.claim_to_commit_us);
        run.report.write("resume", &record)?;
    }
    run.report.write(
        "summary",
        &serde_json::json!({
            "case": case.name,
            "dialect": run.database.dialect(),
            "prior_turns": prior_turns,
            "claim_to_commit": Distribution::of(&boots)?,
        }),
    )?;
    deployment.shutdown().await
}

#[derive(Serialize)]
struct BlockSample {
    case: String,
    dialect: &'static str,
    batch: usize,
    block: usize,
    label: &'static str,
    snapshot_bytes: usize,
    commit_us: u64,
    /// From this block's snapshot commit to the next one's.
    cycle_us: Option<u64>,
}

/// The RLM cell (H5): a turn whose cell awaits `ext.echo` `cell_calls`
/// times, keeping each answer; each await blocks the VM, which commits its
/// snapshot with the operation's admission.
pub async fn cell(run: &Run<'_>, case: &Case) -> Result<()> {
    let mut deployment = run.deployment();
    deployment.boot_many(1).await?;
    let counters = Counters::open(&run.database).await?;
    let mut bytes = Vec::new();
    let mut commits = Vec::new();
    let mut cycles = Vec::new();
    let mut totals = Vec::new();
    for batch in 0..run.samples {
        let session = session_id(&format!("{}-b{batch}", case.name))?;
        let actor = session_actor(&session)?.to_string();
        crate::cells::create_session(deployment.cells(0), &session).await?;
        deployment.scripts.set(&session, case.script);
        let before = counters.read().await?;
        let from = deployment.recorder.now_us();
        let timing = one_turn(
            &deployment,
            0,
            &session,
            &turn_id(&format!("{session}-t0"))?,
        )
        .await?;
        let after = counters.read().await?;
        let transactions = deployment.recorder.transactions_since(from);
        let blocks: Vec<&Transaction> = transactions
            .iter()
            .filter(|t| t.actor == actor && t.ok && t.snapshot_bytes > 0)
            .collect();
        for (index, block) in blocks.iter().enumerate() {
            let cycle_us = blocks.get(index + 1).map(|next| next.at_us - block.at_us);
            bytes.push(block.snapshot_bytes as u64);
            commits.push(block.micros);
            cycles.extend(cycle_us);
            run.report.write(
                "block",
                &BlockSample {
                    case: case.name.clone(),
                    dialect: run.database.dialect(),
                    batch,
                    block: index,
                    label: block.label,
                    snapshot_bytes: block.snapshot_bytes,
                    commit_us: block.micros,
                    cycle_us,
                },
            )?;
        }
        totals.push(micros(timing.0, timing.1));
        run.report.write(
            "batch",
            &batch_record(
                run,
                case,
                batch,
                1,
                micros(timing.0, timing.1),
                &transactions,
                after.since(&before),
            ),
        )?;
        close(&deployment, std::slice::from_ref(&session)).await?;
    }
    bytes.sort_unstable();
    run.report.write(
        "summary",
        &serde_json::json!({
            "case": case.name,
            "dialect": run.database.dialect(),
            "blocks": commits.len(),
            "snapshot_bytes_p50": bytes.get(bytes.len() / 2),
            "snapshot_bytes_max": bytes.last(),
            "snapshot_commit": Distribution::of(&commits)?,
            "block_cycle": Distribution::of(&cycles).ok(),
            "total": Distribution::of(&totals)?,
        }),
    )?;
    counters.close().await;
    deployment.shutdown().await
}
