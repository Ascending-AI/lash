//! FIG-4988: the real agent-workbench over its production OpenAI-compatible
//! HTTP transport. The recorded provider fixture binds each request body to
//! the scenario's facts; Restate journal, API, trace and SQLite evidence come
//! from the browser-side oracle and the checks below.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, ensure};
use futures_util::StreamExt as _;
use lash::provider::ProviderFailureKind;
use lash_core::RuntimeEffectOutcome;
use lash_remote_protocol::RemoteSessionObservationEvent;
use lash_restate::EFFECT_JOURNAL_VERSION;
use lash_upgrade_harness::e2e::case::{ArtifactIdentity, CaseLease, Leg, Permutation, StoreKind};
use lash_upgrade_harness::e2e::control::ProcessReceipt;
use lash_upgrade_harness::e2e::evidence::Evidence;
use lash_upgrade_harness::e2e::host::{HostAdapter as _, HostCommand};
use lash_upgrade_harness::e2e::host_adapters::workbench::WorkbenchHost;
use lash_upgrade_harness::e2e::provider_http::transcript::{HttpOccurrence, HttpTranscript};
use lash_upgrade_harness::e2e::provider_http::{HttpReceipt, RecordedHttpFixture, TransportEvent};
use lash_upgrade_harness::restate_view::RestateView;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::Command;
use tokio::time::{sleep, timeout};

use super::{artifact, required, turn_journals, write_case_receipt};

pub struct Scenario {
    /// Lease, artifact and scorecard identity.
    pub id: &'static str,
    /// `AGENT_WORKBENCH_PROTOCOL` for the booted workbench.
    pub protocol: &'static str,
    pub transcript: &'static [u8],
    /// Relates each request body to the occurrence's pinned facts and to the
    /// bodies already matched.
    pub binding: Binding,
}

pub type Binding = fn(&HttpOccurrence, &Value, &[Value]) -> Result<()>;

pub const S26_RATE_LIMIT: Scenario = Scenario {
    id: "s26-rate-limit",
    protocol: "standard",
    transcript: include_bytes!("../../testdata/e2e/providers/workbench/s26-rate-limit.json"),
    binding: rate_limit_binding,
};

pub const S26_PARTIAL_DISCONNECT: Scenario = Scenario {
    id: "s26-partial-disconnect",
    protocol: "standard",
    transcript: include_bytes!(
        "../../testdata/e2e/providers/workbench/s26-partial-disconnect.json"
    ),
    binding: partial_disconnect_binding,
};

pub const S27_AUTH_NEXT_RUN: Scenario = Scenario {
    id: "s27-auth-next-run",
    protocol: "standard",
    transcript: include_bytes!("../../testdata/e2e/providers/workbench/s27-auth-next-run.json"),
    binding: auth_next_run_binding,
};

pub const S18_APPLICATION_TIMER: Scenario = Scenario {
    id: "s18-application-timer",
    protocol: "rlm",
    transcript: include_bytes!("../../testdata/e2e/providers/workbench/s18-application-timer.json"),
    binding: application_timer_binding,
};

/// Every top-level fact the authored `body` pattern pins must appear verbatim.
fn pinned_facts(occurrence: &HttpOccurrence, body: &Value) -> Result<()> {
    let pattern = occurrence
        .body
        .as_object()
        .context("occurrence body pattern is not an object")?;
    for (key, want) in pattern {
        ensure!(
            body.get(key) == Some(want),
            "request {key} differs from the pinned fact {want}"
        );
    }
    Ok(())
}

/// The text of every `role: "user"` message part in a chat-completions body.
fn user_texts(body: &Value) -> Vec<String> {
    let mut texts = Vec::new();
    for message in body["messages"].as_array().into_iter().flatten() {
        if message["role"] != "user" {
            continue;
        }
        match &message["content"] {
            Value::String(text) => texts.push(text.clone()),
            Value::Array(parts) => texts.extend(
                parts
                    .iter()
                    .filter(|part| part["type"] == "text")
                    .map(|part| part["text"].as_str().unwrap_or_default().to_string()),
            ),
            _ => {}
        }
    }
    texts
}

