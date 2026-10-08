//! The workbench page against its API and store: S29 (browser, API and
//! store agree after a real host kill).

use std::process::Stdio;

use anyhow::{Context as _, Result, ensure};
use lash_e2e::{Case, Host, NodeOptions, ProviderReply};
use serde_json::{Value, json};

use crate::support::{self, Turn};
use crate::workbench;

case!(
    s29_browser_api_and_store_agree_after_a_host_kill,
    SqliteFile,
    Live,
    s29
);
case!(
    s29_browser_api_and_store_agree_after_a_host_kill_postgresql,
    Postgresql,
    Live,
    s29
);

const STREAM: &str = "answer-started";

/// Node options for a Standard workbench whose production OpenAI-compatible
/// client calls the case's recorded provider.
fn recorded(case: &Case) -> NodeOptions {
    NodeOptions {
        env: vec![
            ("AGENT_WORKBENCH_PROTOCOL".to_owned(), "standard".to_owned()),
            (
                "AGENT_WORKBENCH_PROVIDER_URL".to_owned(),
                case.control.provider_url(),
            ),
            // The recorded provider takes any bearer; the client sends one.
            (
                "OPENROUTER_API_KEY".to_owned(),
                "e2e-recorded-provider".to_owned(),
            ),
        ],
        ..NodeOptions::default()
    }
}

/// The session's state through `node`.
async fn state(case: &Case, node: &str) -> Result<Value> {
    let session = support::session(case);
    case.node(node)?
        .get(&format!("/api/state?session_id={session}"))
        .await
}

/// The shown transcript rows of `state`.
fn shown(state: &Value) -> Vec<Value> {
    state["transcript"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["suppressed"].is_null())
        .cloned()
        .collect()
}

/// Open the session's page through `node` in a fresh headless browser and
/// read its timeline once it shows `rows` transcript rows.
async fn browse(case: &mut Case, node: &str, name: &str, rows: usize) -> Result<Value> {
    let python = std::env::var("LASH_E2E_PYTHON").context(
        "LASH_E2E_PYTHON is required; a case without its runner's setup is not a passing case",
    )?;
    let repo = std::env::var("LASH_E2E_REPO").context("LASH_E2E_REPO is required")?;
    let script = std::path::Path::new(&repo).join("crates/lash-e2e/tests/e2e/timeline.py");
    let out = case.dir.join(format!("{name}.json"));
    let mut command = tokio::process::Command::new(python);
    command
        .arg(&script)
        .arg(&case.node(node)?.url)
        .arg(support::session(case))
        .arg(rows.to_string())
        .arg(&out)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(
            case.dir.join(format!("{name}.stdout")),
        )?)
        .stderr(std::fs::File::create(
            case.dir.join(format!("{name}.stderr")),
        )?)
        .kill_on_drop(true);
    let status = tokio::time::timeout_at(case.deadline.into(), command.status())
        .await
        .context("the browser did not finish by the case deadline")??;
    ensure!(status.success(), "the browser {name} exited {status}");
    case.evidence.cleanup.push(lash_e2e::CleanupReceipt {
        resource: format!("browser {name}"),
        closed: true,
        detail: "exited 0".to_owned(),
    });
    let page: Value = serde_json::from_slice(&std::fs::read(&out)?)?;
    case.evidence
        .outputs
        .push(json!({"browser": name, "page": page}));
    Ok(page)
}

/// How many timeline nodes the page draws for `rows`: one per row, plus
/// one per reasoning block, and a reasoning row is only its blocks.
fn drawn(rows: &[Value]) -> usize {
    rows.iter()
        .map(|row| {
            row["content"]["reasoning"].as_array().map_or(0, Vec::len)
                + usize::from(row["kind"] != "reasoning")
        })
        .sum()
}

