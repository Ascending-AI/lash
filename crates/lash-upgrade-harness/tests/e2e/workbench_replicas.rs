//! S37: two workbench replicas share one Restate authority and one
//! PostgreSQL store. Replica B registers last, so the turn A accepts
//! executes on B while observers follow the session's feed on A and on B;
//! B is killed mid-turn and the turn recovers when B comes back.
//!
//! The feed's snapshot is the durable head on every replica; which
//! processes' activity reaches a feed's tail is the configured live replay
//! store's property (docs/observing-turns.md). The variant names the store
//! both replicas run with and the contract its oracle holds them to.
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use futures_util::StreamExt as _;
use lash_remote_protocol::RemoteSessionObservationEvent;
use lash_upgrade_harness::e2e::case::{ArtifactIdentity, CaseLease, Leg, Permutation, StoreKind};
use lash_upgrade_harness::e2e::evidence::Evidence;
use lash_upgrade_harness::e2e::host::{HostAdapter as _, HostCommand};
use lash_upgrade_harness::e2e::host_adapters::workbench::WorkbenchHost;
use lash_upgrade_harness::e2e::provider_http::transcript::{HttpOccurrence, HttpTranscript};
use lash_upgrade_harness::e2e::provider_http::{HttpReceipt, RecordedHttpFixture, TransportEvent};
use lash_upgrade_harness::restate_view::RestateView;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use super::{artifact, required, turn_journals, write_case_receipt};

const QUESTION: &str = "S37-REPLICA-FEED observe me from the other replica";
const OUTPUT: &str = "S37 replica program output";
const ANSWER: &str = "Answered on replica B";
const BARRIER: &str = "s37-b-mid-turn";

/// The live replay store both replicas run with.
pub struct Variant {
    /// Lease and scorecard identity.
    pub id: &'static str,
    /// `AGENT_WORKBENCH_LIVE_REPLAY_STORE` for both replicas.
    pub live_replay_store: &'static str,
    /// Whether one store carries every replica's activity. A process-local
    /// store holds only its own process's: A's feed then carries none of
    /// B's live activity, and B's commit reaches A through the durable head.
    pub shared: bool,
}

pub const MEMORY_LIVE_REPLAY: Variant = Variant {
    id: "s37-memory-live-replay",
    live_replay_store: "memory",
    shared: false,
};

/// Both replicas share the PostgreSQL live replay store (FIG-5101): A's
/// feeds carry B's live activity and converge without a gap.
pub const POSTGRESQL_LIVE_REPLAY: Variant = Variant {
    id: "s37-postgresql-live-replay",
    live_replay_store: "postgresql",
    shared: true,
};

/// The production RLM stream: one cell that prints and finishes. The first
/// request is held after its first frame and B dies under it, so it ends
/// with no terminating chunk; the recovered B's request replays in full.
fn transcript() -> Result<HttpTranscript> {
    let frame = |delta: Value, finish: Value, usage: Option<Value>| {
        let mut frame = json!({
            "id": "s37-recorded-call",
            "model": "openai/gpt-5.4",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        });
        if let Some(usage) = usage {
            frame["usage"] = usage;
        }
        format!("data: {frame}\n\n")
    };
    let first = frame(
        json!({"role": "assistant", "content": format!("<typescript>\nprint({});\n", json!(OUTPUT))}),
        Value::Null,
        None,
    );
    let rest = frame(
        json!({"content": format!("finish({});\n</typescript>", json!(ANSWER))}),
        Value::Null,
        None,
    );
    let stop = frame(
        json!({}),
        json!("stop"),
        Some(json!({"prompt_tokens": 100, "completion_tokens": 30, "total_tokens": 130})),
    );
    let occurrence = |identity: &str, chunks: Value, termination: &str| {
        json!({
            "identity": identity,
            "method": "POST",
            "path": "/v1/chat/completions",
            "body": {"model": "openai/gpt-5.4", "stream": true},
            "response": {
                "status": 200,
                "headers": [["content-type", "text/event-stream"]],
                "chunks": chunks,
                "termination": termination,
                "usage": Value::Null,
                "typed_failure": Value::Null,
            },
        })
    };
    let transcript = json!({
        "name": "workbench-s37-replica-feed",
        "occurrences": [
            occurrence(
                "s37-killed-on-b",
                json!([{"bytes": first, "hold_after": BARRIER}]),
                "disconnect",
            ),
            occurrence(
                "s37-recovered-on-b",
                json!([
                    {"bytes": first, "hold_after": Value::Null},
                    {"bytes": rest, "hold_after": Value::Null},
                    {"bytes": stop, "hold_after": Value::Null},
                    {"bytes": "data: [DONE]\n\n", "hold_after": Value::Null},
                ]),
                "complete",
            ),
        ],
    });
    HttpTranscript::from_json(&serde_json::to_vec(&transcript)?)
}

