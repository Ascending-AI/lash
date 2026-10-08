//! Session feeds across workbench nodes: S37 (a turn on one node reaches
//! another node's feed).

use anyhow::{Result, ensure};
use lash_e2e::{Case, Host, NodeOptions, ProviderReply};
use serde_json::{Value, json};

use crate::support;
use crate::workbench;

/// An RLM cell that finishes with `value`, as the provider's text deltas.
fn cell(value: &str) -> Vec<String> {
    vec![
        "<typescript>\n".to_owned(),
        format!("finish('{value}');\n</typescript>"),
    ]
}

fn recorded_rlm(case: &Case) -> NodeOptions {
    NodeOptions {
        env: vec![
            ("AGENT_WORKBENCH_PROTOCOL".to_owned(), "rlm".to_owned()),
            (
                "AGENT_WORKBENCH_PROVIDER_URL".to_owned(),
                case.control.provider_url(),
            ),
            (
                "OPENROUTER_API_KEY".to_owned(),
                "e2e-recorded-provider".to_owned(),
            ),
        ],
        ..NodeOptions::default()
    }
}

/// The barrier the turn's held stream waits at.
const STREAM: &str = "s37-stream";

case!(
    s37_a_turn_on_one_node_reaches_another_nodes_feed_memory_replay,
    Postgresql,
    Live,
    s37
);
case!(
    s37_a_turn_on_one_node_reaches_another_nodes_feed_live_replay,
    Postgresql,
    Live,
    s37
);