/// Whether the page shows each of `rows` under its id and turn, with the
/// committed text of every input, tool row and reply.
fn agrees(page: &Value, rows: &[Value]) -> Result<()> {
    let nodes = page["rows"].as_array().context("the page read no rows")?;
    for row in rows {
        let id = &row["row_id"];
        let drawn: Vec<&Value> = nodes.iter().filter(|node| node["id"] == *id).collect();
        let expected = drawn_count(row);
        ensure!(
            drawn.len() == expected,
            "the page draws row {id} {} times, not {expected}",
            drawn.len()
        );
        let turn = row["provenance"]["turn_id"].as_str().unwrap_or_default();
        for node in &drawn {
            ensure!(
                node["turn"] == turn,
                "the page attributes row {id} to {}",
                node["turn"]
            );
        }
        match row["kind"].as_str() {
            Some("user") => ensure!(
                drawn[0]["role"] == "user" && drawn[0]["text"] == row["content"]["text"],
                "the page shows input {id} as {}",
                drawn[0]
            ),
            Some("assistant_reply") => ensure!(
                drawn[0]["role"] == "assistant"
                    && drawn[0]["text"].as_str().is_some_and(|text| text.trim()
                        == row["content"]["text"].as_str().unwrap_or_default().trim()),
                "the page shows reply {id} as {}",
                drawn[0]
            ),
            Some("tool_call") => ensure!(
                drawn[0]["text"].as_str().is_some_and(
                    |text| text.ends_with(row["content"]["text"].as_str().unwrap_or_default())
                ),
                "the page shows tool row {id} as {}",
                drawn[0]
            ),
            _ => {}
        }
    }
    ensure!(
        nodes.len() == drawn(rows),
        "the page draws {} rows for {} committed",
        nodes.len(),
        drawn(rows)
    );
    Ok(())
}

fn drawn_count(row: &Value) -> usize {
    drawn(std::slice::from_ref(row))
}

/// One turn reply attributed to each of `turns`, and one input each.
fn attributed(rows: &[Value], turns: &[(&Turn, &str)]) -> Result<()> {
    let replies: Vec<&Value> = rows
        .iter()
        .filter(|row| row["kind"] == "assistant_reply")
        .collect();
    let inputs: Vec<&Value> = rows.iter().filter(|row| row["kind"] == "user").collect();
    ensure!(
        replies.len() == turns.len() && inputs.len() == turns.len(),
        "the transcript holds {} replies and {} inputs for {} turns",
        replies.len(),
        inputs.len(),
        turns.len()
    );
    for (turn, text) in turns {
        let of: Vec<&&Value> = replies
            .iter()
            .filter(|row| row["provenance"]["turn_id"] == turn.run.as_str())
            .collect();
        ensure!(
            of.len() == 1
                && of[0]["provenance"]["is_turn_reply"] == true
                && of[0]["content"]["text"] == *text,
            "turn {} has replies {of:?}",
            turn.run
        );
    }
    Ok(())
}

/// The answers of the session's terminal trace records, in order. The
/// workbench writes one per turn it settles, naming the session and the
/// committed outcome.
fn terminals(case: &Case) -> Result<Vec<Value>> {
    let session = support::session(case);
    Ok(
        lash_e2e::read_jsonl(&case.dir.join(data_dir(case)).join("trace.jsonl"))?
            .into_iter()
            .filter(|record| {
                record["name"] == "agent_workbench.user_turn.completed"
                    && record["context"]["session_id"] == session.as_str()
            })
            .map(|record| {
                record["payload"]["outcome"]["finished"]["assistant_message"]["text"].clone()
            })
            .collect(),
    )
}

fn data_dir(case: &Case) -> &'static str {
    match case.store {
        lash_e2e::Store::Postgresql => "data-node-a",
        _ => "data",
    }
}

