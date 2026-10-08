//! Cases on agent-workbench nodes: the product host with its `e2e-tools`
//! fixture, whose scripted provider and bodies the case's H2 fixture
//! configures. S01 (a singleton returns through one owner) lives here with
//! the helpers the other workbench modules share.

use anyhow::{Context as _, Result, ensure};
use lash_e2e::{Case, Host, Leg, NodeOptions};
use serde_json::{Value, json};

use crate::support::{self, Turn, successor};

/// The channel a workbench session speaks.
#[derive(Clone, Copy)]
pub enum Protocol {
    Standard,
    Rlm,
}

/// Node options for a workbench of H2 `scenario` whose bodies in `holds`
/// are held on entry.
pub fn options(
    case: &Case,
    scenario: &str,
    protocol: Protocol,
    holds: &[&str],
    extra: Value,
) -> Result<NodeOptions> {
    Ok(NodeOptions {
        fixture: Some(case.h2(scenario, holds, extra)?),
        env: vec![(
            "AGENT_WORKBENCH_PROTOCOL".to_owned(),
            match protocol {
                Protocol::Standard => "standard",
                Protocol::Rlm => "rlm",
            }
            .to_owned(),
        )],
        ..NodeOptions::default()
    })
}

/// Send `text` to the case's session through workbench `node`. The send
/// runs as the turn the workbench names in its receipt.
pub async fn send(case: &Case, node: &str, text: &str) -> Result<Turn> {
    let session = support::session(case);
    // The workbench clears a settled turn's claim after it settles; a send
    // before that queues for the next turn.
    case.until("no turn is left active", || async {
        let state = case
            .node(node)?
            .get(&format!("/api/state?session_id={session}"))
            .await?;
        Ok(state["active_turns"]
            .as_array()
            .is_some_and(Vec::is_empty)
            .then_some(()))
    })
    .await?;
    let receipt = case
        .node(node)?
        .post(
            &format!("/api/turn?session_id={session}"),
            &json!({"text": text}),
        )
        .await?;
    case.record_barrier(json!({"barrier": "accepted", "node": node, "receipt": receipt}));
    ensure!(
        receipt["accepted"] == true && receipt["queued"] == false,
        "the send was not started: {receipt}"
    );
    let turn = receipt["turn_id"]
        .as_str()
        .context("the receipt names no turn")?
        .to_owned();
    Ok(Turn {
        input: turn.clone(),
        run: turn,
    })
}

/// Follow `turn` to its settled outcome through workbench `node`.
pub async fn follow(case: &mut Case, node: &str, turn: &Turn) -> Result<Value> {
    let session = support::session(case);
    let outcome = case
        .node(node)?
        .get(&format!("/api/e2e/sessions/{session}/turns/{}", turn.input))
        .await?;
    case.evidence
        .outputs
        .push(json!({"node": node, "turn": turn.input, "outcome": outcome}));
    Ok(outcome)
}

/// Cancel the session's running turn through workbench `node`, answering
/// the receipt.
pub async fn cancel(case: &mut Case, node: &str) -> Result<Value> {
    let session = support::session(case);
    let receipt = case
        .node(node)?
        .post(
            &format!("/api/turn/cancel?session_id={session}"),
            &json!({}),
        )
        .await?;
    case.evidence
        .faults
        .push(json!({"fault": "cancel", "node": node, "receipt": receipt}));
    Ok(receipt)
}

/// The fixture body deliveries of `label`, in order.
pub fn deliveries(case: &Case, label: &str) -> Result<Vec<Value>> {
    Ok(case
        .bodies()?
        .into_iter()
        .filter(|line| line["label"] == label)
        .collect())
}

/// The scripted provider's requests, in order, by stage.
pub fn stages(case: &Case) -> Result<Vec<String>> {
    Ok(lash_e2e::read_jsonl(&case.dir.join("provider.jsonl"))?
        .into_iter()
        .map(|line| line["stage"].as_str().unwrap_or_default().to_owned())
        .collect())
}

/// Read the session's product event stream from its start until a line
/// satisfies `found`, answering the lines read.
pub async fn product_events(
    case: &Case,
    node: &str,
    found: impl Fn(&Value) -> bool,
) -> Result<Vec<Value>> {
    product_events_after(case, node, 0, found).await
}

/// [`product_events`] from after sequence `cursor`.
pub async fn product_events_after(
    case: &Case,
    node: &str,
    cursor: u64,
    found: impl Fn(&Value) -> bool,
) -> Result<Vec<Value>> {
    let session = support::session(case);
    let url = format!(
        "{}/api/events?session_id={session}&cursor={cursor}",
        case.node(node)?.url
    );
    let mut response = reqwest::get(url).await?.error_for_status()?;
    let mut buffer = Vec::new();
    let mut lines = Vec::new();
    loop {
        let chunk = tokio::time::timeout_at(case.deadline.into(), response.chunk())
            .await
            .context("the product stream showed no awaited item by the deadline")??
            .context("the product stream ended")?;
        buffer.extend_from_slice(&chunk);
        while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let value: Value = serde_json::from_slice(&line)?;
            let done = found(&value);
            lines.push(value);
            if done {
                return Ok(lines);
            }
        }
    }
}

/// Whether product stream line `line` is turn `run`'s `done` item.
pub fn done(line: &Value, run: &str) -> bool {
    line["type"] == "event" && line["event"]["type"] == "done" && line["event"]["turn_id"] == run
}

/// Release the commit cut `label` held on workbench `node`.
pub async fn release_cut(case: &Case, node: &str, label: &str) -> Result<()> {
    case.node(node)?
        .post("/api/e2e/control/cuts/release", &json!(label))
        .await?;
    Ok(())
}

