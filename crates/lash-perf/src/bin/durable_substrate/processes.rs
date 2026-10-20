//! Process scenarios: a parked process resumed by an external completion
//! (L12a), a process resolved ten times while hot (design §6 (d)), and the
//! idle cost of waiting actors (H8).

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use lash_core_execution::runtime::actor::waits::ResolveAnswer;
use lash_core_execution::runtime::actor::waits::resolve_host;
use lash_durable::{ActorKey, ActorState};
use lash_execution_contracts::backend::Backend;
use lash_execution_contracts::process::LifetimeDecision;
use lash_execution_contracts::process::ProcessInput;
use lash_execution_contracts::process::ProcessProvenance;
use lash_execution_contracts::process::ProcessRegistration;
use lash_execution_contracts::waits::PinnedKey;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::deploy::micros;
use crate::process::{KIND, Seen, payload};
use crate::support::{Counters, Distribution, by_label};
use crate::{Case, Run};

const LIMIT: Duration = Duration::from_secs(600);

/// Register a bench process that awaits `waits` keys; returns its actor.
async fn start(backend: &Backend, token: &str, waits: usize) -> Result<ActorKey> {
    start_payload(backend, token, payload(token, waits)).await
}

async fn start_payload(
    backend: &Backend,
    token: &str,
    payload: serde_json::Value,
) -> Result<ActorKey> {
    let registration = ProcessRegistration::new(
        ProcessInput::Engine {
            kind: KIND.to_owned(),
            payload,
            launch: lash_execution_contracts::process::EngineLaunch::new(
                lash_execution_contracts::testing::process_execution_env_fixture_ref(),
                None,
            ),
        },
        ProcessProvenance::host(),
        LifetimeDecision::Detached,
    );
    let record = backend
        .process_registry()
        .register_process(registration)
        .await
        .map_err(|error| anyhow::anyhow!("register {token}: {error}"))?;
    ActorKey::process(record.id.as_str()).map_err(|error| anyhow::anyhow!("{error}"))
}

async fn next(feed: &mut UnboundedReceiver<Seen>) -> Result<Seen> {
    tokio::time::timeout(LIMIT, feed.recv())
        .await
        .context("the engine went quiet")?
        .context("the engine's feed closed")
}

async fn pinned(feed: &mut UnboundedReceiver<Seen>) -> Result<(Instant, PinnedKey)> {
    match next(feed).await? {
        Seen::Pinned(at, key) => Ok((at, key)),
        other => bail!("expected a pinned key, saw {other:?}"),
    }
}

async fn resolve(backend: &Backend, key: &PinnedKey) -> Result<()> {
    let answer = resolve_host(
        backend,
        key.as_str(),
        lash_core_execution::runtime::actor::waits::Resolution::Ok(
            serde_json::json!({ "ok": true }),
        ),
    )
    .await
    .map_err(|error| anyhow::anyhow!("resolve: {error}"))?;
    if answer != ResolveAnswer::Resolved {
        bail!("the resolution was answered {answer:?}");
    }
    Ok(())
}