/// The accepted input reaches the provider, and the recovered replica's
/// request is the same logical call as the killed one's.
fn binding(occurrence: &HttpOccurrence, body: &Value, prior: &[Value]) -> Result<()> {
    for (key, want) in occurrence
        .body
        .as_object()
        .context("occurrence body pattern is not an object")?
    {
        ensure!(
            body.get(key) == Some(want),
            "request {key} differs from the pinned fact {want}"
        );
    }
    ensure!(
        body["messages"].to_string().contains(QUESTION),
        "provider request lost the accepted input"
    );
    if let Some(first) = prior.first() {
        ensure!(
            body == first,
            "the recovered request is not the killed request's logical call"
        );
    }
    Ok(())
}

/// One HTTP observer of a replica's `/api/observations`, read on its own
/// task so the case keeps driving while frames arrive.
struct Observer {
    label: String,
    session: String,
    /// The snapshot the observer started from: the durable head's rows
    /// and the cursor bound to its revision.
    start: Snapshot,
    /// Every frame with the instant it arrived.
    frames: Arc<Mutex<Vec<(Instant, Value)>>>,
    changed: Arc<Notify>,
    reader: JoinHandle<Result<()>>,
}

#[derive(Clone)]
struct Snapshot {
    rows: Vec<Value>,
    cursor: String,
    revision: u64,
}

impl Snapshot {
    async fn read(host: &WorkbenchHost, session: &str) -> Result<(Self, Value)> {
        let state = host.snapshot(session).await?;
        let cursor = state["observation"]["cursor"]
            .as_str()
            .context("snapshot has no cursor")?
            .to_owned();
        let snapshot = Self {
            rows: state["transcript"]
                .as_array()
                .context("snapshot has no transcript")?
                .clone(),
            revision: revision(session, &cursor)?,
            cursor,
        };
        Ok((snapshot, state))
    }

    fn row_ids(&self) -> Vec<String> {
        row_ids(&self.rows)
    }
}

fn revision(session: &str, cursor: &str) -> Result<u64> {
    let cursor = lash_core::SessionCursor::from_store_token(cursor)?;
    Ok(cursor
        .parse_for_session(&session.parse::<lash_core::SessionId>()?)?
        .revision
        .0)
}

fn row_ids(rows: &[Value]) -> Vec<String> {
    rows.iter().map(|row| row["row_id"].to_string()).collect()
}

impl Observer {
    /// Snapshot the replica, then follow its feed from that snapshot's
    /// cursor, returning once the feed has answered with its cursor frame.
    async fn open(
        label: &str,
        host: &WorkbenchHost,
        session: &str,
        deadline: Instant,
    ) -> Result<Self> {
        let (start, _) = Snapshot::read(host, session).await?;
        let observer = Self::follow(label, host, session, start).await?;
        observer
            .wait(deadline, |frames| !frames.is_empty())
            .await
            .with_context(|| format!("{label} feed never answered"))?;
        let first = observer.frames().remove(0);
        ensure!(
            first["type"] == "cursor" && first["cursor"] == observer.start.cursor,
            "{label} feed did not open at its snapshot's cursor: {first}"
        );
        Ok(observer)
    }

