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
//! Under the fault controller (FIG-4169, `LASH_LOAD_FAULT_CAMPAIGN=1`) the
//! sessions keep their open-loop clocks running until the controller records
//! the end of its campaign, so every fault lands on live traffic. A lost
//! answer is never a terminal: the driver resubmits under the same workflow
//! key and attaches to the invocation Restate already accepted, so every
//! accepted input is read back from its durable outcome.
//!
//! Environment: `LASH_LOAD_WORKLOAD` (checked-in workload name),
//! `LASH_LOAD_TURNS_PER_SESSION` (the minimum under a campaign), optional
//! `LASH_LOAD_SESSIONS` (default: the workload's population),
//! `LASH_LOAD_RUN` (default: a fresh run ID) and `LASH_LOAD_FAULT_CAMPAIGN`,
//! `RESTATE_INGRESS_URL`, `WORKER_CONTROL_URLS` and `WITNESS_DATABASE_URL`.

use anyhow::{Context, Result, ensure};
use lash_restate_postgres_workers_e2e::load::{
    FAULT_CAMPAIGN_ENV, LOAD_WORKFLOW, LoadContext, LoadEvent, LoadRequest, LoadResponse,
    WitnessedOperation, WitnessedPhase, actor_session_id, cron_session_id, record_load_event,
    verify,
};
use lash_restate_postgres_workers_e2e::{env, required_env, witness};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;

/// How long the driver keeps resubmitting, attaching or re-reading one
/// operation through faults before it records the last failure.
fn settle_deadline(load: &LoadContext) -> Duration {
    Duration::from_secs(u64::from(load.workload.spec().drain_timeout_s))
}

/// The pause between two attempts at one operation.
const RETRY_PAUSE: Duration = Duration::from_secs(1);

/// At most this many transient failures are kept in an operation's terminal.
const KEPT_RETRY_ERRORS: usize = 8;

struct Driver {
    run: String,
    load: LoadContext,
    witness: PgPool,
    client: reqwest::Client,
    ingress: String,
    workers: Vec<String>,
    started: Instant,
    collector: Arc<lash_restate_postgres_workers_e2e::load::measurements::Collector>,
    /// `true` once the fault controller ended its campaign; `None` when the
    /// run has no campaign and each session stops after its planned turns.
    campaign: Option<tokio::sync::watch::Receiver<bool>>,
}

/// How one operation reached its answer.
#[derive(Default)]
struct Delivery {
    attempts: u32,
    submit_attempts: u32,
    attach_attempts: u32,
    accepted_ns: Option<u128>,
    invocation_id: Option<String>,
    response_bytes: usize,
    timed_out: bool,
    terminal_failure: bool,
    reattached: bool,
    retried: Vec<String>,
}

impl Delivery {
    fn retry(&mut self, error: String) {
        if self.retried.len() < KEPT_RETRY_ERRORS {
            self.retried.push(error);
        }
    }
}

/// What one HTTP exchange with the ingress answered.
enum Exchange {
    Answer(Box<LoadResponse>),
    /// The workflow ended with a failure the ingress reports again on attach.
    Failed(String),
    /// The workflow already runs under this key: attach to it.
    Accepted,
    /// Attach found no workflow under this key: submit it.
    Unknown,
    /// A failure the fault may have caused: try again.
    Transient(String),
}

impl Driver {
    fn elapsed_ns(&self) -> u128 {
        self.started.elapsed().as_nanos()
    }

    /// Whether the sessions should keep sending past their planned turns.
    fn campaign_running(&self) -> bool {
        self.campaign
            .as_ref()
            .is_some_and(|campaign| !*campaign.borrow())
    }

