//! The workbench's production provider client against the case's recorded
//! provider: S26 (streaming retries produce one logical answer) and S27 (a
//! provider authentication failure permits the next Run).

use anyhow::{Result, ensure};
use lash_e2e::{Case, Host, Leg, NodeOptions, ProviderReply};
use serde_json::{Value, json};

use crate::support::{self, successor};
use crate::workbench;

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

/// Whether product stream line `line` records a model call of turn `run`.
fn model_call_of(line: &Value, run: &str) -> bool {
    line["event"]["type"] == "model_call_recorded"
        && line["event"]["event_id"]
            .as_str()
            .is_some_and(|id| id.contains(run))
}

/// The product stream's model call records of settled turn `run`, read by
/// a late observer.
async fn model_calls(case: &mut Case, node: &str, run: &str) -> Result<Vec<Value>> {
    let lines = workbench::product_events(case, node, |line| model_call_of(line, run)).await?;
    let records: Vec<Value> = lines
        .iter()
        .filter(|line| model_call_of(line, run))
        .map(|line| line["event"]["record"].clone())
        .collect();
    case.evidence
        .outputs
        .push(json!({"node": node, "run": run, "product_stream": lines}));
    Ok(records)
}

/// The recorded provider's requests so far, kept as evidence.
fn requests(case: &mut Case) -> Vec<Value> {
    let requests = case.control.provider_requests();
    case.evidence
        .effects
        .push(json!({"provider_requests": requests.len(), "endpoints": requests.iter().map(|request| request["endpoint"].clone()).collect::<Vec<_>>()}));
    requests
}

case!(
    s27_provider_authentication_failure_permits_next_run,
    SqliteFile,
    Live,
    s27
);
case!(
    s27_provider_authentication_failure_permits_next_run_resume,
    SqliteFile,
    Resume,
    s27
);

/// The provider refuses the first Run's request with a recorded 401. The
/// Run fails with the provider's classification, not cancelled, and is not
/// retried. A fresh request afterwards (on the resume leg, on another node
/// after the first is killed) answers; no active turn is left stuck.
async fn s27(case: &mut Case) -> Result<()> {
    case.control.script_provider(vec![
        ProviderReply::refused(401, "invalid credentials"),
        ProviderReply::answer(&["recovered"], (11, 2)),
    ]);
    let options = recorded(case);
    case.boot(Host::Workbench, "node-a", options.clone())
        .await?;
    let first = workbench::send(case, "node-a", "invalid credentials").await?;
    let outcome = workbench::follow(case, "node-a", &first).await?;
    let (kind, _) = support::settled(&outcome);
    ensure!(
        kind == "failed:provider_error",
        "the refused Run settled {kind}: {outcome}"
    );
    let calls = model_calls(case, "node-a", &first.run).await?;
    ensure!(
        calls.len() == 1
            && calls[0]["attempts"]
                .as_array()
                .is_some_and(|attempts| attempts.len() == 1),
        "the refused Run's model call is not one attempt: {calls:?}"
    );
    ensure!(
        calls[0]["attempts"][0].to_string().contains("auth"),
        "the refusal is not classified as authentication: {calls:?}"
    );
    ensure!(
        requests(case).len() == 1,
        "the authentication failure was retried"
    );
    let node = match case.leg {
        Leg::Live => "node-a",
        Leg::Resume => {
            case.kill("node-a", "the refused Run settled").await?;
            let next = successor(case);
            case.boot(Host::Workbench, next, options).await?;
            next
        }
    };
    let second = workbench::send(case, node, "fresh valid request").await?;
    ensure!(
        second.run != first.run,
        "the fresh request reused the failed Run"
    );
    let outcome = workbench::follow(case, node, &second).await?;
    let (kind, reply) = support::settled(&outcome);
    ensure!(
        kind == "completed" && reply.as_deref() == Some("recovered"),
        "the next Run settled {kind} {reply:?}: {outcome}"
    );
    let sent = requests(case);
    ensure!(
        sent.len() == 2 && sent[0]["body"] != sent[1]["body"],
        "the provider saw {} requests, not one per Run",
        sent.len()
    );
    // The workbench's own follower clears a settled turn's claim; none stays.
    let session = support::session(case);
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
    case.stop(node).await?;
    for (turn, end) in [(&first, "Failed"), (&second, "Answered")] {
        let facts = support::record_facts(case, &turn.run, "after both Runs").await?;
        ensure!(
            facts["end"]["kind"] == end,
            "the store's end of {} is {facts}, not {end}",
            turn.run
        );
    }
    Ok(())
}