/// The authored fact is that the turn's input reaches the provider verbatim.
/// Under the standard protocol it is the last user message; under RLM the
/// last user message is the protocol scaffold and the input sits earlier, so
/// presence anywhere in the user texts is what both protocols share.
fn some_user_text(body: &Value, fact: &str) -> Result<()> {
    ensure!(
        user_texts(body).iter().any(|text| text.contains(fact)),
        "no user message in the request contains {fact}"
    );
    Ok(())
}

fn rate_limit_binding(occurrence: &HttpOccurrence, body: &Value, prior: &[Value]) -> Result<()> {
    pinned_facts(occurrence, body)?;
    match prior.len() {
        0 => some_user_text(body, "answer once"),
        _ => {
            ensure!(
                body == &prior[0],
                "the retried request is not the same logical call"
            );
            Ok(())
        }
    }
}

fn partial_disconnect_binding(
    occurrence: &HttpOccurrence,
    body: &Value,
    prior: &[Value],
) -> Result<()> {
    pinned_facts(occurrence, body)?;
    ensure!(
        prior.is_empty(),
        "the partial-disconnect transcript has a single occurrence"
    );
    some_user_text(body, "partial answer")
}

fn auth_next_run_binding(occurrence: &HttpOccurrence, body: &Value, prior: &[Value]) -> Result<()> {
    pinned_facts(occurrence, body)?;
    match prior.len() {
        0 => some_user_text(body, "invalid credentials"),
        _ => {
            some_user_text(body, "fresh valid request")?;
            ensure!(
                user_texts(body)
                    .iter()
                    .any(|text| text.contains("invalid credentials")),
                "the fresh request does not carry the earlier user message"
            );
            ensure!(
                body != &prior[0],
                "the fresh request is byte-identical to the failed one"
            );
            Ok(())
        }
    }
}

fn application_timer_binding(
    occurrence: &HttpOccurrence,
    body: &Value,
    prior: &[Value],
) -> Result<()> {
    pinned_facts(occurrence, body)?;
    ensure!(
        prior.is_empty(),
        "the application-timer transcript has a single occurrence"
    );
    some_user_text(body, "await the application timer")
}

#[derive(Deserialize)]
struct InvocationRow {
    id: String,
    status: String,
    target_handler_name: String,
}

#[derive(Deserialize)]
struct JournalRow {
    entry_json: Option<String>,
}

fn where_namespace(view: &RestateView) -> String {
    format!(
        "WHERE target_service_name LIKE '{}'",
        view.service_name("").replace('\'', "''") + "%"
    )
}

fn journal_sql(id: &str) -> String {
    format!(
        "SELECT index, entry_type, name, version, entry_json FROM sys_journal WHERE id = '{}' ORDER BY index",
        id.replace('\'', "''")
    )
}

/// Every sealed `RuntimeEffectOutcome::LlmCall` in the namespace's journals.
async fn llm_completions(view: &RestateView) -> Result<Vec<RuntimeEffectOutcome>> {
    let invocations: Vec<InvocationRow> = view
        .query(&format!(
            "SELECT id, status, target_handler_name FROM sys_invocation {}",
            where_namespace(view)
        ))
        .await?;
    let mut outcomes = Vec::new();
    for invocation in invocations {
        for entry in view
            .query::<JournalRow>(&journal_sql(&invocation.id))
            .await?
        {
            let Some(raw) = entry.entry_json else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let Some(bytes) = value.pointer("/Notification/Completion/Run/result/Success") else {
                continue;
            };
            let bytes: Vec<u8> = serde_json::from_value(bytes.clone())?;
            let Ok(result) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            if result["effect_journal_version"] != json!(EFFECT_JOURNAL_VERSION)
                || result.get("envelope").is_none()
            {
                continue;
            }
            if let Some(ok) = result.pointer("/outcome/Ok")
                && let Ok(outcome @ RuntimeEffectOutcome::LlmCall { .. }) =
                    serde_json::from_value::<RuntimeEffectOutcome>(ok.clone())
            {
                outcomes.push(outcome);
            }
        }
    }
    Ok(outcomes)
}

