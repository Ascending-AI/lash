//! MCP integrations on a workbench node: S28 (attach and detach, and
//! attachments, survive a reconnect). The node's MCP peers are the
//! workbench's own fixture server: its stdio child, and an HTTP peer the
//! case runs and attaches through the operator routes.

use anyhow::{Context as _, Result, ensure};
use lash::tools::{ToolCallOutcome, ToolCallOutput};
use lash_e2e::{Case, Host, Leg, NodeOptions};
use serde_json::{Value, json};

use crate::support::{self, Turn, successor};
use crate::workbench;

case!(
    s28_mcp_attach_detach_and_attachments_survive_reconnect,
    SqliteFile,
    Live,
    s28
);
case!(
    s28_mcp_attach_detach_and_attachments_survive_reconnect_resume,
    SqliteFile,
    Resume,
    s28
);
case!(
    s28_mcp_attach_detach_and_attachments_survive_reconnect_postgresql,
    Postgresql,
    Live,
    s28
);
case!(
    s28_mcp_attach_detach_and_attachments_survive_reconnect_postgresql_resume,
    Postgresql,
    Resume,
    s28
);

/// The fixture's `workspace_badge` blob.
const BADGE: &[u8] = b"workbench workspace badge v1\x00\x01\x02\x03";
const BADGE_TOOL: &str = "mcp__workspace_http__workspace_badge";
const TOKEN: &str = "workbench-mcp-fixture-token";

/// The case's HTTP MCP peer: where it listens and the barrier directory its
/// first badge call blocks in.
struct HttpPeer {
    address: String,
    env: Vec<(String, String)>,
}

impl HttpPeer {
    async fn start(case: &mut Case) -> Result<Self> {
        let address = format!("127.0.0.1:{}", lash_e2e::free_port()?);
        let env = vec![
            ("AGENT_WORKBENCH_MCP_ADDR".to_owned(), address.clone()),
            (
                "AGENT_WORKBENCH_MCP_GATE_DIR".to_owned(),
                case.dir.join("mcp-gates").display().to_string(),
            ),
        ];
        let peer = Self { address, env };
        peer.restart(case).await?;
        Ok(peer)
    }

    /// Start (again) on the same address.
    async fn restart(&self, case: &mut Case) -> Result<u32> {
        case.peer(
            Host::Workbench,
            "mcp-peer",
            &["mcp-fixture", "http"],
            self.env.clone(),
            Some(&self.address),
        )
        .await
    }

    fn url(&self) -> String {
        format!("http://{}/mcp", self.address)
    }
}

/// A Workbench RLM node whose provider is the workbench's MCP development
/// scenario and whose stdio MCP peer is the workbench binary itself; with
/// `peer`, its deployment also configures the HTTP peer at boot.
fn options(case: &mut Case, node: &str, peer: Option<&HttpPeer>) -> Result<NodeOptions> {
    let binary = case.binary(Host::Workbench)?;
    let mut options = NodeOptions {
        env: vec![
            ("AGENT_WORKBENCH_PROTOCOL".to_owned(), "rlm".to_owned()),
            (
                "AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO".to_owned(),
                "mcp-fixture".to_owned(),
            ),
            (
                "AGENT_WORKBENCH_MCP_FIXTURE_BIN".to_owned(),
                binary.display().to_string(),
            ),
            (
                "AGENT_WORKBENCH_MCP_PROVIDER_LOG".to_owned(),
                case.dir
                    .join("provider-requests.jsonl")
                    .display()
                    .to_string(),
            ),
            (
                "AGENT_WORKBENCH_MCP_STDIO_PID".to_owned(),
                stdio_pid_file(case, node).display().to_string(),
            ),
        ],
        ..NodeOptions::default()
    };
    if let Some(peer) = peer {
        options.env.push((
            "AGENT_WORKBENCH_MCP_SERVERS".to_owned(),
            json!([{"name": "workspace_http", "url": peer.url(), "token": TOKEN}]).to_string(),
        ));
    }
    Ok(options)
}