/// Wait until `actor` has no owner; returns how long that took.
async fn released(
    backend: &Backend,
    actor: &ActorKey,
    limit: Duration,
) -> Result<Option<Duration>> {
    let started = Instant::now();
    while started.elapsed() < limit {
        let snapshot = backend
            .durable()
            .actor(actor)
            .await
            .map_err(|error| anyhow::anyhow!("read {actor}: {error}"))?;
        if let Some(snapshot) = snapshot
            && snapshot.owner.is_none()
            && snapshot.state != ActorState::Owned
        {
            return Ok(Some(started.elapsed()));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(None)
}

#[derive(Serialize)]
struct ParkedSample {
    case: String,
    dialect: &'static str,
    nodes: usize,
    sample: usize,
    /// Key pinned to the actor's release, when it released.
    release_after_pin_us: Option<u64>,
    /// How long it stayed parked before the resolution.
    parked_us: u64,
    /// Resolution start to the engine seeing it.
    resolve_to_resumed_us: u64,
    /// Resolve acknowledgement to engine resume; observation only, never a gate.
    after_resolve_commit_to_resumed_us: u64,
    /// Resolution start to `process.terminal`.
    resolve_to_terminal_us: u64,
    /// The resolution's own transaction.
    resolve_commit_us: u64,
}

/// A parked process resumed by one external completion: it pins a key and
/// awaits it, its owner releases it, it stays parked `park`, and a host
/// resolves the key through another node's backend when there is one.
pub async fn parked(run: &Run<'_>, case: &Case, park: Duration) -> Result<()> {
    let mut deployment = run.deployment();
    deployment.boot_many(run.nodes).await?;
    let mut resumes = Vec::new();
    let mut wake_latencies = Vec::new();
    for sample in 0..run.samples {
        let token = format!("{}-{sample}", case.name);
        let mut feed = deployment.board.follow(&token);
        let actor = start(deployment.producer(0), &token, 1).await?;
        let (pinned_at, key) = pinned(&mut feed).await?;
        let release = released(deployment.producer(0), &actor, Duration::from_secs(120)).await?;
        let release_after_pin_us = release.map(|_| micros(pinned_at, Instant::now()));
        let parked_from = Instant::now();
        tokio::time::sleep(park).await;
        let terminal = deployment.recorder.watch_terminal(&actor);
        let from = deployment.recorder.now_us();
        let started = Instant::now();
        resolve(deployment.producer(sample + 1), &key).await?;
        let committed = Instant::now();
        let resumed = match next(&mut feed).await? {
            Seen::Resolved(at) => at,
            other => bail!("expected the resolution, saw {other:?}"),
        };
        let done = tokio::time::timeout(LIMIT, terminal)
            .await
            .context("the process did not end")??;
        let resolve_commit_us = deployment
            .recorder
            .transactions_since(from)
            .iter()
            .find(|t| t.label == "wait.resolve")
            .map_or(0, |t| t.micros);
        let record = ParkedSample {
            case: case.name.clone(),
            dialect: run.database.dialect(),
            nodes: run.nodes,
            sample,
            release_after_pin_us,
            parked_us: micros(parked_from, started),
            resolve_to_resumed_us: micros(started, resumed),
            after_resolve_commit_to_resumed_us: micros(committed, resumed),
            resolve_to_terminal_us: micros(started, done),
            resolve_commit_us,
        };
        resumes.push(record.resolve_to_terminal_us);
        wake_latencies.push(record.after_resolve_commit_to_resumed_us);
        run.report.write("parked", &record)?;
    }
    run.report.write(
        "summary",
        &serde_json::json!({
            "case": case.name,
            "dialect": run.database.dialect(),
            "nodes": run.nodes,
            "resolve_to_terminal": Distribution::of(&resumes)?,
            "after_resolve_commit_to_resumed": Distribution::of(&wake_latencies)?,
        }),
    )?;
    deployment.shutdown().await
}

/// A process that awaits `waits` keys, each resolved as soon as it is
/// pinned, while its owner still holds it.
pub async fn hot_waits(run: &Run<'_>, case: &Case, waits: usize) -> Result<()> {
    let mut deployment = run.deployment();
    deployment.boot_many(run.nodes).await?;
    let mut latencies = Vec::new();
    for sample in 0..run.samples {
        let token = format!("{}-{sample}", case.name);
        let mut feed = deployment.board.follow(&token);
        start(deployment.producer(0), &token, waits).await?;
        for wait in 0..waits {
            let (_, key) = pinned(&mut feed).await?;
            let started = Instant::now();
            resolve(deployment.producer(wait + 1), &key).await?;
            let resumed = match next(&mut feed).await? {
                Seen::Resolved(at) => at,
                other => bail!("expected the resolution, saw {other:?}"),
            };
            let micros = micros(started, resumed);
            latencies.push(micros);
            run.report.write(
                "wait",
                &serde_json::json!({
                    "case": case.name, "dialect": run.database.dialect(), "nodes": run.nodes,
                    "sample": sample, "wait": wait, "resolve_to_resumed_us": micros,
                }),
            )?;
        }
    }
    run.report.write(
        "summary",
        &serde_json::json!({
            "case": case.name,
            "dialect": run.database.dialect(),
            "nodes": run.nodes,
            "resolve_to_resumed": Distribution::of(&latencies)?,
        }),
    )?;
    deployment.shutdown().await
}

/// H8: `actors` processes wait on keys no one resolves; after they settle,
/// measure what the deployment does for `window` with nothing to run.
pub async fn idle(run: &Run<'_>, case: &Case, actors: usize, window: Duration) -> Result<()> {
    let mut deployment = run.deployment();
    deployment.boot_many(run.nodes).await?;
    let counters = Counters::open(&run.database).await?;
    let empty = counters.read().await?;
    let mut feeds = Vec::new();
    let mut keys = Vec::new();
    for index in 0..actors {
        let token = format!("{}-{index}", case.name);
        let mut feed = deployment.board.follow(&token);
        let actor = start(deployment.producer(index), &token, 1).await?;
        feeds.push(actor);
        keys.push(pinned(&mut feed).await?.1);
    }
    // Settle: every waiting actor released or evicted as its activation
    // decides, then a quiet window.
    let mut owned = 0;
    for actor in feeds.iter().take(20) {
        if released(deployment.producer(0), actor, Duration::from_secs(90))
            .await?
            .is_none()
        {
            owned += 1;
        }
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let before = counters.read().await?;
    let from = deployment.recorder.now_us();
    tokio::time::sleep(window).await;
    let after = counters.read().await?;
    let transactions = deployment.recorder.transactions_since(from);
    let seconds = window.as_secs_f64();
    let delta = after.since(&before)?;
    run.report.write(
        "idle",
        &serde_json::json!({
            "case": case.name,
            "dialect": run.database.dialect(),
            "nodes": run.nodes,
            "waiting_actors": actors,
            "sampled_still_owned_of_20": owned,
            "window_s": seconds,
            "transactions": by_label(&transactions),
            "durable_transactions_per_s": transactions.iter().filter(|t| t.ok).count() as f64 / seconds,
            "counters": delta,
            "statements_per_s": delta.postgres.map(|pg| pg.all_statements as f64 / seconds),
            "relation_bytes_per_waiting_actor": before.since(&empty)?.postgres
                .filter(|_| actors > 0)
                .map(|pg| pg.relation_bytes as f64 / actors as f64),
            "sqlite_bytes_per_waiting_actor": before.since(&empty)?.sqlite_bytes
                .filter(|_| actors > 0)
                .map(|bytes| bytes as f64 / actors as f64),
            "connections": after.postgres.map(|pg| pg.connections),
        }),
    )?;
    drop(keys);
    counters.close().await;
    deployment.shutdown().await
}

/// Maintained serial/timer/fanout populations, with counted attribution.
pub async fn probe(
    run: &Run<'_>,
    case: &Case,
    mode: crate::process::Mode,
    count: usize,
) -> Result<()> {
    use crate::process::Mode;
    let mut deployment = run.deployment();
    deployment.boot_many(run.nodes).await?;
    let witness = lash_core_ids::perf_witness::Collector::install_with_sql_shapes()?;
    let capture = lash_perf::perf_support::async_operations::Capture::window(
        "one process case, after node readiness and before shutdown",
    )?;
    let from = deployment.recorder.now_us();
    for sample in 0..run.samples {
        let token = format!("{}-{sample}", case.name);
        let mut feed = deployment.board.follow(&token);
        let barrier = (mode == Mode::ReadyFanout).then(|| deployment.board.barrier(&token, count));
        let mut input = payload(&token, count);
        input["mode"] = serde_json::to_value(mode)?;
        let ready = Instant::now();
        let actor = start_payload(deployment.producer(0), &token, input).await?;
        let registered = Instant::now();
        let terminal = deployment.recorder.watch_terminal(&actor);
        let mut starts = Vec::new();
        let mut body_ids = std::collections::BTreeSet::new();
        let mut settled_ids = std::collections::BTreeSet::new();
        let mut settled = Vec::new();
        let mut previous = None;
        if matches!(mode, Mode::Fanout | Mode::ReadyFanout) {
            while starts.len() < count {
                match next(&mut feed).await? {
                    Seen::StepStarted(at, id) => {
                        anyhow::ensure!(body_ids.insert(id), "repeated body");
                        starts.push(at);
                    }
                    Seen::StepSettled(at, id) => {
                        anyhow::ensure!(settled_ids.insert(id), "repeated settlement");
                        settled.push(at);
                    }
                    other => bail!("unexpected fanout event {other:?}"),
                }
            }
            if let Some(barrier) = barrier {
                // Every body has entered; one explicit release readies all bodies.
                tokio::time::timeout(LIMIT, barrier.wait()).await?;
            }
            while settled.len() < count {
                match next(&mut feed).await? {
                    Seen::StepSettled(at, id) => {
                        anyhow::ensure!(settled_ids.insert(id), "repeated settlement");
                        settled.push(at);
                    }
                    other => bail!("unexpected fanout settlement {other:?}"),
                }
            }
        } else {
            for ordinal in 0..count {
                let started = match next(&mut feed).await? {
                    Seen::StepStarted(at, id) if mode == Mode::Steps => {
                        anyhow::ensure!(body_ids.insert(id), "repeated body");
                        at
                    }
                    Seen::TimerDue(at) if mode == Mode::Timers => at,
                    other => bail!("expected body or due, got {other:?}"),
                };
                let done = match next(&mut feed).await? {
                    Seen::StepSettled(at, id) if mode == Mode::Steps => {
                        anyhow::ensure!(settled_ids.insert(id), "repeated settlement");
                        at
                    }
                    Seen::Resolved(at) if mode == Mode::Timers => at,
                    other => bail!("expected settlement, got {other:?}"),
                };
                starts.push(started);
                settled.push(done);
                run.report.write("process_probe", &serde_json::json!({
                    "case": case.name, "dialect": run.database.dialect(), "sample": sample, "ordinal": ordinal,
                    "register_us": micros(ready, registered),
                    "body_or_due_at_us": deployment.recorder.since(started),
                    "engine_settled_at_us": deployment.recorder.since(done),
                    "registered_to_first_body_us": (ordinal == 0 && mode == Mode::Steps).then(|| micros(registered, started)),
                    "previous_engine_settlement_to_body_us": previous.filter(|_| mode == Mode::Steps).map(|at| micros(at, started)),
                    "body_to_engine_settlement_us": (mode == Mode::Steps).then(|| micros(started, done)),
                    "due_to_engine_woke_us": (mode == Mode::Timers).then(|| micros(started, done)),
                }))?;
                previous = Some(done);
            }
        }
        tokio::time::timeout(LIMIT, terminal).await??;
        if mode != Mode::Timers {
            anyhow::ensure!(
                body_ids == settled_ids && body_ids.len() == count,
                "body/settlement identity mismatch"
            );
        }
        let transactions = deployment.recorder.transactions_since(from);
        run.report.write("process_population", &serde_json::json!({
            "case": case.name, "sample": sample, "actor": actor.to_string(),
            "body_entries": if mode == Mode::Timers { 0 } else { starts.len() },
            "body_identities": body_ids, "settlement_identities": settled_ids,
            "settlements": settled.len(), "claims": deployment.recorder.claims_of(&actor).len(),
            "outcome_transactions": transactions.iter().filter(|tx| tx.actor == actor.to_string() && tx.label == lash_durable::CommitLabel::STEP_OUTCOME.as_str() && tx.ok).count(),
            "transactions": transactions.iter().filter(|tx| tx.actor == actor.to_string()).collect::<Vec<_>>(),
        }))?;
    }
    let snapshot = witness.snapshot();
    run.report.write(
        "process_trace",
        &serde_json::json!({
            "case": case.name, "dialect": run.database.dialect(),
            "transactions": deployment.recorder.transactions_since(from),
            "port_calls": deployment.recorder.port_calls(),
            "discoveries": deployment.recorder.discoveries(),
            "sql": snapshot.sql_shapes, "graph_decoded_nodes": snapshot.graph_decoded_nodes,
            "graph_decoded_bytes": snapshot.graph_decoded_bytes,
            "async": capture.finish_receipt(),
        }),
    )?;
    drop(witness);
    deployment.shutdown().await
}
