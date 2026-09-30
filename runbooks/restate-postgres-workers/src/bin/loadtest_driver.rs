//! The FIG-3790 durable load driver (lane L3, FIG-4168).
//!
//! Replays a checked-in synthetic workload against a running topology through
//! Restate ingress: every session is an open-loop Poisson clock of primary
//! turns, the cron schedules emit on their seeded phases, retired sessions are
//! deleted once their turns end, and every answered blob is read back through
//! the workers. The driver witnesses each operation it sends and the typed
//! terminal it reads back, then reconciles the run's witness ledgers and
//! exits non-zero unless every evidence class passed.
//!
//! Environment: `LASH_LOAD_WORKLOAD` (checked-in workload name),
//! `LASH_LOAD_TURNS_PER_SESSION`, optional `LASH_LOAD_SESSIONS` (default: the
//! workload's population) and `LASH_LOAD_RUN` (default: a fresh run ID),
//! `RESTATE_INGRESS_URL`, `WORKER_CONTROL_URLS` and `WITNESS_DATABASE_URL`.

use anyhow::{Context, Result, ensure};
use lash_restate_postgres_workers_e2e::load::{
    LOAD_WORKFLOW, LoadContext, LoadEvent, LoadRequest, LoadResponse, WitnessedOperation,
    WitnessedPhase, actor_session_id, cron_session_id, record_load_event, verify,
};
use lash_restate_postgres_workers_e2e::{env, required_env, witness};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;

struct Driver {
    run: String,
    load: LoadContext,
    witness: PgPool,
    client: reqwest::Client,
    ingress: String,
    workers: Vec<String>,
    started: Instant,
}

impl Driver {
    fn elapsed_ns(&self) -> u128 {
        self.started.elapsed().as_nanos()
    }

    fn subject(&self, request: &LoadRequest) -> Result<String> {
        Ok(match request {
            LoadRequest::Turn { actor, ordinal, .. } => self
                .load
                .generator(&self.run)?
                .operation(*actor, *ordinal)
                .key(),
            LoadRequest::CronSetup { .. } => self.load.generator(&self.run)?.cron_setup_key(),
            LoadRequest::CronTick {
                subscription, tick, ..
            } => self
                .load
                .generator(&self.run)?
                .cron_tick_key(*subscription, *tick),
            LoadRequest::DeleteSession { session_id, .. } => session_id.clone(),
        })
    }

    /// Send one operation, witnessing the send and the typed terminal (or
    /// the failure) it read back.
    async fn submit(
        &self,
        request: LoadRequest,
        scheduled_ns: u128,
        after_delete: bool,
    ) -> Result<Option<LoadResponse>> {
        let subject = self.subject(&request)?;
        let operation = WitnessedOperation::of(&request);
        let sent_ns = self.elapsed_ns();
        record_load_event(
            &self.witness,
            LoadEvent {
                run: &self.run,
                subject: &subject,
                operation,
                phase: WitnessedPhase::Sent,
                observer: "driver",
                detail: &json!({
                    "request": request,
                    "scheduled_ns": scheduled_ns.to_string(),
                    "sent_ns": sent_ns.to_string(),
                    "after_delete": after_delete,
                }),
                content: None,
            },
        )
        .await?;
        let mut reattached = false;
        let answer = async {
            let key = request.workflow_key();
            let response = self
                .client
                .post(format!("{}/{LOAD_WORKFLOW}/{key}/run", self.ingress))
                .json(&request)
                .send()
                .await?;
            let mut status = response.status();
            let mut body = response.bytes().await?;
            // The workflow already runs under this key (a submission the
            // ingress accepted before its answer was lost): attach to it.
            if status == reqwest::StatusCode::CONFLICT {
                reattached = true;
                let response = self
                    .client
                    .get(format!(
                        "{}/restate/workflow/{LOAD_WORKFLOW}/{key}/attach",
                        self.ingress
                    ))
                    .send()
                    .await?;
                status = response.status();
                body = response.bytes().await?;
            }
            ensure!(
                status.is_success(),
                "HTTP {status}: {}",
                String::from_utf8_lossy(&body)
            );
            Ok::<_, anyhow::Error>(serde_json::from_slice::<LoadResponse>(&body)?)
        }
        .await;
        let terminal_ns = self.elapsed_ns();
        let detail = match &answer {
            Ok(response) => json!({
                "response": response,
                "reattached": reattached,
                "terminal_ns": terminal_ns.to_string(),
            }),
            Err(error) => json!({
                "error": format!("{error:#}"),
                "reattached": reattached,
                "terminal_ns": terminal_ns.to_string(),
            }),
        };
        record_load_event(
            &self.witness,
            LoadEvent {
                run: &self.run,
                subject: &subject,
                operation,
                phase: WitnessedPhase::Terminal,
                observer: "driver",
                detail: &detail,
                content: None,
            },
        )
        .await?;
        match answer {
            Ok(response) => Ok(Some(response)),
            Err(error) => {
                eprintln!("load operation {subject} failed: {error:#}");
                Ok(None)
            }
        }
    }