/// The S26 variants.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    /// The provider refuses with a recorded 429 once, then answers.
    RateLimited,
    /// The stream resets its connection after its first delta.
    PartialReset,
    /// A product observer disconnects mid-answer and reconnects.
    ObserverReconnect,
}

case!(
    s26_streaming_retries_produce_one_logical_answer_recorded_429_retry,
    SqliteFile,
    Live,
    s26_rate_limited
);
case!(
    s26_streaming_retries_produce_one_logical_answer_recorded_429_retry_resume,
    SqliteFile,
    Resume,
    s26_rate_limited
);
case!(
    s26_streaming_retries_produce_one_logical_answer_partial_stream_reset,
    SqliteFile,
    Live,
    s26_partial_reset
);
case!(
    s26_streaming_retries_produce_one_logical_answer_partial_stream_reset_resume,
    SqliteFile,
    Resume,
    s26_partial_reset
);
case!(
    s26_streaming_retries_produce_one_logical_answer_observer_reconnect,
    SqliteFile,
    Live,
    s26_observer_reconnect
);
case!(
    s26_streaming_retries_produce_one_logical_answer_observer_reconnect_resume,
    SqliteFile,
    Resume,
    s26_observer_reconnect
);
case!(
    s26_streaming_retries_produce_one_logical_answer_live_replay,
    SqliteFile,
    Live,
    s26_rate_limited
);

async fn s26_rate_limited(case: &mut Case) -> Result<()> {
    s26(case, Stream::RateLimited).await
}

async fn s26_partial_reset(case: &mut Case) -> Result<()> {
    s26(case, Stream::PartialReset).await
}

async fn s26_observer_reconnect(case: &mut Case) -> Result<()> {
    s26(case, Stream::ObserverReconnect).await
}

/// The barrier a held recorded stream waits at.
const STREAM: &str = "s26-stream";

/// The assistant rows of turn `run` the workbench's transcript shows.
async fn transcript_replies(case: &mut Case, node: &str, run: &str) -> Result<Vec<Value>> {
    let session = support::session(case);
    let state = case
        .node(node)?
        .get(&format!("/api/state?session_id={session}"))
        .await?;
    let replies: Vec<Value> = state["transcript"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| {
            row["kind"] == "assistant_reply"
                && row["suppressed"].is_null()
                && row["provenance"]["turn_id"] == run
                && row["provenance"]["is_turn_reply"] == true
        })
        .cloned()
        .collect();
    case.evidence
        .outputs
        .push(json!({"node": node, "transcript replies": replies}));
    Ok(replies)
}