case!(
    s01_singleton_returns_through_one_owner,
    SqliteFile,
    Live,
    s01
);
case!(
    s01_singleton_returns_through_one_owner_resume,
    SqliteFile,
    Resume,
    s01
);
case!(
    s01_singleton_returns_through_one_owner_live_replay,
    SqliteFile,
    Live,
    s01
);

/// One echo call on a Standard workbench. Its admission, start, outcome
/// and presentation are rows of the run that owns it, decoded by the
/// controller from the store; the reply is the provider's one answer. On
/// the resume leg the node dies once the echo's outcome is committed and
/// another node presents it: the body never runs again.
async fn s01(case: &mut Case) -> Result<()> {
    let options = options(case, "S01", Protocol::Standard, &[], json!({}))?;
    let held = NodeOptions {
        cuts: vec![json!({"label": "round.outcome"})],
        ..options.clone()
    };
    let resume = case.leg == Leg::Resume;
    case.boot(
        Host::Workbench,
        "node-a",
        if resume { held } else { options.clone() },
    )
    .await?;
    let turn = send(case, "node-a", "S01 echo once").await?;
    let node = if resume {
        case.held("node-a", "round.outcome").await?;
        case.kill("node-a", "the echo's outcome committed, nothing presented")
            .await?;
        let cut = support::record_facts(case, &turn.run, "after the kill").await?;
        ensure!(
            support::outcome_epochs(&cut).len() == 1
                && support::records(&cut, "present").is_empty(),
            "the cut is not one committed outcome before its presentation: {cut}"
        );
        let next = successor(case);
        case.boot(Host::Workbench, next, options).await?;
        next
    } else {
        "node-a"
    };
    let outcome = follow(case, node, &turn).await?;
    let (kind, reply) = support::settled(&outcome);
    ensure!(
        kind == "completed" && reply.as_deref() == Some("echo"),
        "the turn settled {kind} {reply:?}: {outcome}"
    );
    if case.live_replay {
        ensure!(
            outcome["gaps"].as_array().is_none_or(Vec::is_empty),
            "the shared live replay store reported gaps: {outcome}"
        );
    }
    let echo = deliveries(case, "echo")?;
    ensure!(echo.len() == 1, "the echo body ran {} times", echo.len());
    ensure!(
        stages(case)? == ["initial", "terminal"],
        "the provider answered {:?}, not one call per stage",
        stages(case)?
    );
    case.stop(node).await?;
    let facts = support::record_facts(case, &turn.run, "after the turn").await?;
    ensure!(
        facts["end"]["kind"] == "Answered",
        "the store's turn end is {facts}"
    );
    let call = echo[0]["call_id"].as_str().context("no echo call id")?;
    // The rows are read under the turn's owner key; one run of it holds
    // them all.
    let runs: std::collections::BTreeSet<_> = facts["records"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|record| record["run"].to_string())
        .collect();
    ensure!(runs.len() == 1, "the turn's rows span runs {runs:?}");
    for kind in ["x_start", "x_outcome"] {
        let rows = support::records(&facts, kind);
        ensure!(
            rows.len() == 1 && rows[0]["call"] == call,
            "the run holds {} {kind} rows, not one of call {call}: {facts}",
            rows.len()
        );
    }
    for kind in ["admit", "present"] {
        let rows = support::records(&facts, kind);
        ensure!(
            rows.len() == 1,
            "the run holds {} {kind} rows: {facts}",
            rows.len()
        );
    }
    if resume {
        let outcome_epoch = support::records(&facts, "x_outcome")[0]["epoch"].clone();
        let present_epoch = support::records(&facts, "present")[0]["epoch"].clone();
        ensure!(
            present_epoch.as_i64() > outcome_epoch.as_i64(),
            "the outcome (epoch {outcome_epoch}) and its presentation (epoch {present_epoch}) were not written by the dead node and its claimer"
        );
    }
    Ok(())
}

/// A session feed a case follows on a workbench node: the node's
/// `/api/observations` stream, its lines kept as they arrive.
pub struct Feed {
    lines: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Feed {
    /// The lines so far.
    pub fn lines(&self) -> Vec<Value> {
        self.lines
            .lock()
            .map(|lines| lines.clone())
            .unwrap_or_default()
    }

    /// Stop following, answering every line read.
    pub fn stop(self) -> Vec<Value> {
        self.task.abort();
        self.lines()
    }
}

/// Follow the case's session feed on workbench `node` from now.
pub async fn observe(case: &Case, node: &str) -> Result<Feed> {
    let session = support::session(case);
    let url = format!(
        "{}/api/observations?session_id={session}",
        case.node(node)?.url
    );
    let response = reqwest::Client::new()
        .get(url)
        .header(
            "x-lash-protocol-hello",
            json!({"negotiation": "hello", "supported": {"min": 100, "max": 100}}).to_string(),
        )
        .send()
        .await?
        .error_for_status()?;
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let kept = std::sync::Arc::clone(&lines);
    let task = tokio::spawn(async move {
        let mut response = response;
        let mut buffer = Vec::new();
        while let Ok(Some(chunk)) = response.chunk().await {
            buffer.extend_from_slice(&chunk);
            while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = buffer.drain(..=end).collect();
                if let Ok(value) = serde_json::from_slice::<Value>(&line)
                    && let Ok(mut kept) = kept.lock()
                {
                    kept.push(value);
                }
            }
        }
    });
    Ok(Feed { lines, task })
}