    /// Read every blob of an answered turn back `explicit_reads` times,
    /// rotating across the workers so peers read what another worker put.
    async fn read_blobs(&self, actor: u64, ordinal: u64, session_id: &str) -> Result<()> {
        let generator = self.load.generator(&self.run)?;
        let plan = generator.plan(actor, ordinal)?;
        let reads = self
            .load
            .workload
            .spec()
            .attachments
            .explicit_reads_per_blob;
        for index in 0..plan.attachments.len() {
            let blob = generator.attachment(&plan, index)?;
            let attachment_id = lash::attachments::content_id(&blob.bytes);
            for read in 0..reads {
                let worker =
                    &self.workers[(actor as usize + index + read as usize) % self.workers.len()];
                let response = self
                    .client
                    .get(format!(
                        "{worker}/load/attachments/{session_id}/{attachment_id}"
                    ))
                    .query(&[
                        ("run", self.run.as_str()),
                        ("blob_key", blob.blob_key.as_str()),
                    ])
                    .send()
                    .await?;
                let status = response.status();
                let body: Value = response.json().await.unwrap_or(Value::Null);
                if !status.is_success() || body["committed"] != Value::Bool(true) {
                    eprintln!(
                        "load read of {} in {session_id} from {worker} answered {status}: {body}",
                        blob.blob_key
                    );
                }
            }
        }
        Ok(())
    }
}