    /// Resolves once the campaign ended; never, without a campaign.
    async fn campaign_ended(&self) {
        match &self.campaign {
            Some(campaign) => {
                let mut campaign = campaign.clone();
                if campaign.wait_for(|ended| *ended).await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
            None => std::future::pending().await,
        }
    }

    async fn exchange(
        &self,
        request: &LoadRequest,
        attach: bool,
        delivery: &mut Delivery,
    ) -> Exchange {
        let key = request.workflow_key();
        let sent = if attach {
            delivery.attach_attempts += 1;
            self.client
                .get(format!(
                    "{}/restate/workflow/{LOAD_WORKFLOW}/{key}/attach",
                    self.ingress
                ))
                .send()
                .await
        } else {
            delivery.submit_attempts += 1;
            self.client
                .post(format!("{}/{LOAD_WORKFLOW}/{key}/run/send", self.ingress))
                .json(request)
                .send()
                .await
        };
        let response = match sent {
            Ok(response) => response,
            Err(error) => return Exchange::Transient(format!("{error:#}")),
        };
        let status = response.status();
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(error) => return Exchange::Transient(format!("read the answer: {error:#}")),
        };
        let text = || format!("HTTP {status}: {}", String::from_utf8_lossy(&body));
        if status.is_success() && !attach {
            let acknowledgement: Value = match serde_json::from_slice(&body) {
                Ok(value) => value,
                Err(error) => {
                    return Exchange::Failed(format!("undecodable send acknowledgement: {error}"));
                }
            };
            let Some(id) = acknowledgement["invocationId"].as_str() else {
                return Exchange::Failed("send acknowledgement has no invocation ID".to_owned());
            };
            if delivery
                .invocation_id
                .as_deref()
                .is_some_and(|previous| previous != id)
            {
                return Exchange::Failed(
                    "resubmission changed the accepted invocation ID".to_owned(),
                );
            }
            delivery
                .accepted_ns
                .get_or_insert_with(|| self.elapsed_ns());
            delivery.invocation_id = Some(id.to_owned());
            delivery.reattached |= acknowledgement["status"] == "PreviouslyAccepted";
            return Exchange::Accepted;
        }
        if attach {
            delivery.response_bytes = body.len();
        }
        if status.is_success() {
            return match serde_json::from_slice::<LoadResponse>(&body) {
                Ok(response) => Exchange::Answer(Box::new(response)),
                Err(error) => Exchange::Failed(format!("undecodable answer ({error}): {}", text())),
            };
        }
        match status {
            reqwest::StatusCode::NOT_FOUND if attach => Exchange::Unknown,
            // The handler's terminal failure, which the workflow keeps: it
            // counts only once an attach reads it back from the invocation.
            reqwest::StatusCode::INTERNAL_SERVER_ERROR if attach => {
                delivery.terminal_failure = true;
                Exchange::Failed(text())
            }
            _ => Exchange::Transient(text()),
        }
    }

    /// Submit `request` and read its durable answer, resubmitting and
    /// attaching through faults until it answers or the settle deadline.
    async fn settle(&self, request: &LoadRequest) -> (Result<LoadResponse, String>, Delivery) {
        let deadline = Instant::now() + settle_deadline(&self.load);
        let mut delivery = Delivery::default();
        let mut attach = false;
        loop {
            delivery.attempts += 1;
            delivery.reattached |= attach;
            let retry = match self.exchange(request, attach, &mut delivery).await {
                Exchange::Answer(response) => return (Ok(*response), delivery),
                Exchange::Failed(error) => return (Err(error), delivery),
                Exchange::Accepted => {
                    attach = true;
                    continue;
                }
                Exchange::Unknown => {
                    attach = false;
                    "attach found no workflow".to_owned()
                }
                Exchange::Transient(error) => error,
            };
            if Instant::now() >= deadline {
                delivery.timed_out = true;
                return (
                    Err(format!(
                        "no answer within {:?}; last: {retry}",
                        settle_deadline(&self.load)
                    )),
                    delivery,
                );
            }
            delivery.retry(retry);
            tokio::time::sleep(RETRY_PAUSE).await;
        }
    }