    async fn follow(
        label: &str,
        host: &WorkbenchHost,
        session: &str,
        start: Snapshot,
    ) -> Result<Self> {
        let response = host.observations(session, &start.cursor).await?;
        let frames = Arc::new(Mutex::new(Vec::new()));
        let changed = Arc::new(Notify::new());
        let reader = tokio::spawn({
            let frames = Arc::clone(&frames);
            let changed = Arc::clone(&changed);
            async move {
                let mut stream = response.bytes_stream();
                let mut buffer = Vec::new();
                while let Some(chunk) = stream.next().await {
                    buffer.extend_from_slice(&chunk?);
                    while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
                        let line: Vec<u8> = buffer.drain(..=newline).collect();
                        let line = String::from_utf8(line)?;
                        if line.trim().is_empty() {
                            continue;
                        }
                        let frame: Value = serde_json::from_str(line.trim())?;
                        if let Some(event) = frame.get("event") {
                            RemoteSessionObservationEvent::decode_json(&serde_json::to_vec(event)?)
                                .map_err(|error| {
                                    anyhow!(
                                        "observation frame fails remote-protocol decode: {error}"
                                    )
                                })?;
                        }
                        frames
                            .lock()
                            .map_err(|_| anyhow!("observer frames poisoned"))?
                            .push((Instant::now(), frame));
                        changed.notify_waiters();
                    }
                }
                anyhow::Ok(())
            }
        });
        Ok(Self {
            label: label.into(),
            session: session.into(),
            start,
            frames,
            changed,
            reader,
        })
    }

    fn frames(&self) -> Vec<Value> {
        self.arrivals()
            .into_iter()
            .map(|(_, frame)| frame)
            .collect()
    }

    /// Every frame with the instant it arrived.
    fn arrivals(&self) -> Vec<(Instant, Value)> {
        self.frames
            .lock()
            .map(|frames| frames.clone())
            .unwrap_or_default()
    }

    /// Wait until the frames satisfy `predicate`, or the deadline passes.
    async fn wait(&self, deadline: Instant, predicate: impl Fn(&[Value]) -> bool) -> Result<()> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if predicate(&self.frames()) {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "{} watchdog", self.label);
            // A dead stream sends no wake; recheck on a short tick.
            let _ = timeout(remaining.min(Duration::from_millis(200)), changed).await;
        }
    }

    /// The newest position the feed has handed its consumer.
    fn last_cursor(&self) -> String {
        let mut cursor = self.start.cursor.clone();
        for frame in self.frames() {
            let next = match frame["type"].as_str() {
                Some("cursor" | "terminal_replacement" | "resident_replacement") => {
                    frame["cursor"].as_str()
                }
                Some("observation") => frame["event"]["cursor"].as_str(),
                Some("replay_gap") => frame["observation"]["cursor"].as_str(),
                _ => None,
            };
            if let Some(next) = next {
                cursor = next.to_owned();
            }
        }
        cursor
    }

    /// Stop reading and return every frame the feed delivered.
    fn close(&self) -> Vec<Value> {
        self.reader.abort();
        self.frames()
    }
}

/// What one observer's feed made of the turn.
struct Folded {
    /// The rows the observer holds: its snapshot, then every delivered
    /// commit's rows. `None` after a gap until the fold re-anchors.
    rows: Option<Vec<String>>,
    /// Rows delivered by commits after the last gap.
    suffix: Vec<String>,
    /// Every row any commit delivered, in delivery order.
    committed: Vec<String>,
    /// The turn's live activity events, and how many arrived before the
    /// first commit that carried any of the turn's rows.
    activity: usize,
    activity_before_commit: usize,
    gaps: Vec<FeedGap>,
}

/// A replay gap the feed delivered.
#[derive(Debug, serde::Serialize)]
struct FeedGap {
    /// The gap frame's index among the feed's frames.
    frame: usize,
    latest_revision: u64,
    reason: String,
}

fn is_commit(frame: &Value) -> bool {
    matches!(
        frame["type"].as_str(),
        Some("observation" | "terminal_replacement")
    ) && frame["event"]["type"] == "committed"
}

fn is_turn_activity(frame: &Value, turn: &str) -> bool {
    frame["type"] == "observation"
        && frame["event"]["type"] == "turn_activity"
        && frame["event"]["turn_id"] == turn
}