/// Poll until a namespace invocation is suspended with a `/Command/Sleep`
/// whose wake-up is at least a day out, and return its identity.
async fn restate_suspended(view: &RestateView, deadline: Instant) -> Result<Value> {
    loop {
        ensure!(
            Instant::now() < deadline,
            "no suspended invocation holds a day-long Sleep command"
        );
        let suspended: Vec<InvocationRow> = view
            .query(&format!(
                "SELECT id, status, target_handler_name FROM sys_invocation {} AND status = 'suspended'",
                where_namespace(view)
            ))
            .await?;
        for invocation in suspended {
            for entry in view
                .query::<JournalRow>(&journal_sql(&invocation.id))
                .await?
            {
                let Some(raw) = entry.entry_json else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(&raw) else {
                    continue;
                };
                let Some(sleep_entry) = value.pointer("/Command/Sleep") else {
                    continue;
                };
                let wake_up_time = sleep_entry["wake_up_time"]
                    .as_u64()
                    .context("Sleep command lacks wake_up_time")?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_millis() as u64;
                ensure!(
                    wake_up_time >= now + 86_000_000,
                    "Sleep wake_up_time {wake_up_time} is not at least a day out"
                );
                return Ok(json!({
                    "invocation": invocation.id,
                    "completion_id": sleep_entry["completion_id"],
                    "wake_up_time": wake_up_time,
                }));
            }
        }
        sleep(Duration::from_millis(20)).await;
    }
}

/// S18 journal law: after cancellation the suspended invocation completes
/// without ever receiving a Sleep completion notification, and no open
/// invocation still targets `await_terminal`.
async fn timer_journal_evidence(view: &RestateView) -> Result<Value> {
    let invocations: Vec<InvocationRow> = view
        .query(&format!(
            "SELECT id, status, target_handler_name FROM sys_invocation {}",
            where_namespace(view)
        ))
        .await?;
    let mut sleeper = None;
    for invocation in &invocations {
        let journal = view
            .query::<JournalRow>(&journal_sql(&invocation.id))
            .await?;
        for entry in &journal {
            let Some(raw) = &entry.entry_json else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(raw) else {
                continue;
            };
            let Some(sleep_entry) = value.pointer("/Command/Sleep") else {
                continue;
            };
            let completion_id = sleep_entry["completion_id"].clone();
            ensure!(
                invocation.status == "completed",
                "suspended invocation {} did not reach completed",
                invocation.id
            );
            let notified = journal.iter().any(|row| {
                row.entry_json
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .is_some_and(|value| {
                        value.pointer("/Notification/Completion/Sleep/completion_id")
                            == Some(&completion_id)
                    })
            });
            ensure!(
                !notified,
                "invocation {} received a Sleep completion for {completion_id}",
                invocation.id
            );
            sleeper = Some(json!({
                "invocation": invocation.id,
                "completion_id": completion_id,
                "status": invocation.status,
            }));
        }
    }
    ensure!(
        sleeper.is_some(),
        "no namespace invocation's journal holds a Sleep command"
    );
    ensure!(
        invocations
            .iter()
            .all(|row| row.status == "completed" || row.target_handler_name != "await_terminal"),
        "an open invocation still targets await_terminal"
    );
    sleeper.context("sleeper missing")
}