fn stdio_pid_file(case: &Case, node: &str) -> std::path::PathBuf {
    let boot = case
        .evidence
        .nodes
        .iter()
        .find(|evidence| evidence.node == node)
        .map_or(0, |evidence| evidence.boots.len());
    case.dir.join(format!("stdio-{node}-{boot}.pid"))
}

async fn servers(case: &mut Case, node: &str) -> Result<Vec<Value>> {
    let servers = case.node(node)?.get("/api/mcp/servers").await?;
    case.evidence
        .outputs
        .push(json!({"node": node, "mcp servers": servers}));
    Ok(servers["servers"].as_array().cloned().unwrap_or_default())
}

async fn attach(case: &mut Case, node: &str, peer: &HttpPeer) -> Result<Value> {
    let attached = case
        .node(node)?
        .post(
            "/api/mcp/servers",
            &json!({"name": "workspace_http", "url": peer.url(), "token": TOKEN}),
        )
        .await?;
    case.evidence
        .effects
        .push(json!({"node": node, "attached": attached}));
    ensure!(
        attached["connected"] == true
            && attached["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|tool| tool == BADGE_TOOL)),
        "the attached peer offers no badge tool: {attached}"
    );
    Ok(attached)
}

async fn detach(case: &mut Case, node: &str) -> Result<()> {
    let detached = case
        .node(node)?
        .delete("/api/mcp/servers/workspace_http")
        .await?;
    case.evidence
        .effects
        .push(json!({"node": node, "detached": detached}));
    ensure!(
        detached == json!({"detached": "workspace_http"}),
        "the detach answered {detached}"
    );
    Ok(())
}

/// Turn `turn`'s settled report through `node`, requiring it finished with
/// `value`.
async fn finished(case: &mut Case, node: &str, turn: &Turn, value: &str) -> Result<Value> {
    let outcome = workbench::follow(case, node, turn).await?;
    let (kind, reply) = support::settled(&outcome);
    ensure!(
        kind == "completed" && reply.as_deref() == Some(value),
        "the turn settled {kind} with {reply:?}, not {value:?}: {outcome}"
    );
    Ok(outcome["output"].clone())
}

/// The report's `tool_call_started` and `tool_call_completed` activities
/// for tool `name`.
fn calls(report: &Value, name: &str) -> (Vec<Value>, Vec<Value>) {
    let of = |kind: &str| -> Vec<Value> {
        report["activities"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|activity| activity["type"] == kind && activity["name"] == name)
            .cloned()
            .collect()
    };
    (of("tool_call_started"), of("tool_call_completed"))
}

/// The one call of `name` the report admitted and completed, as its native
/// outcome.
fn one_call(report: &Value, name: &str) -> Result<Value> {
    let (started, completed) = calls(report, name);
    ensure!(
        started.len() == 1 && completed.len() == 1,
        "{name} was admitted {} and completed {} times",
        started.len(),
        completed.len()
    );
    ensure!(
        started[0]["call_id"] == completed[0]["call_id"],
        "{name}'s completion names another call"
    );
    Ok(completed[0]["output"].clone())
}

/// The structured success value of `name`'s one call.
fn structured(report: &Value, name: &str) -> Result<Value> {
    let output: ToolCallOutput = serde_json::from_value(one_call(report, name)?)
        .with_context(|| format!("{name}'s output is not a typed tool output"))?;
    let ToolCallOutcome::Success(value) = &output.outcome else {
        anyhow::bail!("{name} failed: {output:?}")
    };
    value
        .to_json_value()
        .get("structuredContent")
        .cloned()
        .with_context(|| format!("{name}'s success has no structured content"))
}

/// Whether an admitted badge call interrupted by its peer's death resolved,
/// or failed with the MCP plugin's typed transport failure.
fn resolved_or_typed(outcome: &Value) -> Result<bool> {
    let outcome: ToolCallOutcome = serde_json::from_value(outcome.clone())
        .context("the badge call has no typed tool outcome")?;
    Ok(match outcome {
        ToolCallOutcome::Success(_) => true,
        ToolCallOutcome::Failure(failure) => {
            let failure = failure.to_json_value();
            let raw = &failure["raw"];
            let timeout = raw["kind"] == "call_timeout"
                && failure["class"] == "timeout"
                && raw["timeout_ms"].as_u64().is_some_and(|ms| ms > 0)
                && failure["code"]
                    == if raw["deadline"] == true {
                        "mcp_call_deadline_exceeded"
                    } else {
                        "mcp_call_timeout"
                    };
            let lost = raw["kind"] == "connection_lost"
                && failure["class"] == "unavailable"
                && failure["code"] == "mcp_connection_lost"
                && matches!(
                    raw["cause"]["kind"].as_str(),
                    Some("transport_closed" | "transport_send")
                );
            failure["source"] == "plugin" && raw["server"] == "workspace_http" && (timeout || lost)
        }
        ToolCallOutcome::Cancelled(_) => false,
    })
}

/// The scripted provider's requests for the turn whose input names
/// `marker`: whether each one's instructions offered the HTTP peer.
fn offered(case: &Case, marker: &str) -> Result<Vec<bool>> {
    Ok(
        lash_e2e::read_jsonl(&case.dir.join("provider-requests.jsonl"))?
            .into_iter()
            .filter(|request| {
                request["messages"]
                    .as_array()
                    .and_then(|messages| {
                        messages
                            .iter()
                            .rev()
                            .find(|message| message["starts_user_segment"] == true)
                    })
                    .is_some_and(|input| input.to_string().contains(marker))
            })
            .map(|request| {
                request["instructions"]
                    .to_string()
                    .contains("workspace_http")
            })
            .collect(),
    )
}

/// Whether process `pid` is gone (or only awaits its reaper).
fn exited(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
        stat.rsplit(')')
            .next()
            .is_some_and(|rest| rest.trim_start().starts_with('Z'))
    })
}