fn fold(start: &Snapshot, frames: &[Value], turn: &str, final_rows: &[String]) -> Result<Folded> {
    let turn_rows: BTreeSet<&String> = final_rows
        .iter()
        .filter(|row| !start.row_ids().contains(row))
        .collect();
    let mut folded = Folded {
        rows: Some(start.row_ids()),
        suffix: Vec::new(),
        committed: Vec::new(),
        activity: 0,
        activity_before_commit: 0,
        gaps: Vec::new(),
    };
    let mut turn_committed = false;
    for (index, frame) in frames.iter().enumerate() {
        if is_turn_activity(frame, turn) {
            folded.activity += 1;
            if !turn_committed {
                folded.activity_before_commit += 1;
            }
            continue;
        }
        let rows = match frame["type"].as_str() {
            _ if is_commit(frame) => row_ids(
                frame["event"]["rows"]
                    .as_array()
                    .context("commit without rows")?,
            ),
            Some("replay_gap") => {
                folded.gaps.push(FeedGap {
                    frame: index,
                    latest_revision: frame["gap"]["latest_revision"]
                        .as_u64()
                        .context("gap has no latest revision")?,
                    reason: frame["gap"]["reason"]
                        .as_str()
                        .context("gap has no reason")?
                        .to_owned(),
                });
                folded.rows = None;
                folded.suffix.clear();
                continue;
            }
            _ => continue,
        };
        for row in rows {
            ensure!(
                !folded.committed.contains(&row),
                "feed delivered row {row} twice"
            );
            turn_committed |= turn_rows.contains(&row);
            folded.committed.push(row.clone());
            folded.suffix.push(row.clone());
            if let Some(held) = folded.rows.as_mut() {
                held.push(row);
            }
        }
    }
    Ok(folded)
}

impl Folded {
    /// Whether the observer's rows are exactly the final durable rows.
    fn converged(&self, final_rows: &[String], final_revision: u64) -> bool {
        match &self.rows {
            Some(rows) => rows == final_rows,
            // A gap re-anchors on the durable head at its revision; the
            // feed holds the final rows once that head is the final one
            // and no commit followed it.
            None => {
                self.gaps
                    .last()
                    .is_some_and(|gap| gap.latest_revision == final_revision)
                    && self.suffix.is_empty()
            }
        }
    }

    /// A shared store's feed gaps only where the scenario forced it: a
    /// writer crash forces a gap, never a silent resume. The killed
    /// publisher may have committed a revision durably without publishing
    /// it, because a run's mid-run commits are published at settle
    /// (`crates/lash/src/turn.rs:70`/`:91`); the restarted publisher's first `Committed`
    /// then extends a revision the feed never saw, and the feed rebuilds
    /// from the durable head (the Diverged rebuild, reason `unavailable`).
    /// So at most one gap per kill, of that reason, arriving after the kill
    /// and no later than the first commit the feed delivers after it, and
    /// the feed converges afterwards (checked by the caller). Any other gap
    /// is a lost event.
    fn gaps_only_from_kills(
        &self,
        label: &str,
        arrivals: &[(Instant, Value)],
        kills: &[Instant],
    ) -> Result<()> {
        let mut gapped = BTreeSet::new();
        for gap in &self.gaps {
            let (arrived, _) = &arrivals[gap.frame];
            let Some(kill) = kills.iter().rposition(|killed| killed <= arrived) else {
                bail!("{label}: a shared store's feed gapped before any kill: {gap:?}");
            };
            let killed = &kills[kill];
            ensure!(
                gapped.insert(kill),
                "{label}: a shared store's feed gapped more than once for one kill: {:?}",
                self.gaps
            );
            ensure!(
                gap.reason == "unavailable",
                "{label}: a shared store's feed gapped after a kill for a reason other than \
                 the restarted publisher's divergence: {gap:?}"
            );
            let committed_first = arrivals[..gap.frame]
                .iter()
                .any(|(at, frame)| at >= killed && is_commit(frame));
            ensure!(
                !committed_first,
                "{label}: a shared store's feed gapped after the first commit that followed \
                 the kill, so the kill did not cause it: {gap:?}"
            );
        }
        Ok(())
    }
}

/// Every lash run invocation of the case and the deployment it is pinned to.
async fn run_invocations(view: &RestateView) -> Result<Vec<Value>> {
    let prefix = view.service_name("LashTurn");
    let rows: Vec<Value> = view
        .query(
            "SELECT id, target_service_name, target_service_key, status, pinned_deployment_id \
             FROM sys_invocation WHERE target_handler_name='run'",
        )
        .await?;
    Ok(rows
        .into_iter()
        .filter(|row| {
            row["target_service_name"]
                .as_str()
                .is_some_and(|name| name == prefix || name.starts_with(&format!("{prefix}_g")))
        })
        .collect())
}