/// A turn calls a tool with a known output (a fresh session has no process
/// handles) and answers; the next turn's provider stream enters its first
/// delta and the host is SIGKILLed there. A restart of the same node over
/// the same store finishes the turn. Then
/// two browsers that connect only after the restart each show exactly the
/// committed transcript the API answers: one input and one attributed reply
/// per turn, never a second copy of a reply while loading. The trace has one
/// terminal per turn, and the store ends both turns answered.
async fn s29(case: &mut Case) -> Result<()> {
    let answer = ProviderReply::answer(&["second ", "answer"], (17, 2));
    case.control.script_provider(vec![
        ProviderReply::call("call-handles", "list_process_handles", json!({}), (11, 2)),
        ProviderReply::answer(&["no ", "processes"], (13, 2)),
        ProviderReply {
            hold: Some((1, STREAM.to_owned())),
            ..answer.clone()
        },
        answer,
    ]);
    let options = recorded(case);
    case.boot(Host::Workbench, "node-a", options.clone())
        .await?;
    let first = workbench::send(case, "node-a", "S29 list the processes").await?;
    let outcome = workbench::follow(case, "node-a", &first).await?;
    let (kind, reply) = support::settled(&outcome);
    ensure!(
        kind == "completed" && reply.as_deref() == Some("no processes"),
        "the first turn settled {kind} {reply:?}"
    );
    let tool: Vec<&Value> = outcome["report"]["activities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|activity| {
            activity["type"] == "tool_call_completed" && activity["name"] == "list_process_handles"
        })
        .collect();
    case.evidence
        .outputs
        .push(json!({"known tool output": tool}));
    ensure!(
        tool.len() == 1
            && tool[0]["output"]["outcome"] == json!({"payload": [], "status": "success"}),
        "the tool call completed as {tool:?}"
    );
    case.until("no turn is left active", || async {
        Ok(state(case, "node-a").await?["active_turns"]
            .as_array()
            .is_some_and(Vec::is_empty)
            .then_some(()))
    })
    .await?;
    let second = workbench::send(case, "node-a", "S29 answer after a kill").await?;
    case.control.wait_held(STREAM, 1, case.deadline).await?;
    case.record_barrier(json!({"barrier": "provider stream entered", "held": STREAM}));
    case.kill("node-a", "the provider stream entered its first delta")
        .await?;
    case.boot(Host::Workbench, "node-a", options).await?;
    let outcome = workbench::follow(case, "node-a", &second).await?;
    let (kind, reply) = support::settled(&outcome);
    ensure!(
        kind == "completed" && reply.as_deref() == Some("second answer"),
        "the resumed turn settled {kind} {reply:?}"
    );
    let requests = case.control.provider_requests();
    case.evidence
        .effects
        .push(json!({"provider requests": requests.len()}));
    ensure!(
        requests.len() == 4 && requests[2]["body"] == requests[3]["body"],
        "the provider saw {} requests, the killed one not repeated as itself",
        requests.len()
    );
    let api = case
        .until("the restarted node settled its claims", || async {
            let state = state(case, "node-a").await?;
            Ok(state["active_turns"]
                .as_array()
                .is_some_and(Vec::is_empty)
                .then_some(state))
        })
        .await?;
    let rows = shown(&api);
    case.evidence
        .stores
        .push(json!({"at": "after the restart", "api transcript": rows}));
    attributed(
        &rows,
        &[(&first, "no processes"), (&second, "second answer")],
    )?;
    ensure!(
        api["pending_turn_inputs"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|input| input["status"]["kind"] != "open"),
        "an input is still open: {}",
        api["pending_turn_inputs"]
    );
    for name in ["late-browser-1", "late-browser-2"] {
        let page = browse(case, "node-a", name, drawn(&rows)).await?;
        agrees(&page, &rows).with_context(|| format!("{name} disagrees with the API"))?;
        ensure!(
            page["most_assistants"].as_u64() <= Some(2),
            "{name} showed {} replies at once",
            page["most_assistants"]
        );
    }
    let again = shown(&state(case, "node-a").await?);
    ensure!(
        again == rows,
        "the API transcript changed while browsers read it"
    );
    let terminal = terminals(case)?;
    ensure!(
        terminal == [json!("no processes"), json!("second answer")],
        "the trace's turn terminals answer {terminal:?}, not one per turn"
    );
    case.stop("node-a").await?;
    for turn in [&first, &second] {
        let facts = support::record_facts(case, &turn.run, "after both turns").await?;
        ensure!(
            facts["end"]["kind"] == "Answered" && facts["unfinished"].is_null(),
            "the store's end of {} is {facts}",
            turn.run
        );
    }
    Ok(())
}