/// The node runs RLM turns over the MCP fixture peers. One turn calls the
/// stdio peer's tools that call back into the host: sampling, form and URL
/// elicitation, and roots. The operator attaches the HTTP peer and a turn's
/// badge call enters it and blocks; then the peer is killed and restarted on
/// its address (live), or the node is killed and a successor, whose
/// deployment configures the peer, resumes the turn (resume). That admitted
/// call resolves or fails typed, once, and the turn settles. After a
/// catalog refresh (detach, attach) a badge call returns the badge blob as
/// one stored attachment; once the peer is detached, the next turn is not
/// offered it. A cold reboot shows the same transcript and reads the
/// attachment back with exactly the badge's bytes.
async fn s28(case: &mut Case) -> Result<()> {
    let peer = HttpPeer::start(case).await?;
    let options_a = options(case, "node-a", None)?;
    case.boot(Host::Workbench, "node-a", options_a).await?;
    let stdio = servers(case, "node-a").await?;
    ensure!(
        stdio
            .iter()
            .any(|server| server["name"] == "workspace_stdio" && server["connected"] == true)
            && !stdio
                .iter()
                .any(|server| server["name"] == "workspace_http"),
        "the node's peers at boot are {stdio:?}"
    );

    // Host sampling, elicitation and roots, through the stdio peer.
    let depth = workbench::send(case, "node-a", "S28 MCP-DEPTH").await?;
    let report = finished(case, "node-a", &depth, "Host-generated summary.").await?;
    let summary = structured(&report, "mcp__workspace_stdio__sample_summary")?;
    ensure!(
        summary == json!({"model": "dev/failure-paths", "summary": "Host-generated summary."}),
        "the sampled summary is {summary}"
    );
    let sampling: Vec<Value> = lash_e2e::read_jsonl(&case.dir.join("provider-requests.jsonl"))?
        .into_iter()
        .filter(|request| request["model"]["model"]["key"] == "mcp-sampling")
        .collect();
    ensure!(
        sampling.len() == 1
            && sampling[0]["messages"]
                .to_string()
                .contains("Summarize this in one short sentence: workbench"),
        "the host made {} sampling requests: {sampling:?}",
        sampling.len()
    );
    let form = structured(&report, "mcp__workspace_stdio__elicit_confirmation")?;
    ensure!(
        form == json!({"action": "accept", "answer": "yes"}),
        "the form elicitation answered {form}"
    );
    let url = structured(&report, "mcp__workspace_stdio__elicit_via_url")?;
    ensure!(
        url == json!({"action": "accept", "completion_notified": true, "elicitation_id": "workbench-demo-url-1"}),
        "the URL elicitation answered {url}"
    );
    let roots = structured(&report, "mcp__workspace_stdio__list_host_roots")?;
    ensure!(
        roots["roots"]
            .as_array()
            .is_some_and(|roots| roots.len() == 1
                && roots[0]["name"] == "workbench"
                && roots[0]["uri"]
                    .as_str()
                    .is_some_and(|uri| uri.starts_with("file://"))),
        "the host's roots are {roots}"
    );

    // An admitted badge call whose peer (live) or host (resume) dies.
    attach(case, "node-a", &peer).await?;
    let interrupted = workbench::send(case, "node-a", "S28 MCP-RECONNECT").await?;
    let entered = case.dir.join("mcp-gates/badge-entered");
    case.until("the badge call entered the peer", || async {
        Ok(entered.exists().then_some(()))
    })
    .await?;
    case.record_barrier(json!({"barrier": "badge call entered the HTTP peer"}));
    let node = successor(case);
    match case.leg {
        Leg::Live => {
            case.kill_peer("mcp-peer", "badge call entered").await?;
            peer.restart(case).await?;
        }
        Leg::Resume => {
            case.kill("node-a", "badge call entered").await?;
            let options_b = options(case, node, Some(&peer))?;
            case.boot(Host::Workbench, node, options_b).await?;
        }
    }
    let outcome = workbench::follow(case, node, &interrupted).await?;
    let (kind, _) = support::settled(&outcome);
    ensure!(
        kind == "completed",
        "the interrupted turn settled {kind}: {outcome}"
    );
    let executions = badge_calls(case)?;
    match case.leg {
        Leg::Live => {
            // The live node's own report has the call's native outcome.
            let badge_call = one_call(&outcome["output"], BADGE_TOOL)?;
            ensure!(
                resolved_or_typed(&badge_call["outcome"])?,
                "the interrupted badge call neither resolved nor failed typed: {badge_call}"
            );
            let resolved = badge_call["outcome"]["status"] == "success";
            ensure!(
                executions == if resolved { 2 } else { 1 },
                "the peer ran the badge body {executions} times for one call"
            );
        }
        Leg::Resume => {
            // A started call without an outcome records Interrupted on the
            // successor and never runs again (ADR 0132 NR-2).
            let cells: Vec<&Value> = outcome["output"]["activities"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|activity| {
                    activity["type"] == "code_block_completed"
                        && activity["tool_call_ids"]
                            .as_array()
                            .is_some_and(|calls| !calls.is_empty())
                })
                .collect();
            ensure!(
                cells.len() == 1
                    && cells[0]["tool_call_ids"].as_array().map(Vec::len) == Some(1)
                    && cells[0]["result"]["kind"] == "failed"
                    && cells[0]["result"]["value"]["message"]
                        .as_str()
                        .is_some_and(|message| {
                            message.contains("tool was interrupted by a runtime restart")
                        }),
                "the successor did not record the badge call interrupted: {cells:?}"
            );
            ensure!(
                executions == 1,
                "the badge body ran {executions} times; a started call without an outcome never runs again"
            );
        }
    }

    // A catalog refresh, a badge attachment, and a detach.
    detach(case, node).await?;
    attach(case, node, &peer).await?;
    let badge = workbench::send(case, node, "S28 MCP-ATTACH").await?;
    let report = finished(case, node, &badge, "workspace badge came back").await?;
    let reference = attachment(&report)?;
    detach(case, node).await?;
    let after = servers(case, node).await?;
    ensure!(
        !after
            .iter()
            .any(|server| server["name"] == "workspace_http"),
        "the detached peer is still listed: {after:?}"
    );
    let detached = workbench::send(case, node, "S28 MCP-DETACHED").await?;
    let report = finished(case, node, &detached, "badge tool is detached").await?;
    ensure!(
        calls(&report, BADGE_TOOL).0.is_empty(),
        "the detached tool was called"
    );
    let during = offered(case, "MCP-ATTACH")?;
    let later = offered(case, "MCP-DETACHED")?;
    case.evidence
        .outputs
        .push(json!({"offered while attached": during, "offered after detach": later}));
    ensure!(
        !during.is_empty() && during.iter().all(|offered| *offered),
        "the attached peer was not offered: {during:?}"
    );
    ensure!(
        !later.is_empty() && later.iter().all(|offered| !offered),
        "the detached peer was still offered: {later:?}"
    );
    ensure!(
        badge_calls(case)? == executions + 1,
        "the attached badge call ran its body other than once"
    );
    retrieved(case, node, &reference).await?;

    // A cold reboot keeps the committed transcript and the attachment's
    // bytes.
    let before = transcript(case, node).await?;
    case.stop(node).await?;
    let options_c = options(case, node, (case.leg == Leg::Resume).then_some(&peer))?;
    case.boot(Host::Workbench, node, options_c).await?;
    let rebooted = transcript(case, node).await?;
    ensure!(
        rebooted == before,
        "the rebooted node shows another transcript: {rebooted:?}"
    );
    retrieved(case, node, &reference).await?;
    case.stop(node).await?;
    for entry in std::fs::read_dir(&case.dir)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if name.starts_with("stdio-") && name.ends_with(".pid") {
            let pid = std::fs::read_to_string(&path)?;
            ensure!(exited(pid.trim()), "the stdio peer {pid} outlived its node");
            case.evidence.cleanup.push(lash_e2e::CleanupReceipt {
                resource: format!("stdio MCP peer {}", pid.trim()),
                closed: true,
                detail: "exited with its node".to_owned(),
            });
        }
    }
    Ok(())
}