/// One session's open-loop clock: turns start on their scheduled arrival
/// whatever earlier turns are doing; a retired session is deleted once every
/// turn sent to it has ended.
async fn run_actor(driver: Arc<Driver>, actor: u64, turns: u64) -> Result<()> {
    let generator = driver.load.generator(&driver.run)?;
    let rate = driver.load.workload.spec().turns_per_session_s;
    let mut scheduled_s = 0.0;
    let mut generation = 0;
    let mut in_flight = JoinSet::new();
    let mut deletes = JoinSet::new();
    for ordinal in 0..turns {
        let plan = generator.plan(actor, ordinal)?;
        scheduled_s += generator.arrival_gap_s(actor, ordinal, rate)?;
        let scheduled = Duration::from_secs_f64(scheduled_s);
        tokio::time::sleep_until(tokio::time::Instant::from_std(driver.started + scheduled)).await;
        let session_id = actor_session_id(&driver.run, actor, generation);
        let request = LoadRequest::Turn {
            workload_sha256: driver.load.sha256().to_owned(),
            run: driver.run.clone(),
            actor,
            ordinal,
            session_id: session_id.clone(),
        };
        let task_driver = Arc::clone(&driver);
        let task_session_id = session_id.clone();
        in_flight.spawn(async move {
            let response = task_driver
                .submit(request, scheduled.as_nanos(), false)
                .await?;
            if let Some(LoadResponse::Turn(report)) = response
                && report.outcome.status
                    == lash_restate_postgres_workers_e2e::load::ReportedStatus::Answered
            {
                task_driver
                    .read_blobs(actor, ordinal, &task_session_id)
                    .await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        if plan.delete || plan.rotate {
            let retired = std::mem::take(&mut in_flight);
            let task_driver = Arc::clone(&driver);
            deletes.spawn(async move {
                retired
                    .join_all()
                    .await
                    .into_iter()
                    .collect::<Result<()>>()?;
                task_driver
                    .submit(
                        LoadRequest::DeleteSession {
                            run: task_driver.run.clone(),
                            session_id,
                        },
                        task_driver.elapsed_ns(),
                        false,
                    )
                    .await?;
                Ok::<_, anyhow::Error>(())
            });
            generation += 1;
        }
    }
    in_flight
        .join_all()
        .await
        .into_iter()
        .collect::<Result<()>>()?;
    deletes
        .join_all()
        .await
        .into_iter()
        .collect::<Result<()>>()?;
    Ok(())
}

/// Every schedule emits on its seeded phase, then every cadence, until the
/// sessions finish. Answers the first tick index no schedule has used.
async fn run_cron(driver: Arc<Driver>, stop: Arc<tokio::sync::Notify>) -> Result<u64> {
    let generator = driver.load.generator(&driver.run)?;
    let cron = &driver.load.workload.spec().cron;
    let cadence = Duration::from_secs(u64::from(cron.cadence_s));
    let mut due: Vec<(Duration, u64, u64)> = (0..u64::from(cron.subscriptions))
        .map(|subscription| {
            (
                Duration::from_secs_f64(generator.cron_phase_s(subscription)),
                subscription,
                0,
            )
        })
        .collect();
    let mut ticks = JoinSet::new();
    loop {
        due.sort();
        let (at, subscription, tick) = due[0];
        tokio::select! {
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(driver.started + at)) => {}
            () = stop.notified() => break,
        }
        let task_driver = Arc::clone(&driver);
        ticks.spawn(async move {
            task_driver
                .submit(
                    LoadRequest::CronTick {
                        workload_sha256: task_driver.load.sha256().to_owned(),
                        run: task_driver.run.clone(),
                        subscription,
                        tick,
                    },
                    at.as_nanos(),
                    false,
                )
                .await
        });
        due[0] = (at + cadence, subscription, tick + 1);
    }
    for result in ticks.join_all().await {
        result?;
    }
    Ok(due.iter().map(|(_, _, tick)| *tick).max().unwrap_or(0))
}

/// For every operation a violation names, print the tail of the last model
/// request the provider receipted for it: a failed cell's error feedback.
async fn diagnose(witness: &PgPool, verdict: &verify::Verdict) -> Result<()> {
    let mut keys = std::collections::BTreeSet::new();
    for tally in verdict.classes.values() {
        for violation in &tally.violations {
            if let Some(key) = violation.split('`').nth(1) {
                keys.insert(key.to_owned());
            }
        }
    }
    for key in keys {
        let requests: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT request_bytes FROM witness_provider_receipts
             WHERE workflow_id = $1 ORDER BY receipt_id",
        )
        .bind(&key)
        .fetch_all(witness)
        .await
        .context("read the receipts of a violated operation")?;
        let Some(last) = requests.last() else {
            println!("load diagnosis key={key} receipts=0");
            continue;
        };
        let request: Value = serde_json::from_slice(last).unwrap_or(Value::Null);
        let messages = request["messages"].as_array().cloned().unwrap_or_default();
        let tail: Vec<String> = messages
            .iter()
            .rev()
            .take(2)
            .rev()
            .map(|message| {
                let text = message["content"].to_string();
                let start = text.len().saturating_sub(1500);
                let start = (start..text.len())
                    .find(|index| text.is_char_boundary(*index))
                    .unwrap_or(text.len());
                format!("{}: {}", message["role"], &text[start..])
            })
            .collect();
        println!(
            "load diagnosis key={key} receipts={} messages={} tail={}",
            requests.len(),
            messages.len(),
            tail.join(" | ")
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let workload_name = required_env(lash_restate_postgres_workers_e2e::load::LOAD_WORKLOAD_ENV)?;
    let load = LoadContext::named(&workload_name)?;
    let run = env(
        "LASH_LOAD_RUN",
        &format!(
            "{workload_name}-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ),
    );
    let sessions: u64 = env(
        "LASH_LOAD_SESSIONS",
        &load.workload.spec().sessions.to_string(),
    )
    .parse()
    .context("parse LASH_LOAD_SESSIONS")?;
    ensure!(
        sessions <= u64::from(load.workload.spec().sessions),
        "the workload plans {} sessions",
        load.workload.spec().sessions
    );
    let turns: u64 = required_env("LASH_LOAD_TURNS_PER_SESSION")?
        .parse()
        .context("parse LASH_LOAD_TURNS_PER_SESSION")?;
    let workers: Vec<String> = required_env("WORKER_CONTROL_URLS")?
        .split(',')
        .map(str::to_owned)
        .collect();
    ensure!(!workers.is_empty(), "WORKER_CONTROL_URLS names no worker");
    let driver = Arc::new(Driver {
        run: run.clone(),
        witness: witness::connect_witness().await?,
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(u64::from(
                load.workload.spec().drain_timeout_s,
            )))
            .build()?,
        ingress: required_env("RESTATE_INGRESS_URL")?,
        workers,
        started: Instant::now(),
        load,
    });
    println!(
        "load run={run} workload={workload_name} sha256={} sessions={sessions} turns_per_session={turns}",
        driver.load.sha256()
    );

    let cron_session = cron_session_id(&run);
    driver
        .submit(
            LoadRequest::CronSetup {
                workload_sha256: driver.load.sha256().to_owned(),
                run: run.clone(),
                session_id: cron_session.clone(),
            },
            0,
            false,
        )
        .await?;
    let stop = Arc::new(tokio::sync::Notify::new());
    let cron = tokio::spawn(run_cron(Arc::clone(&driver), Arc::clone(&stop)));
    let mut actors = JoinSet::new();
    for actor in 0..sessions {
        actors.spawn(run_actor(Arc::clone(&driver), actor, turns));
    }
    for result in actors.join_all().await {
        result?;
    }
    // A stored permit ends the ticker even if it is not waiting yet.
    stop.notify_one();
    let next_tick = cron.await??;
    // Deleting the owner ends its subscriptions: one more emission per
    // schedule must start nothing.
    driver
        .submit(
            LoadRequest::DeleteSession {
                run: run.clone(),
                session_id: cron_session,
            },
            driver.elapsed_ns(),
            false,
        )
        .await?;
    for subscription in 0..u64::from(driver.load.workload.spec().cron.subscriptions) {
        driver
            .submit(
                LoadRequest::CronTick {
                    workload_sha256: driver.load.sha256().to_owned(),
                    run: run.clone(),
                    subscription,
                    tick: next_tick,
                },
                driver.elapsed_ns(),
                true,
            )
            .await?;
    }
    println!(
        "load drained run={run} elapsed_s={:.1}",
        driver.started.elapsed().as_secs_f64()
    );

    let snapshot = verify::load_snapshot(&driver.witness, &run).await?;
    let verdict = verify::verify(&driver.load, &run, &snapshot)?;
    for line in verdict.lines() {
        println!("{line}");
    }
    diagnose(&driver.witness, &verdict).await?;
    println!("load witness summary {}", serde_json::to_string(&verdict)?);
    ensure!(
        verdict.passed(),
        "load witness failed with {} violations",
        verdict.violations()
    );
    Ok(())
}