/// The cursor's revision and sequence, `lashsc2:<id>:<revision>:<sequence>:<session>`.
fn position(cursor: &Value) -> Option<(u64, u64)> {
    let mut parts = cursor.as_str()?.split(':').skip(2);
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// A feed line's cursor.
fn cursor(line: &Value) -> &Value {
    if line["cursor"].is_string() {
        &line["cursor"]
    } else {
        &line["event"]["cursor"]
    }
}

/// Whether `line` carries live model stream activity (not a durable row).
fn streamed(line: &Value) -> bool {
    line["type"] == "observation" && line["event"]["activity"]["type"] == "stream_block_started"
}

/// The workbench transcript `node` serves.
async fn transcript(case: &Case, node: &str) -> Result<Value> {
    let session = support::session(case);
    let state = case
        .node(node)?
        .get(&format!("/api/state?session_id={session}"))
        .await?;
    Ok(state["transcript"].clone())
}

/// Two workbench nodes serve one PostgreSQL store. B owns the session, so
/// the turn A accepts executes on B; observers follow the session feed on A
/// from before the turn and from mid-turn, and on B. The provider stream is
/// held mid-turn and B is SIGKILLed under it; A reaps B and resumes the
/// turn from committed state, calling the model once more. Both nodes then
/// serve one durable head, and every A feed converges on the turn's
/// terminal once, in order. With the process-local memory live replay
/// store B's live activity reaches B's feed and never A's, and A's feeds
/// report the span they never held once, as an unavailable replay gap
/// (FIG-5399). With the shared PostgreSQL store (FIG-5101) A's feed from
/// before the turn carries B's live activity, one attempt reset retracts
/// it before A's own streams, and no gap is reported (FIG-5366).
async fn s37(case: &mut Case) -> Result<()> {
    let reply = |value: &str| ProviderReply {
        status: 200,
        text: cell(value),
        usage: (11, 2),
        ..ProviderReply::default()
    };
    case.control.script_provider(vec![
        reply("warm"),
        ProviderReply {
            hold: Some((1, STREAM.to_owned())),
            ..reply("fed")
        },
        reply("fed"),
    ]);
    let options = recorded_rlm(case);
    case.boot(Host::Workbench, "node-b", options.clone())
        .await?;
    let warm = workbench::send(case, "node-b", "warm the session on B").await?;
    let outcome = workbench::follow(case, "node-b", &warm).await?;
    ensure!(
        support::settled(&outcome).0 == "completed",
        "the warm turn settled {outcome}"
    );
    case.boot(Host::Workbench, "node-a", options.clone())
        .await?;
    let before = workbench::observe(case, "node-a").await?;
    let on_b = workbench::observe(case, "node-b").await?;
    let turn = workbench::send(case, "node-a", "feed the other node").await?;
    case.control.wait_held(STREAM, 1, case.deadline).await?;
    let mid = workbench::observe(case, "node-a").await?;
    // B's live activity has reached B's own feed before B dies.
    case.until("B's feed shows its live stream", || async {
        Ok(on_b.lines().iter().any(streamed).then_some(()))
    })
    .await?;
    case.kill("node-b", "the provider stream held mid-turn")
        .await?;
    case.control.release(STREAM);
    let outcome = workbench::follow(case, "node-a", &turn).await?;
    let (kind, reply) = support::settled(&outcome);
    ensure!(
        kind == "completed" && reply.as_deref() == Some("fed"),
        "the turn settled {kind} {reply:?}: {outcome}"
    );
    let ended = |feed: &workbench::Feed| {
        feed.lines()
            .iter()
            .any(|line| line["type"] == "terminal_replacement" && line.to_string().contains("fed"))
    };
    case.until("A's feeds reach the terminal", || async {
        Ok((ended(&before) && ended(&mid)).then_some(()))
    })
    .await?;
    let feeds = [("before", before.stop()), ("mid", mid.stop())];
    let on_b = on_b.stop();
    case.evidence.outputs.push(json!({
        "feeds": {"before": feeds[0].1, "mid": feeds[1].1, "on_b": on_b},
    }));
    // The turn ran on B, then on A from committed state.
    let ledger = |node: &str| lash_e2e::read_jsonl(&case.dir.join(format!("commits-{node}.jsonl")));
    let (a, b) = (ledger("node-a")?, ledger("node-b")?);
    let labels =
        |lines: &[Value], label: &str| lines.iter().filter(|line| line["label"] == label).count();
    ensure!(
        labels(&b, "turn.admit") == 2
            && labels(&b, "model.start") == 2
            && labels(&b, "turn.commit") == 1,
        "B did not admit and start the turn A accepted: {b:?}"
    );
    ensure!(
        a.iter()
            .any(|line| line["label"] == "reap" && line["from"] == "node-b")
            && labels(&a, "turn.admit") == 0
            && labels(&a, "model.start") == 1
            && labels(&a, "turn.commit") == 1,
        "A did not reap B and resume the turn once: {a:?}"
    );
    let requests = case.control.provider_requests().len();
    ensure!(
        requests == 3,
        "the provider saw {requests} requests, not the warm turn, B's held call and A's one resumed call"
    );
    // Every A feed converges on the durable rows once, in order.
    for (name, lines) in &feeds {
        let positions: Vec<_> = lines
            .iter()
            .filter_map(|line| position(cursor(line)))
            .collect();
        ensure!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "A's {name} feed went backwards or repeated: {positions:?}"
        );
        let terminals = lines
            .iter()
            .filter(|line| {
                line["type"] == "terminal_replacement" && line.to_string().contains("fed")
            })
            .count();
        ensure!(
            terminals == 1,
            "A's {name} feed holds {terminals} terminals of the turn"
        );
        let gaps: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line["type"] == "replay_gap")
            .map(|(index, _)| index)
            .collect();
        let resets: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line["event"]["activity"]["type"] == "model_attempt_reset")
            .map(|(index, _)| index)
            .collect();
        if case.live_replay {
            // The shared store holds B's attempt: the takeover retracts it
            // with one reset and streams A's own attempt after it, and
            // nothing is reported missing.
            ensure!(
                gaps.is_empty(),
                "A's {name} feed reported a replay gap on the shared live replay store: {lines:?}"
            );
            ensure!(
                resets.len() == 1 && lines[resets[0]..].iter().any(streamed),
                "A's {name} feed holds {} attempt resets, not one followed by A's attempt: {lines:?}",
                resets.len()
            );
            if *name == "before" {
                ensure!(
                    lines[..resets[0]].iter().any(streamed),
                    "A's {name} feed never carried B's live activity"
                );
            }
        } else {
            // A's own store never held B's attempt, and a reset cannot
            // certify it from the turn marker A re-created on resume
            // (FIG-5399): the feed reports the missing span once, typed
            // unavailable, and converges through the durable head.
            ensure!(
                gaps.len() == 1
                    && lines[gaps[0]]["gap"]["reason"] == "unavailable"
                    && resets.is_empty(),
                "A's {name} feed holds {} replay gaps and {} resets, not one unavailable gap: {lines:?}",
                gaps.len(),
                resets.len()
            );
            ensure!(
                !lines[..gaps[0]].iter().any(streamed),
                "B's live activity reached A's {name} feed: {lines:?}"
            );
        }
    }
    // Both nodes serve one durable head.
    case.boot(Host::Workbench, "node-b", options).await?;
    let (on_a, again_b) = (
        transcript(case, "node-a").await?,
        transcript(case, "node-b").await?,
    );
    ensure!(
        on_a == again_b && on_a.as_array().is_some_and(|rows| !rows.is_empty()),
        "the nodes serve different heads: {on_a} and {again_b}"
    );
    case.stop("node-a").await?;
    case.stop("node-b").await?;
    let facts = support::record_facts(case, &turn.run, "after the turn").await?;
    ensure!(
        facts["end"]["kind"] == "Answered",
        "the store's end is {facts}"
    );
    Ok(())
}