fn deployment_id(directory: &Path) -> Result<String> {
    let deployment: Value =
        serde_json::from_slice(&std::fs::read(directory.join("workbench-deployment.json"))?)?;
    Ok(deployment["id"]
        .as_str()
        .context("deployment receipt has no id")?
        .to_owned())
}

fn write(directory: &Path, name: &str, value: &Value) -> Result<()> {
    std::fs::write(directory.join(name), serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

pub async fn run(variant: &'static Variant) -> Result<()> {
    let permutation = Permutation::provisioned(StoreKind::PostgreSql, Leg::Live)?;
    let root = PathBuf::from(required("LASH_E2E_HOST_ARTIFACTS")?);
    let directory = root.join(variant.id);
    std::fs::create_dir_all(&directory)?;
    let gate = required("KILN_GATE_ID")?;
    let port: u16 = required("LASH_E2E_HOST_PORT")?.parse()?;
    let deadline = Instant::now() + Duration::from_secs(300);
    // One authority and namespace: both replicas are deployments of one
    // service lineage. Each keeps its own logs and deployment receipt.
    let lease = |name: &str, ports: Vec<u16>| -> Result<CaseLease> {
        let directory = directory.join(name);
        std::fs::create_dir_all(&directory)?;
        Ok(CaseLease {
            gate_id: gate.clone(),
            namespace: format!("{}-{gate}", variant.id),
            authority: format!("{}-{gate}", variant.id),
            directory,
            postgres_url: None,
            ports,
            deadline,
            processes: Vec::new(),
            cleanup: Vec::new(),
        })
    };
    let mut lease_a = lease("replica-a", vec![port, port + 1])?;
    let mut lease_b = lease("replica-b", vec![port + 2, port + 3])?;
    let postgres_url = permutation
        .postgres_url(&mut lease_a)
        .await?
        .context("the PostgreSQL permutation provisioned no database")?;
    lease_b.postgres_url = Some(postgres_url.clone());
    let fixture = RecordedHttpFixture::start_bound(
        SocketAddr::from(([127, 0, 0, 1], 0)),
        transcript()?,
        &directory.join("effects.jsonl"),
        Arc::new(binding),
    )
    .await?;
    let environment = BTreeMap::from([
        (
            "AGENT_WORKBENCH_PROVIDER_URL".to_string(),
            fixture.base_url(),
        ),
        (
            "AGENT_WORKBENCH_SEARCH_MCP_URL".to_string(),
            "http://127.0.0.1:1/mcp".to_string(),
        ),
        ("AGENT_WORKBENCH_DATABASE_URL".to_string(), postgres_url),
        (
            "AGENT_WORKBENCH_LIVE_REPLAY_STORE".to_string(),
            variant.live_replay_store.to_string(),
        ),
        (
            "OPENROUTER_API_KEY".to_string(),
            "s37-local-fixture".to_string(),
        ),
        ("OPENROUTER_MODEL".to_string(), "openai/gpt-5.4".to_string()),
        ("OPENROUTER_MODEL_VARIANT".to_string(), "high".to_string()),
    ]);
    let ingress = required("RESTATE_INGRESS_URL")?;
    let admin = required("RESTATE_ADMIN_URL")?;
    let mut host_a = WorkbenchHost::new(ingress.clone(), admin.clone(), port, port + 1)?
        .configure(environment.clone())?;
    let mut host_b =
        WorkbenchHost::new(ingress, admin.clone(), port + 2, port + 3)?.configure(environment)?;
    let workbench = artifact("WORKBENCH", "workbench")?;
    let server_path = required("LASH_RESTATE_SERVER_BIN")?;
    let server = ArtifactIdentity {
        role: "restate-server".into(),
        sha256: lash_core::stable_hash::sha256_hex(&std::fs::read(&server_path)?),
        path: server_path.into(),
        candidate_sha: required("LASH_E2E_CANDIDATE_SHA")?,
        generation: required("LASH_E2E_HOST_GENERATION")?,
    };
    let view = RestateView::new(&admin, &lease_a.namespace)?;
    let mut evidence = Evidence::empty("S37".into());
    evidence.artifacts = vec![workbench.clone(), server];
    let mut observers = Vec::new();
    let result = scenario(
        variant,
        &fixture,
        &view,
        (&mut host_a, &mut lease_a),
        (&mut host_b, &mut lease_b),
        &workbench,
        &mut observers,
        &mut evidence,
        &directory,
    )
    .await;
    for observer in &observers {
        write(
            &directory,
            &format!("{}-frames.json", observer.label),
            &json!(observer.close()),
        )?;
    }
    let cleanup_a = host_a.stop().await;
    let cleanup_b = host_b.stop().await;
    let mut errors = Vec::new();
    if let Err(error) = &result {
        errors.push(format!("{error:#}"));
    }
    for host in [&host_a, &host_b] {
        match host.transcript() {
            Ok(observations) => evidence.outputs.extend(observations),
            Err(error) => errors.push(format!("transcript: {error:#}")),
        }
    }
    match fixture.requests() {
        Ok(requests) => evidence.effects = requests,
        Err(error) => errors.push(format!("fixture requests: {error:#}")),
    }
    let fixture_receipt = match fixture.finish().await {
        Ok(receipt) => receipt,
        Err(error) => HttpReceipt {
            transcript: variant.id.to_string(),
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
    evidence.cleanup.extend(lease_a.cleanup.iter().cloned());
    evidence.cleanup.extend(lease_b.cleanup.iter().cloned());
    let errors = write_case_receipt(
        &directory,
        evidence,
        errors,
        &[("replica-a", &cleanup_a), ("replica-b", &cleanup_b)],
    )?;
    result?;
    ensure!(
        errors.is_empty(),
        "S37 case receipt recorded a failure: {errors:?}"
    );
    ensure!(
        cleanup_a?
            .iter()
            .chain(cleanup_b?.iter())
            .all(|receipt| receipt.closed),
        "S37 leaked a replica lifetime"
    );
    println!(
        "{} selected=1 executed=1 workbench={}",
        variant.id, workbench.sha256
    );
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "the case owns both replicas, the fixture and the evidence it writes"
)]
async fn scenario(
    variant: &Variant,
    fixture: &RecordedHttpFixture,
    view: &RestateView,
    (host_a, lease_a): (&mut WorkbenchHost, &mut CaseLease),
    (host_b, lease_b): (&mut WorkbenchHost, &mut CaseLease),
    workbench: &ArtifactIdentity,
    observers: &mut Vec<Observer>,
    evidence: &mut Evidence,
    directory: &Path,
) -> Result<()> {
    let deadline = lease_a.deadline;
    // B registers last, so Restate hands every new call to B's deployment.
    host_a.boot(workbench, lease_a).await?;
    host_b.boot(workbench, lease_b).await?;
    let deployment_a = deployment_id(&lease_a.directory)?;
    let deployment_b = deployment_id(&lease_b.directory)?;
    ensure!(
        deployment_a != deployment_b,
        "the replicas share one deployment"
    );
    let session = host_a
        .command(HostCommand::Process {
            action: "create-session".into(),
            input: json!({"session": "s37"}),
        })
        .await?
        .output["session_id"]
        .as_str()
        .context("session creation returned no id")?
        .to_owned();
    let before = Snapshot::read(host_a, &session).await?.0;
    observers.push(Observer::open("a-before-turn", host_a, &session, deadline).await?);
    observers.push(Observer::open("b-before-turn", host_b, &session, deadline).await?);

    // A accepts the turn; it executes on B.
    let accepted = host_a
        .command(HostCommand::Submit {
            session: "s37".into(),
            idempotency_key: "s37-replica-turn".into(),
            input: json!(QUESTION),
        })
        .await?;
    let turn = accepted.output["receipt"]["turn_id"]
        .as_str()
        .context("accepted input names no turn")?
        .to_owned();
    fixture
        .wait_for(deadline.saturating_duration_since(Instant::now()), |event| {
            matches!(event, TransportEvent::BarrierEntered { barrier, .. } if barrier == BARRIER)
        })
        .await?;

    // Mid-turn, under the held provider stream: nothing of the turn is
    // committed, and B has published its live activity.
    observers[1]
        .wait(deadline, |frames| {
            frames.iter().any(|frame| is_turn_activity(frame, &turn))
        })
        .await
        .context("B's own feed carried no live activity of the turn B executes")?;
    if variant.shared {
        observers[0]
            .wait(deadline, |frames| {
                frames.iter().any(|frame| is_turn_activity(frame, &turn))
            })
            .await
            .context("A's feed carried no live activity from B before commit")?;
    } else {
        // B's activity reached B's feed; give it the same time to reach A.
        sleep(Duration::from_secs(1)).await;
        let leaked = observers[0]
            .frames()
            .iter()
            .filter(|frame| is_turn_activity(frame, &turn))
            .count();
        ensure!(
            leaked == 0,
            "a process-local live replay store carried {leaked} of B's activities to A"
        );
    }
    let (mid_a, mid_a_state) = Snapshot::read(host_a, &session).await?;
    let (mid_b, _) = Snapshot::read(host_b, &session).await?;
    ensure!(
        mid_a.revision == mid_b.revision && mid_a.row_ids() == mid_b.row_ids(),
        "mid-turn snapshots differ across replicas: A r{} {:?}, B r{} {:?}",
        mid_a.revision,
        mid_a.row_ids(),
        mid_b.revision,
        mid_b.row_ids()
    );
    ensure!(
        !mid_a_state["transcript"].to_string().contains(ANSWER),
        "a mid-turn snapshot holds the uncommitted reply"
    );
    let mid = Observer::follow("a-mid-turn", host_a, &session, mid_a).await?;
    mid.wait(deadline, |frames| !frames.is_empty())
        .await
        .context("A's mid-turn feed never answered")?;
    observers.push(mid);

    // B dies under the held stream; the turn recovers on B's return.
    let killed = host_b
        .command(HostCommand::Process {
            action: "kill-host".into(),
            input: Value::Null,
        })
        .await?
        .output;
    ensure!(
        killed["killed"] == true && killed["reaped"] == true,
        "B was not killed: {killed}"
    );
    // B is reaped: a gap a feed delivers from here on may be this kill's.
    let kills = [Instant::now()];
    fixture.release(BARRIER)?;
    let restarted = host_b
        .command(HostCommand::Process {
            action: "restart-in-place".into(),
            input: Value::Null,
        })
        .await?
        .output;
    ensure!(
        restarted["process"]["pid"] != killed["process"]["pid"],
        "B did not come back as a new process"
    );
    let final_state = loop {
        let state = host_a.snapshot(&session).await?;
        let replied = state["transcript"].as_array().is_some_and(|rows| {
            rows.iter()
                .any(|row| row["kind"] == "assistant_reply" && row["content"]["text"] == ANSWER)
        });
        if replied && state["active_turns"].as_array().is_some_and(Vec::is_empty) {
            break state;
        }
        ensure!(Instant::now() < deadline, "the turn never recovered");
        sleep(Duration::from_millis(50)).await;
    };
    let (final_a, _) = Snapshot::read(host_a, &session).await?;
    let (final_b, _) = Snapshot::read(host_b, &session).await?;
    ensure!(
        final_a.revision == final_b.revision && final_a.row_ids() == final_b.row_ids(),
        "replicas disagree on the durable head after recovery"
    );
    let final_rows = final_a.row_ids();
    ensure!(
        final_rows.iter().collect::<BTreeSet<_>>().len() == final_rows.len(),
        "the durable transcript holds a row twice"
    );
    ensure!(
        final_rows.starts_with(&before.row_ids()),
        "recovery rewrote rows committed before the turn"
    );
    let rows = final_a.rows.as_slice();
    let users = rows
        .iter()
        .filter(|row| row["kind"] == "user" && row.to_string().contains(QUESTION))
        .count();
    let replies: Vec<&Value> = rows
        .iter()
        .filter(|row| row["kind"] == "assistant_reply" && row["suppressed"].is_null())
        .collect();
    ensure!(users == 1, "the turn's input committed {users} times");
    ensure!(
        replies.len() == 1
            && replies[0]["content"]["text"] == ANSWER
            && replies[0]["provenance"]["turn_id"] == turn.as_str(),
        "the turn did not commit exactly one reply of its own: {replies:?}"
    );

    // Each of A's feeds converges on the durable rows, every row once and
    // in order: live through a shared store, through the durable head when
    // the store is process-local.
    let mut reports = Vec::new();
    for index in [0, 2] {
        let observer = &observers[index];
        let grace = Instant::now() + Duration::from_secs(if variant.shared { 30 } else { 2 });
        let _ = observer
            .wait(grace.min(deadline), |frames| {
                fold(&observer.start, frames, &turn, &final_rows)
                    .is_ok_and(|folded| folded.converged(&final_rows, final_a.revision))
            })
            .await;
        let arrivals = observer.arrivals();
        let mut frames: Vec<Value> = arrivals.iter().map(|(_, frame)| frame.clone()).collect();
        let live = fold(&observer.start, &frames, &turn, &final_rows)?;
        let reconnected = if live.converged(&final_rows, final_a.revision) {
            false
        } else if variant.shared {
            bail!(
                "{}: a shared live replay store's feed did not converge: {:?}",
                observer.label,
                live.rows
            );
        } else {
            let start = Snapshot {
                cursor: observer.last_cursor(),
                ..observer.start.clone()
            };
            let again = Observer::follow(
                &format!("{}-reconnected", observer.label),
                host_a,
                &observer.session,
                start,
            )
            .await?;
            again
                .wait(deadline, |frames| frames.len() >= 2)
                .await
                .context("the reconnected feed answered nothing after its cursor")?;
            let tail = again.close();
            write(
                directory,
                &format!("{}-frames.json", again.label),
                &json!(tail),
            )?;
            ensure!(
                tail[1]["type"] == "replay_gap",
                "{}: a cursor behind the durable head did not rebuild from it: {}",
                observer.label,
                tail[1]
            );
            frames.extend(tail.into_iter().skip(1));
            true
        };
        let folded = fold(&observer.start, &frames, &turn, &final_rows)?;
        ensure!(
            folded.converged(&final_rows, final_a.revision),
            "{}: the feed did not converge on the durable rows",
            observer.label
        );
        if variant.shared {
            folded.gaps_only_from_kills(&observer.label, &arrivals, &kills)?;
        } else {
            ensure!(
                folded.activity == 0,
                "{}: B's live activity reached A through a process-local store",
                observer.label
            );
        }
        if index == 0 && variant.shared {
            ensure!(
                folded.activity_before_commit > 0,
                "{}: no live activity preceded the turn's commit",
                observer.label
            );
        }
        reports.push(json!({
            "observer": observer.label,
            "start_revision": observer.start.revision,
            "start_rows": observer.start.rows.len(),
            "committed": folded.committed,
            "activity": folded.activity,
            "activity_before_commit": folded.activity_before_commit,
            "gaps": folded.gaps,
            "reconnected": reconnected,
        }));
    }
    let b_activity = observers[1]
        .frames()
        .iter()
        .filter(|frame| is_turn_activity(frame, &turn))
        .count();

    // The turn ran on B: every Run invocation of the case, the killed
    // attempt's included, is pinned to B's deployment and completed.
    let invocations = run_invocations(view).await?;
    ensure!(
        !invocations.is_empty()
            && invocations.iter().all(|invocation| {
                invocation["pinned_deployment_id"] == deployment_b.as_str()
                    && invocation["status"] == "completed"
            }),
        "the turn's Run did not execute on B ({deployment_b}): {invocations:?}"
    );
    evidence.journals = turn_journals(view).await?;
    evidence.stores = vec![
        final_state.clone(),
        json!({"kind": "s37_run_invocations", "invocations": invocations,
               "deployments": {"a": deployment_a, "b": deployment_b}}),
    ];
    write(
        directory,
        "scorecard.json",
        &json!({
            "scenario": "S37",
            "variant": variant.id,
            "live_replay_store": variant.live_replay_store,
            "selected": 1,
            "executed": 1,
            "verdict": "PASS",
            "turn": turn,
            "final_revision": final_a.revision,
            "final_rows": final_rows,
            "b_live_activity": b_activity,
            "observers": reports,
            "killed": killed,
            "restarted": restarted,
        }),
    )?;
    Ok(())
}