/// How many times the HTTP peer has run the badge body.
fn badge_calls(case: &Case) -> Result<usize> {
    match std::fs::read_to_string(case.dir.join("mcp-gates/badge-calls")) {
        Ok(text) => Ok(text.lines().filter(|line| !line.trim().is_empty()).count()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

/// The session's shown transcript rows through `node`: kind, turn and text.
async fn transcript(case: &mut Case, node: &str) -> Result<Vec<Value>> {
    let session = support::session(case);
    let state = case
        .node(node)?
        .get(&format!("/api/state?session_id={session}"))
        .await?;
    let rows: Vec<Value> = state["transcript"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["suppressed"].is_null() && row["kind"] != "event")
        .map(|row| {
            json!({
                "kind": row["kind"],
                "turn": row["provenance"]["turn_id"],
                "text": row["content"]["text"],
                "code": row["content"]["code"],
                "tools": row["content"]["tools"],
            })
        })
        .collect();
    case.evidence
        .outputs
        .push(json!({"node": node, "transcript": rows}));
    Ok(rows)
}

/// The one stored attachment reference the badge turn's one call returned.
fn attachment(report: &Value) -> Result<Value> {
    let output = one_call(report, BADGE_TOOL)?;
    ensure!(
        output["outcome"]["status"] == "success",
        "the badge call failed: {output}"
    );
    let blocks: Vec<&Value> = output["view"]["blocks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "attachment")
        .collect();
    ensure!(
        blocks.len() == 1 && blocks[0]["reference"].is_object(),
        "the badge call presented {} attachments: {output}",
        blocks.len()
    );
    let reference = blocks[0]["reference"].clone();
    ensure!(
        reference["byte_len"] == BADGE.len()
            && reference["media_type"] == "application/octet-stream",
        "the badge attachment is {reference}"
    );
    Ok(reference)
}

/// Read attachment `reference` back through `node`: exactly the badge.
async fn retrieved(case: &mut Case, node: &str, reference: &Value) -> Result<()> {
    let id = reference["id"]
        .as_str()
        .context("the reference has no id")?;
    let (bytes, media) = case
        .node(node)?
        .bytes(&format!("/api/attachments/{id}"))
        .await?;
    case.evidence.stores.push(json!({
        "node": node,
        "attachment": reference,
        "retrieved": bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
        "media_type": media,
    }));
    ensure!(
        bytes == BADGE && media == "application/octet-stream",
        "the attachment read back {} bytes of {media}",
        bytes.len()
    );
    Ok(())
}