    fn subject(&self, request: &LoadRequest) -> Result<String> {
        Ok(match request {
            LoadRequest::Behaviors { run, .. } => format!("{run}/behaviors"),
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
        let (answer, delivery) = self.settle(&request).await;
        let accepted_ns = delivery.accepted_ns;
        let invocation_id = &delivery.invocation_id;
        let terminal_ns = self.elapsed_ns();
        let response = answer.as_ref().ok().map(serde_json::to_value).transpose()?;
        let outcome = match &answer {
            Ok(_) => response
                .as_ref()
                .and_then(|value| value["outcome"]["status"].as_str())
                .unwrap_or("completed")
                .to_owned(),
            Err(_) if delivery.timed_out => "timeout".into(),
            Err(_) if delivery.terminal_failure => "failed".into(),
            Err(_) => "client_error".into(),
        };
        // Read each completed outer invocation before retention can prune it.
        let (journal, journal_error) = if let Some(id) = invocation_id {
            match self.collector.journal(id).await {
                Ok(journal) => (Some(journal), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            }
        } else {
            (None, None)
        };
        let actor = match &request {
            LoadRequest::Turn { actor, .. } => Some(*actor),
            _ => None,
        };
        let session_id = match &request {
            LoadRequest::Turn { session_id, .. }
            | LoadRequest::CronSetup { session_id, .. }
            | LoadRequest::DeleteSession { session_id, .. } => session_id.clone(),
            LoadRequest::CronTick { run, .. } => cron_session_id(run),
            LoadRequest::Behaviors { run, .. } => format!("load-{run}-behaviors"),
        };
        lash_restate_postgres_workers_e2e::load::measurements::emit(&json!({
            "schema_version": 1, "record": "operation", "run": self.run,
            "id": request.workflow_key(), "invocation_id": invocation_id,
            "subject_id": subject,
            "input_id": lash_restate_postgres_workers_e2e::load::turn_id_for(&subject),
            "actor_id": actor, "session_id": session_id,
            "request": request, "scenario": operation.as_str(),
            "phase": if self.campaign.is_some() { "fault_campaign" } else { "smoke" },
            "scheduled_ns": scheduled_ns, "sent_ns": sent_ns, "accepted_ns": accepted_ns,
            "first_visible_ns": null, "settled_ns": null, "observed_ns": terminal_ns,
            "unavailable": {"first_visible_ns": "workflow attach exposes the terminal only",
                "settled_ns": "remote settlement clock is not synchronized with the client"},
            "outcome": outcome, "error": answer.as_ref().err().map(|error| format!("{error:#}")),
            "root_id": response.as_ref().and_then(|value| value["outcome"]["root"].as_str()),
            "client_attempts": delivery.submit_attempts, "client_reattaches": delivery.attach_attempts.saturating_sub(1),
            "request_bytes": serde_json::to_vec(&request)?.len(), "response_bytes": delivery.response_bytes,
            "response": response, "journal": journal, "journal_error": journal_error, "fault_ids": [],
        }))?;
        let detail = match &answer {
            Ok(response) => json!({
                "response": response,
                "attempts": delivery.attempts, "retried": delivery.retried,
                "reattached": delivery.reattached,
                "terminal_ns": terminal_ns.to_string(),
            }),
            Err(error) => json!({
                "error": format!("{error:#}"),
                "attempts": delivery.attempts, "retried": delivery.retried,
                "reattached": delivery.reattached,
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
            Ok(response) => {
                if let LoadResponse::Turn(report) = &response {
                    println!(
                        "load turn operation={} status={:?} credits_input={} root={}",
                        report.operation,
                        report.outcome.status,
                        report.outcome.credits_input(&report.operation),
                        report.outcome.root.as_deref().unwrap_or("none")
                    );
                }
                Ok(Some(response))
            }
            Err(error) => {
                eprintln!("load operation {subject} failed: {error}");
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
                // A read that never answered recorded nothing: a worker the
                // fault took down is read again once it serves.
                let deadline = Instant::now() + settle_deadline(&self.load);
                loop {
                    let answered = self
                        .client
                        .get(format!(
                            "{worker}/load/attachments/{session_id}/{attachment_id}"
                        ))
                        .query(&[
                            ("run", self.run.as_str()),
                            ("blob_key", blob.blob_key.as_str()),
                        ])
                        .send()
                        .await;
                    let failure = match answered {
                        Ok(response) if response.status().is_success() => {
                            let body: Value = response.json().await.unwrap_or(Value::Null);
                            if body["committed"] != Value::Bool(true) {
                                eprintln!(
                                    "load read of {} in {session_id} from {worker} answered {body}",
                                    blob.blob_key
                                );
                            }
                            break;
                        }
                        Ok(response) => format!(
                            "HTTP {}: {}",
                            response.status(),
                            response.text().await.unwrap_or_default()
                        ),
                        Err(error) => format!("{error:#}"),
                    };
                    if Instant::now() >= deadline {
                        eprintln!(
                            "load read of {} in {session_id} from {worker} failed: {failure}",
                            blob.blob_key
                        );
                        break;
                    }
                    tokio::time::sleep(RETRY_PAUSE).await;
                }
            }
        }
        Ok(())
    }
}

/// One session's open-loop clock: turns start on their scheduled arrival
/// whatever earlier turns are doing; a retired session is deleted once every
/// turn sent to it has ended. The clock runs `turns` turns, and under a fault
/// campaign keeps running until the campaign ends.
async fn run_actor(driver: Arc<Driver>, actor: u64, turns: u64) -> Result<()> {
    let generator = driver.load.generator(&driver.run)?;
    let rate = driver.load.workload.spec().turns_per_session_s;
    let mut scheduled_s = 0.0;
    let mut generation = 0;
    let mut in_flight = JoinSet::new();
    let mut deletes = JoinSet::new();
    let mut active_session = None;
    for ordinal in 0.. {
        let past_plan = ordinal >= turns;
        if past_plan && !driver.campaign_running() {
            break;
        }
        let plan = generator.plan(actor, ordinal)?;
        scheduled_s += generator.arrival_gap_s(actor, ordinal, rate)?;
        let scheduled = Duration::from_secs_f64(scheduled_s);
        let arrival =
            tokio::time::sleep_until(tokio::time::Instant::from_std(driver.started + scheduled));
        if past_plan {
            tokio::select! {
                () = arrival => {}
                () = driver.campaign_ended() => break,
            }
        } else {
            arrival.await;
        }
        let session_id = actor_session_id(&driver.run, actor, generation);
        active_session = Some(session_id.clone());
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
            active_session = None;
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
    if let Some(session_id) = active_session {
        driver
            .submit(
                LoadRequest::DeleteSession {
                    run: driver.run.clone(),
                    session_id,
                },
                driver.elapsed_ns(),
                false,
            )
            .await?;
    }
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

/// Flip `ended` once the fault controller records the end of its campaign
/// for `run`, completed or failed.
async fn watch_campaign(
    witness: PgPool,
    run: String,
    ended: tokio::sync::watch::Sender<bool>,
    started: Instant,
) -> Result<()> {
    let mut last_event = 0_i64;
    loop {
        let before = started.elapsed().as_nanos();
        let row: Option<(i64, String, String, i64)> = sqlx::query_as(
            "SELECT fault_event_id, kind, phase, witness_clock_us() FROM witness_load_faults
             WHERE run_id = $1 AND fault_event_id > $2 ORDER BY fault_event_id LIMIT 1",
        )
        .bind(&run)
        .bind(last_event)
        .fetch_optional(&witness)
        .await
        .context("read the fault event and witness clock")?;
        let after = started.elapsed().as_nanos();
        if let Some((event, kind, phase, witness_us)) = row {
            lash_restate_postgres_workers_e2e::load::measurements::emit(&json!({
                "schema_version": 1, "record": "clock_anchor", "run": run,
                "id": event.to_string(), "clock": "witness_postgres_us",
                "witness_us": witness_us, "monotonic_ns": before + (after - before) / 2,
                "round_trip_ns": after - before,
            }))?;
            last_event = event;
            if kind == "campaign" && (phase == "complete" || phase == "failed") {
                println!("load fault campaign ended run={run} phase={phase}");
                ended.send_replace(true);
                return Ok(());
            }
            continue;
        }
        tokio::time::sleep(RETRY_PAUSE).await;
    }
}

/// The message count of a receipted model request, and its last `count`
/// messages, each cut to its last 1500 bytes.
fn request_tail(request: &[u8], count: usize) -> (usize, String) {
    let request: Value = serde_json::from_slice(request).unwrap_or(Value::Null);
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let tail: Vec<String> = messages
        .iter()
        .rev()
        .take(count)
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
    (messages.len(), tail.join(" | "))
}

/// The first feedback in a receipted model request that reports a failed
/// call: where a cell that keeps failing first went wrong.
fn first_failure(request: &[u8]) -> Option<String> {
    let request: Value = serde_json::from_slice(request).ok()?;
    request["messages"].as_array()?.iter().find_map(|message| {
        let text = message["content"].to_string();
        let start = text.find("Calls:")?;
        text[start..].contains("→ err").then(|| {
            let end = (start + 2000).min(text.len());
            let end = (end..text.len())
                .find(|index| text.is_char_boundary(*index))
                .unwrap_or(text.len());
            text[start..end].to_owned()
        })
    })
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
        let (messages, tail) = request_tail(last, 2);
        println!(
            "load diagnosis key={key} receipts={} messages={messages} first_failure={} tail={tail}",
            requests.len(),
            first_failure(last).unwrap_or_default(),
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let measurement_path = std::path::PathBuf::from(required_env("LASH_LOAD_MEASUREMENTS_PATH")?);
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&measurement_path)
        .context("create the fresh run's measurement file")?;
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
    let campaign = match env(FAULT_CAMPAIGN_ENV, "0").as_str() {
        "0" => false,
        "1" => true,
        other => anyhow::bail!("{FAULT_CAMPAIGN_ENV} must be 0 or 1, not `{other}`"),
    };
    let workers: Vec<String> = required_env("WORKER_CONTROL_URLS")?
        .split(',')
        .map(str::to_owned)
        .collect();
    ensure!(!workers.is_empty(), "WORKER_CONTROL_URLS names no worker");
    let witness = witness::connect_witness().await?;
    let started = Instant::now();
    let before = started.elapsed().as_nanos();
    let witness_us: i64 = sqlx::query_scalar("SELECT witness_clock_us()")
        .fetch_one(&witness)
        .await?;
    let after = started.elapsed().as_nanos();
    lash_restate_postgres_workers_e2e::load::measurements::emit(&json!({
        "schema_version": 1, "record": "clock_anchor", "run": run,
        "id": "campaign-start", "clock": "witness_postgres_us",
        "witness_us": witness_us, "monotonic_ns": before + (after - before) / 2,
        "round_trip_ns": after - before,
    }))?;
    let (campaign, campaign_watch) = if campaign {
        let (ended, watch) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(watch_campaign(witness.clone(), run.clone(), ended, started));
        (Some(watch), Some(task))
    } else {
        (None, None)
    };
    let collector = Arc::new(
        lash_restate_postgres_workers_e2e::load::measurements::Collector {
            run: run.clone(),
            workers: workers.clone(),
            nodes: required_env("RESTATE_METRICS_URLS")?
                .split(',')
                .map(str::to_owned)
                .collect(),
            admin: required_env("RESTATE_ADMIN_URL")?,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            database: sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&required_env("DATABASE_URL")?)
                .await?,
            started,
            initial_invocations: tokio::sync::Mutex::new(None),
        },
    );
    let journal_settings = collector
        .retain_journals(&required_env("LASH_LOAD_JOURNAL_RETENTION")?)
        .await?;
    lash_restate_postgres_workers_e2e::load::measurements::emit(&json!({
        "schema_version": 1, "record": "run", "run": run, "mode": if campaign.is_some() { "fault_campaign" } else { "smoke" },
        "workload": workload_name, "workload_sha256": load.sha256(),
        "seed": load.workload.spec().seed, "sessions": sessions, "turns_per_session": turns,
        "workers": workers, "fault_campaign": campaign.is_some(), "pool": "PENDING", "pool_dependencies": ["FIG-4161", "FIG-4162"],
        "invocation_scope": "isolated topology, one driver; census before admission",
        "journal_settings": journal_settings,
    }))?;
    lash_restate_postgres_workers_e2e::load::measurements::emit(&collector.sample().await?)?;
    let collection_stop = Arc::new(tokio::sync::Notify::new());
    let task_stop = Arc::clone(&collection_stop);
    let task_collector = Arc::clone(&collector);
    let scrape_s = load.workload.spec().collection.scrape_s;
    let collection = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = task_stop.notified() => break,
                _ = tokio::time::sleep(Duration::from_secs(u64::from(scrape_s))) => {
                    let sample = match task_collector.sample().await {
                        Ok(sample) => sample,
                        Err(error) => json!({"schema_version": 1, "record": "sample_error",
                            "run": task_collector.run, "monotonic_ns": task_collector.started.elapsed().as_nanos(),
                            "error": format!("{error:#}")}),
                    };
                    lash_restate_postgres_workers_e2e::load::measurements::emit(&sample)?;
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    });
    let driver = Arc::new(Driver {
        run: run.clone(),
        witness,
        client: reqwest::Client::builder()
            .timeout(settle_deadline(&load))
            .build()?,
        ingress: required_env("RESTATE_INGRESS_URL")?,
        workers,
        started,
        campaign,
        load,
        collector,
    });
    println!(
        "load run={run} workload={workload_name} sha256={} sessions={sessions} turns_per_session={turns} fault_campaign={}",
        driver.load.sha256(),
        driver.campaign.is_some()
    );

    driver
        .submit(
            LoadRequest::Behaviors {
                workload_sha256: driver.load.sha256().to_owned(),
                run: run.clone(),
            },
            0,
            false,
        )
        .await?;
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
    collection_stop.notify_one();
    collection.await??;
    if let Some(task) = campaign_watch {
        task.await.context("join fault clock collection")??;
    }
    let snapshot = verify::load_snapshot(&driver.witness, &run).await?;
    let verdict = verify::verify(&driver.load, &run, &snapshot)?;
    for line in verdict.lines() {
        println!("{line}");
    }
    diagnose(&driver.witness, &verdict).await?;
    lash_restate_postgres_workers_e2e::load::measurements::retain_witness(&driver.witness, &run)
        .await?;
    lash_restate_postgres_workers_e2e::load::measurements::emit(&json!({
        "schema_version": 1, "record": "witness", "run": run, "verdict": verdict,
        "fault_rows": snapshot.faults.len(),
        "sent": snapshot.events.iter().filter(|event| event.phase == "sent").count(),
        "terminal": snapshot.events.iter().filter(|event| event.phase == "terminal").count(),
        "provider_calls": snapshot.receipts.len(), "effect_attempts": snapshot.attempts.len(),
        "provider_retryable_failures": snapshot.receipts.iter().filter(|(_, scenario)| scenario == "load_retryable").count(),
        "effect_commits": snapshot.commits.len(),
    }))?;
    for line in
        lash_restate_postgres_workers_e2e::load::red_side::checks(&driver.load, &run, &snapshot)?
    {
        println!("{line}");
    }

    println!("load witness summary {}", serde_json::to_string(&verdict)?);
    // Recovery requires durable operation and fault evidence, independently
    // of the final internal invocation census. Archive it before that query.
    std::fs::write(
        measurement_path.with_extension("recovery-ready"),
        b"ready\n",
    )?;
    let recovery_collected = measurement_path.with_extension("recovery-collected");
    let recovery_deadline = Instant::now()
        + Duration::from_secs(u64::from(driver.load.workload.spec().drain_timeout_s));
    while !recovery_collected.exists() {
        ensure!(
            Instant::now() < recovery_deadline,
            "controller did not collect recovery evidence"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Public terminals can precede their asynchronous cleanup. Retain the
    // final census after that work settles, bounded to keep this a short smoke.
    let drain_deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let sample = match driver.collector.sample().await {
            Ok(sample) => sample,
            Err(error) => {
                // Retain the gap and the independent witness. Measurement
                // reconciliation rejects sample_error records.
                lash_restate_postgres_workers_e2e::load::measurements::emit(&json!({
                    "schema_version": 1, "record": "sample_error", "run": run,
                    "monotonic_ns": driver.elapsed_ns(), "error": format!("{error:#}"),
                }))?;
                break;
            }
        };
        let owned: std::collections::BTreeSet<_> = sample["journals"]
            .as_array()
            .context("sample has no journals")?
            .iter()
            .filter_map(|row| row["id"].as_str())
            .collect();
        let unfinished = sample["invocations"]
            .as_array()
            .context("sample has no invocations")?
            .iter()
            .filter(|row| {
                owned.contains(row["id"].as_str().unwrap_or_default())
                    && row["status"] != "completed"
            })
            .count();
        lash_restate_postgres_workers_e2e::load::measurements::emit(&sample)?;
        if unfinished == 0 || Instant::now() >= drain_deadline {
            println!(
                "load drained run={run} elapsed_s={:.1} unfinished_internal={unfinished}",
                driver.started.elapsed().as_secs_f64()
            );
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    // The controller copies this file while the container is still alive.
    // Kubernetes only exposes the current rotated log, so stdout cannot be
    // the raw result archive. The acknowledgement ends this foreground Job.
    std::fs::write(measurement_path.with_extension("complete"), b"complete\n")?;
    let collected = measurement_path.with_extension("collected");
    let deadline = Instant::now()
        + Duration::from_secs(u64::from(driver.load.workload.spec().drain_timeout_s));
    while !collected.exists() {
        ensure!(
            Instant::now() < deadline,
            "controller did not collect the measurement file"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    ensure!(
        verdict.passed(),
        "load witness failed with {} violations",
        verdict.violations()
    );
    Ok(())
}