/// Read the workbench's observation stream from `cursor` until the run's
/// terminal replacement, returning every decoded frame.
async fn observations(
    host: &WorkbenchHost,
    input: &Value,
    deadline: Instant,
) -> Result<Vec<Value>> {
    let session = input["session_id"]
        .as_str()
        .context("observations lacks session_id")?;
    let cursor = input["cursor"]
        .as_str()
        .context("observations lacks cursor")?;
    let turn = input["turn_id"]
        .as_str()
        .context("observations lacks turn_id")?;
    let response = host.observations(session, cursor).await?;
    ensure!(
        response.status().is_success(),
        "observation stream HTTP {}",
        response.status()
    );
    let mut stream = response.bytes_stream();
    let mut items = Vec::new();
    let mut buffer = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(
            !remaining.is_zero(),
            "observation stream did not reach the run's terminal"
        );
        let chunk = timeout(remaining, stream.next())
            .await?
            .context("observation stream ended before the run's terminal")??;
        buffer.extend_from_slice(&chunk);
        while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=newline).collect();
            let line = String::from_utf8(line)?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let item: Value = serde_json::from_str(line)?;
            // Session initialization/config commits can precede this Run.
            // Keep their frames, but stop only at the selected turn's commit.
            let terminal =
                item["type"] == "terminal_replacement" && item["event"]["turn_id"] == turn;
            if let Some(event) = item.get("event") {
                RemoteSessionObservationEvent::decode_json(&serde_json::to_vec(event)?).map_err(
                    |error| anyhow!("observation frame fails remote-protocol decode: {error}"),
                )?;
            }
            items.push(item);
            if terminal {
                return Ok(items);
            }
        }
    }
}

fn llm_facts(outcomes: &[RuntimeEffectOutcome]) -> Vec<Value> {
    outcomes
        .iter()
        .map(|outcome| {
            let RuntimeEffectOutcome::LlmCall {
                result,
                call_record,
                ..
            } = outcome
            else {
                unreachable!("llm_completions only collects LlmCall")
            };
            json!({
                "result": match result.as_ref() {
                    Ok(response) => json!({
                        "ok": true,
                        "usage": response.usage,
                    }),
                    Err(error) => json!({
                        "ok": false,
                        "kind": error.kind,
                        "retryable": error.retryable,
                        "terminal_reason": error.terminal_reason,
                    }),
                },
                "attempts": call_record.as_ref().map(|record| record.attempts.iter().map(|attempt| {
                    json!({
                        "ordinal": attempt.ordinal,
                        "error": attempt.error,
                        "retry_decision": attempt.retry_decision,
                        "usage": attempt.usage,
                    })
                }).collect::<Vec<_>>()),
            })
        })
        .collect()
}

async fn journal_oracle(scenario: &Scenario, view: &RestateView) -> Result<Vec<Value>> {
    let outcomes = llm_completions(view).await?;
    let facts = llm_facts(&outcomes);
    match scenario.id {
        "s26-rate-limit" => {
            ensure!(
                outcomes.len() == 1,
                "journal shows {} LlmCalls, expected 1",
                outcomes.len()
            );
            let RuntimeEffectOutcome::LlmCall {
                result,
                call_record,
                ..
            } = &outcomes[0]
            else {
                unreachable!()
            };
            let response = result
                .as_ref()
                .as_ref()
                .map_err(|error| anyhow!("the single LlmCall failed: {error:?}"))?;
            let record = call_record.as_ref().context("LlmCall lacks call_record")?;
            ensure!(
                record.attempts.len() == 2
                    && record.attempts[0].ordinal == 1
                    && record.attempts[1].ordinal == 2,
                "the 429 retry did not produce two ordered attempts: {:?}",
                facts
            );
            let error = record.attempts[0]
                .error
                .as_ref()
                .context("attempt 1 lacks a normalized error")?;
            ensure!(
                error.class == ProviderFailureKind::Quota && error.http_status == Some(429),
                "attempt 1 is not the recorded 429 quota failure: {error:?}"
            );
            ensure!(
                response.usage.input_tokens == 11 && response.usage.output_tokens == 2,
                "the committed usage is not the fixture's 11/2: {:?}",
                response.usage
            );
        }
        "s26-partial-disconnect" => {
            ensure!(
                outcomes.len() == 1,
                "journal shows {} LlmCalls, expected 1",
                outcomes.len()
            );
            let RuntimeEffectOutcome::LlmCall {
                result,
                call_record,
                ..
            } = &outcomes[0]
            else {
                unreachable!()
            };
            let error = result
                .as_ref()
                .as_ref()
                .err()
                .context("the partial disconnect LlmCall unexpectedly succeeded")?;
            ensure!(
                error.kind == ProviderFailureKind::Transport && !error.retryable,
                "partial disconnect is not a non-retryable transport failure: {error:?}"
            );
            let record = call_record.as_ref().context("LlmCall lacks call_record")?;
            ensure!(
                record.attempts.len() == 1,
                "the failed call shows {} attempts, expected 1",
                record.attempts.len()
            );
        }
        "s27-auth-next-run" => {
            ensure!(
                outcomes.len() == 2,
                "journal shows {} LlmCalls, expected 2",
                outcomes.len()
            );
            let auth = outcomes
                .iter()
                .filter(|outcome| {
                    matches!(
                        outcome,
                        RuntimeEffectOutcome::LlmCall { result, .. }
                            if matches!(result.as_ref(), Err(error) if error.kind == ProviderFailureKind::Auth)
                    )
                })
                .count();
            ensure!(auth == 1, "journal shows {auth} auth failures, expected 1");
            for outcome in outcomes {
                let RuntimeEffectOutcome::LlmCall {
                    result,
                    call_record,
                    ..
                } = outcome
                else {
                    unreachable!()
                };
                let record = call_record.as_ref().context("LlmCall lacks call_record")?;
                ensure!(
                    record.attempts.len() == 1,
                    "an S27 call shows {} attempts, expected 1",
                    record.attempts.len()
                );
                if let Ok(response) = result.as_ref() {
                    ensure!(
                        response.usage.input_tokens == 11 && response.usage.output_tokens == 2,
                        "the fresh LlmCall usage is not the fixture's 11/2"
                    );
                }
            }
        }
        _ => {}
    }
    Ok(facts)
}