/// The workbench's production client streams one answer through the
/// recorded provider. A recorded 429 is retried with an actual second HTTP
/// request of the same logical call; a stream that resets after output
/// started ends the Run with the typed provider failure and is not
/// retried; a product observer that disconnects mid-answer and reconnects
/// from its cursor sees the one answer once. On the resume leg the node
/// dies while the stream is held mid-answer, and its successor's call
/// answers: one committed reply, whose usage is the provider's.
async fn s26(case: &mut Case, stream: Stream) -> Result<()> {
    let held = |text: &[&str]| ProviderReply {
        hold: Some((1, STREAM.to_owned())),
        ..ProviderReply::answer(text, (11, 2))
    };
    let answer = ProviderReply::answer(&["one ", "answer"], (11, 2));
    let resume = case.leg == Leg::Resume;
    let mut script = Vec::new();
    if stream == Stream::RateLimited {
        script.push(ProviderReply {
            headers: vec![("retry-after".to_owned(), "0".to_owned())],
            ..ProviderReply::refused(429, "rate limited")
        });
    }
    match (stream, resume) {
        (Stream::PartialReset, false) => script.push(ProviderReply {
            reset: Some(1),
            ..ProviderReply::answer(&["partial ", "answer"], (11, 2))
        }),
        (Stream::RateLimited, false) => script.push(answer.clone()),
        _ => script.extend([held(&["one ", "answer"]), answer.clone()]),
    }
    case.control.script_provider(script);
    let options = recorded(case);
    case.boot(Host::Workbench, "node-a", options.clone())
        .await?;
    let turn = workbench::send(case, "node-a", "answer once").await?;
    let mut observed = Vec::new();
    let mut node = "node-a";
    if resume || stream == Stream::ObserverReconnect {
        case.control.wait_held(STREAM, 1, case.deadline).await?;
        if stream == Stream::ObserverReconnect {
            // The observer reads what the stream holds so far, then goes.
            let run = turn.run.clone();
            let lines = workbench::product_events(case, node, |line| {
                line["event"]["type"] == "message"
                    && line["event"]["message"]["provenance"]["turn_id"] == json!(run)
            })
            .await?;
            observed.extend(lines);
        }
        if resume {
            case.kill("node-a", "the provider stream held mid-answer")
                .await?;
            case.control.release(STREAM);
            node = successor(case);
            case.boot(Host::Workbench, node, options).await?;
        } else {
            case.control.release(STREAM);
        }
    }
    let outcome = workbench::follow(case, node, &turn).await?;
    let (kind, reply) = support::settled(&outcome);
    let sent = requests(case);
    if stream == Stream::PartialReset && !resume {
        ensure!(
            kind == "failed:provider_error",
            "the reset stream's Run settled {kind}: {outcome}"
        );
        ensure!(
            sent.len() == 1,
            "the reset stream was retried: {} requests",
            sent.len()
        );
        let calls = model_calls(case, node, &turn.run).await?;
        let attempts = calls
            .first()
            .and_then(|call| call["attempts"].as_array())
            .cloned()
            .unwrap_or_default();
        ensure!(
            attempts.len() == 1
                && attempts[0]["protocol_position"] == "output_started"
                && attempts[0]["error"]["class"] == "transport"
                && attempts[0]["retry_decision"]["outcome"] == "declined",
            "the reset attempt is not one declined transport failure after output: {calls:?}"
        );
        case.stop(node).await?;
        let facts = support::record_facts(case, &turn.run, "after the Run").await?;
        ensure!(
            facts["end"]["kind"] == "Failed",
            "the store's end is {facts}"
        );
        return Ok(());
    }
    ensure!(
        kind == "completed" && reply.as_deref() == Some("one answer"),
        "the turn settled {kind} {reply:?}: {outcome}"
    );
    if case.live_replay {
        ensure!(
            outcome["gaps"].as_array().is_none_or(Vec::is_empty),
            "the shared live replay store reported gaps: {outcome}"
        );
    }
    // A follower's report is rebuilt from the store and carries usage on
    // the model call records it observed: the one completed attempt's is
    // the provider's.
    let completed: Vec<Value> = outcome["output"]["activities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|activity| activity["type"] == "model_call_recorded")
        .flat_map(|activity| {
            activity["record"]["attempts"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter(|attempt| attempt["outcome"] == "completed")
        .collect();
    ensure!(
        completed.len() == 1
            && completed[0]["usage"]["input_tokens"] == 11
            && completed[0]["usage"]["output_tokens"] == 2,
        "the Run's completed attempts are {completed:?}, not one with the provider's usage"
    );
    let expected = match (stream, resume) {
        (Stream::RateLimited, false) => 2,
        (Stream::RateLimited, true) => 3,
        (_, true) => 2,
        _ => 1,
    };
    ensure!(
        sent.len() == expected,
        "the provider saw {} requests, not {expected}",
        sent.len()
    );
    ensure!(
        sent.windows(2)
            .all(|pair| pair[0]["body"] == pair[1]["body"]),
        "a retried request is not the same logical call"
    );
    let calls = model_calls(case, node, &turn.run).await?;
    if stream == Stream::RateLimited && !resume {
        let attempts = calls
            .first()
            .and_then(|call| call["attempts"].as_array())
            .cloned()
            .unwrap_or_default();
        ensure!(
            calls.len() == 1
                && attempts.len() == 2
                && attempts[0]["error"]["http_status"] == 429
                && attempts[1]["ordinal"] == 2,
            "the retried call's attempts are {calls:?}"
        );
    }
    let replies = transcript_replies(case, node, &turn.run).await?;
    ensure!(
        replies.len() == 1 && replies[0]["content"]["text"] == "one answer",
        "the transcript shows {replies:?}, not the one reply"
    );
    // The observer that left reconnects after its cursor: with what it
    // read first, it holds every item once, the run's model call too.
    let cursor = observed
        .iter()
        .filter_map(|line| line["event"]["sequence"].as_u64())
        .max()
        .unwrap_or(0);
    let run = turn.run.clone();
    let late =
        workbench::product_events_after(case, node, cursor, |line| model_call_of(line, &run))
            .await?;
    observed.extend(late);
    let recorded = observed
        .iter()
        .filter(|line| model_call_of(line, &turn.run))
        .count();
    ensure!(
        recorded == 1,
        "the observers saw the run's model call {recorded} times: {observed:?}"
    );
    let sequences: Vec<_> = observed
        .iter()
        .filter_map(|line| line["event"]["sequence"].as_u64())
        .collect();
    ensure!(
        sequences.windows(2).all(|pair| pair[0] < pair[1]),
        "the reconnected observer saw an item twice: {sequences:?}"
    );
    case.stop(node).await?;
    let facts = support::record_facts(case, &turn.run, "after the Run").await?;
    ensure!(
        facts["end"]["kind"] == "Answered",
        "the store's end is {facts}"
    );
    Ok(())
}