pub async fn run(scenario: &'static Scenario, leg: Leg) -> Result<()> {
    Permutation::provisioned(StoreKind::SqliteFile, leg)?;
    let root = PathBuf::from(required("LASH_E2E_HOST_ARTIFACTS")?);
    let directory = root.join(scenario.id);
    std::fs::create_dir_all(&directory)?;
    let gate = required("KILN_GATE_ID")?;
    let port: u16 = required("LASH_E2E_HOST_PORT")?.parse()?;
    let mut lease = CaseLease {
        gate_id: gate.clone(),
        namespace: format!("{}-{gate}", scenario.id),
        authority: format!("{}-{gate}", scenario.id),
        directory: directory.clone(),
        postgres_url: None,
        ports: (port..port + 3).collect(),
        deadline: Instant::now() + Duration::from_secs(240),
        processes: Vec::new(),
        cleanup: Vec::new(),
    };
    let python: PathBuf = required("LASH_E2E_PYTHON")?.into();
    let digest = Command::new("sha256sum").arg(&python).output().await?;
    ensure!(digest.status.success(), "hash {} interpreter", scenario.id);
    let python = ArtifactIdentity {
        role: "workbench-browser".into(),
        path: python,
        sha256: String::from_utf8(digest.stdout)?
            .split_whitespace()
            .next()
            .context("interpreter digest missing")?
            .into(),
        candidate_sha: required("LASH_E2E_CANDIDATE_SHA")?,
        generation: required("LASH_E2E_HOST_GENERATION")?,
    };
    let view = RestateView::new(&required("RESTATE_ADMIN_URL")?, &lease.namespace)?;
    let fixture = RecordedHttpFixture::start_bound(
        SocketAddr::from(([127, 0, 0, 1], 0)),
        HttpTranscript::from_json(scenario.transcript)?,
        &directory.join("effects.jsonl"),
        Arc::new(scenario.binding),
    )
    .await?;
    let mut host = WorkbenchHost::new(
        required("RESTATE_INGRESS_URL")?,
        required("RESTATE_ADMIN_URL")?,
        lease.ports[0],
        lease.ports[1],
    )?
    .configure(BTreeMap::from([
        (
            "AGENT_WORKBENCH_PROVIDER_URL".to_string(),
            fixture.base_url(),
        ),
        (
            "AGENT_WORKBENCH_SEARCH_MCP_URL".to_string(),
            "http://127.0.0.1:1/mcp".to_string(),
        ),
        (
            "OPENROUTER_API_KEY".to_string(),
            format!("{}-local-fixture", scenario.id),
        ),
        ("OPENROUTER_MODEL".to_string(), "openai/gpt-5.4".to_string()),
        ("OPENROUTER_MODEL_VARIANT".to_string(), "high".to_string()),
        (
            "AGENT_WORKBENCH_PROTOCOL".to_string(),
            scenario.protocol.to_string(),
        ),
    ]))?;
    let workbench = artifact("WORKBENCH", "workbench")?;
    let script =
        PathBuf::from(required("LASH_E2E_REPO")?).join("scripts/e2e-workbench-provider.py");
    let mut evidence = Evidence::empty(scenario.id.to_string());
    evidence.artifacts = vec![workbench.clone(), python.clone()];
    let result = async {
        let boot = host.boot(&workbench, &mut lease).await?;
        let mut browser = Command::new(&python.path)
            .arg(&script)
            .args(["--scenario", scenario.id])
            .args(["--directory", &directory.to_string_lossy()])
            .args(["--base-url", &boot.endpoint])
            .args([
                "--remaining-case-seconds",
                &lease
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .as_secs_f64()
                    .to_string(),
            ])
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(File::create(
                directory.join(format!("{}-browser.stderr", scenario.id)),
            )?)
            .kill_on_drop(true)
            .process_group(0)
            .spawn()?;
        let pid = browser.id().context("browser pid")?;
        lease.processes.push(ProcessReceipt {
            role: "workbench-browser".to_string(),
            pid,
            incarnation: 1,
            log: directory
                .join(format!("{}-browser.stderr", scenario.id))
                .display()
                .to_string(),
        });
        let mut requests = BufReader::new(browser.stdout.take().context("browser stdout")?).lines();
        let mut replies = browser.stdin.take().context("browser stdin")?;
        let mut log = File::create(directory.join(format!("{}-browser.control", scenario.id)))?;
        let collected = async {
            while let Some(line) = timeout(
                lease.deadline.saturating_duration_since(Instant::now()),
                requests.next_line(),
            )
            .await??
            {
                writeln!(log, "{line}")?;
                if let Some(request) = line.strip_prefix("H6_CONTROL ") {
                    let request: Value = serde_json::from_str(request)?;
                    let action = request["action"]
                        .as_str()
                        .context("browser control action missing")?;
                    let input = request["input"].clone();
                    let response = match action {
                        "provider-barrier" => {
                            match fixture
                                .wait_for(
                                    lease.deadline.saturating_duration_since(Instant::now()),
                                    |event| {
                                        matches!(
                                            event,
                                            TransportEvent::BarrierEntered { barrier, .. }
                                                if *barrier
                                                    == input["barrier"].as_str().unwrap_or_default()
                                        )
                                    },
                                )
                                .await
                            {
                                Ok(event) => json!({"entered": event}),
                                Err(error) => json!({"error": format!("{error:#}")}),
                            }
                        }
                        "provider-release" => match input["barrier"].as_str() {
                            Some(barrier) => match fixture.release(barrier) {
                                Ok(()) => json!({"released": barrier}),
                                Err(error) => json!({"error": format!("{error:#}")}),
                            },
                            None => json!({"error": "provider-release lacks barrier"}),
                        },
                        "provider-requests" => match fixture.requests() {
                            Ok(bodies) => json!({"requests": bodies}),
                            Err(error) => json!({"error": format!("{error:#}")}),
                        },
                        "restate-suspended" => {
                            match restate_suspended(&view, lease.deadline).await {
                                Ok(value) => value,
                                Err(error) => json!({"error": format!("{error:#}")}),
                            }
                        }
                        "observations" => match observations(&host, &input, lease.deadline).await {
                            Ok(items) => json!({"items": items}),
                            Err(error) => json!({"error": format!("{error:#}")}),
                        },
                        _ => match host
                            .command(HostCommand::Process {
                                action: action.to_string(),
                                input,
                            })
                            .await
                        {
                            Ok(receipt) => receipt.output,
                            Err(error) => json!({"error": format!("{error:#}")}),
                        },
                    };
                    writeln!(log, "{}", serde_json::to_string(&response)?)?;
                    replies
                        .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
                        .await?;
                    replies.flush().await?;
                }
            }
            ensure!(
                browser.wait().await?.success(),
                "{} browser oracle failed",
                scenario.id
            );
            let score: Value =
                serde_json::from_slice(&std::fs::read(directory.join("scorecard.json"))?)?;
            ensure!(
                score["scenario"] == scenario.id
                    && score["selected"] == 1
                    && score["executed"] == 1
                    && score["verdict"] == "PASS",
                "{} scorecard does not record the executed scenario: {score}",
                scenario.id
            );
            if scenario.id != "s18-application-timer" {
                journal_oracle(scenario, &view).await?;
            } else {
                timer_journal_evidence(&view).await?;
                let bodies = fixture.requests()?;
                ensure!(
                    bodies.len() == 1,
                    "cancellation caused a second provider request: {}",
                    bodies.len()
                );
            }
            evidence.journals = turn_journals(&view).await?;
            println!(
                "{} selected=1 executed=1 workbench={} python={}",
                scenario.id, workbench.sha256, python.sha256
            );
            Ok(())
        }
        .await;
        if browser.try_wait()?.is_none() {
            Command::new("kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .status()
                .await?;
            browser.wait().await?;
        }
        lease
            .cleanup
            .push(lash_upgrade_harness::e2e::control::CleanupReceipt {
                resource: format!("pid:{pid}"),
                closed: true,
                detail: "browser oracle reaped".into(),
            });
        collected
    }
    .await;
    let outcome = result.map_err(|error| format!("{error:#}"));
    let host_cleanup = host.stop().await;
    let mut errors = Vec::new();
    if let Err(error) = &outcome {
        errors.push(error.clone());
    }
    let base: u16 = required("LASH_E2E_PORT_BASE")?.parse()?;
    let leg_observation = lash_upgrade_harness::e2e::cluster::observe_leg(
        leg,
        &[format!("http://127.0.0.1:{}/metrics", base + 47)],
        &directory,
    )
    .await;
    match &leg_observation {
        Ok(receipt) => evidence.stores.push(receipt.clone()),
        Err(error) => errors.push(format!("leg observation: {error:#}")),
    }
    match host.transcript() {
        Ok(observations) => evidence.outputs = observations,
        Err(error) => errors.push(format!("transcript: {error:#}")),
    }
    match fixture.requests() {
        Ok(requests) => evidence.effects = requests,
        Err(error) => errors.push(format!("fixture requests: {error:#}")),
    }
    let fixture_receipt = match fixture.finish().await {
        Ok(receipt) => receipt,
        Err(error) => HttpReceipt {
            transcript: scenario.id.to_string(),
            expected: 0,
            matched: 0,
            ended: 0,
            events: Vec::new(),
            effects: Vec::new(),
            mutations: 0,
            violations: vec![format!("fixture.finish failed: {error:#}")],
        },
    };
    match serde_json::to_value(&fixture_receipt) {
        Ok(receipt) => evidence.effects.push(receipt),
        Err(error) => errors.push(format!("fixture receipt: {error:#}")),
    }
    if let Err(error) = fixture_receipt.verify() {
        errors.push(format!("fixture receipt: {error:#}"));
    }
    evidence.cleanup.extend(lease.cleanup.iter().cloned());
    write_case_receipt(&directory, evidence, errors, &[("host", &host_cleanup)])?;
    outcome.map_err(|error| anyhow!("{error}"))?;
    leg_observation?;
    fixture_receipt.verify()?;
    ensure!(
        host_cleanup?.iter().all(|receipt| receipt.closed),
        "workbench child processes survived teardown"
    );
    Ok(())
}
