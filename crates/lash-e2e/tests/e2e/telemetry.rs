//! OpenTelemetry delivery from a consumer node: S34 (OTel delivery
//! describes logical calls and actual attempts).

use anyhow::{Context as _, Result, ensure};
use lash_e2e::{Case, Host, Leg, NodeOptions};
use serde_json::{Value, json};

use crate::support::{self, successor};

/// The S34 consumer: its retrying workload and the host's OTLP exporter
/// to the case's collector.
fn exporting(case: &Case) -> NodeOptions {
    NodeOptions {
        env: vec![
            ("E2E_CONSUMER_SCENARIO".to_owned(), "S34".to_owned()),
            (
                "E2E_CONSUMER_OTLP_ENDPOINT".to_owned(),
                case.control.traces_url(),
            ),
        ],
        ..NodeOptions::default()
    }
}

/// Wait until the workload's body of `phase` entered on `node`.
async fn entered(case: &Case, node: &str, phase: u32) -> Result<Value> {
    case.node(node)?
        .get(&format!("/control/s34/wait/{phase}"))
        .await
}

/// Release the workload's gate `phase` on `node`.
async fn release(case: &Case, node: &str, phase: u32) -> Result<()> {
    case.node(node)?
        .post(&format!("/control/s34/release/{phase}"), &json!({}))
        .await?;
    Ok(())
}

/// Flush `node`'s exporter, answering its delivery receipt.
async fn flush(case: &mut Case, node: &str) -> Result<Value> {
    let receipt = case
        .node(node)?
        .post("/control/telemetry/flush", &json!({}))
        .await?;
    case.evidence
        .effects
        .push(json!({"telemetry flush": receipt, "node": node}));
    Ok(receipt)
}

/// A span attribute's string value.
fn attribute<'a>(span: &'a Value, key: &str) -> Option<&'a str> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|attribute| attribute["key"] == key)?["value"]
        .as_object()?
        .values()
        .next()?
        .as_str()
}

/// The consumer's shutdown line: its exporter's final delivery receipt.
fn shutdown_receipt(case: &Case, node: &str, boot: usize) -> Result<Value> {
    let stdout = std::fs::read_to_string(case.dir.join(format!("{node}-{boot}.stdout")))?;
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix("CONSUMER_SHUTDOWN "))
        .context("the node wrote no shutdown receipt")?;
    let receipt: Value = serde_json::from_str(line)?;
    Ok(receipt["telemetry"].clone())
}

/// A span with its identity and its lash attributes, as the collector holds
/// it.
fn summary(span: &Value) -> Value {
    json!({
        "name": span["name"], "trace": span["traceId"], "span": span["spanId"],
        "parent": span["parentSpanId"],
        "turn": attribute(span, "lash.turn.id"), "session": attribute(span, "lash.session.id"),
        "ordinal": attribute(span, "lash.model.attempt.ordinal"),
    })
}

case!(
    s34_otel_delivery_describes_logical_calls_and_actual_attempts,
    SqliteFile,
    Live,
    s34
);
case!(
    s34_otel_delivery_describes_logical_calls_and_actual_attempts_resume,
    SqliteFile,
    Resume,
    s34
);

/// One turn calls a `Repeatable` tool whose first attempt fails retryably
/// after its own model call, and whose second succeeds; then the model
/// answers. The host exports spans over OTLP to the case's collector. Live,
/// the collector is down across the retry: the flush there reports the
/// spans it dropped, the Run still completes, and once it is back the
/// shutdown drains the rest, acknowledged. Resume, the node is killed while
/// the second attempt runs and another node finishes the turn. Either way
/// the logical send, its admission and the tool call's admission export
/// once, a model attempt span
/// exists only for a body that ran its model call, every span is delivered
/// at most once, and the spans of both nodes name the one session and turn.
async fn s34(case: &mut Case) -> Result<()> {
    let options = exporting(case);
    case.boot(Host::Consumer, "node-a", options.clone()).await?;
    let turn = support::submit(case, "node-a", "turn-1", "S34 one logical call").await?;
    entered(case, "node-a", 1).await?;
    release(case, "node-a", 1).await?;
    entered(case, "node-a", 2).await?;
    let mut receipts = Vec::new();
    let node = match case.leg {
        Leg::Live => {
            case.control.collector_down(true);
            let dropped = flush(case, "node-a").await?;
            ensure!(
                dropped["dropped_spans"]
                    .as_u64()
                    .is_some_and(|dropped| dropped > 0)
                    && !dropped["flush_error"].is_null(),
                "the outage's flush reported no drop: {dropped}"
            );
            case.control.collector_down(false);
            release(case, "node-a", 2).await?;
            "node-a"
        }
        Leg::Resume => {
            let flushed = flush(case, "node-a").await?;
            ensure!(
                flushed["dropped_spans"] == 0,
                "a flush to a live collector dropped spans: {flushed}"
            );
            receipts.push(flushed);
            case.kill("node-a", "the retry's second attempt running")
                .await?;
            let next = successor(case);
            case.boot(Host::Consumer, next, options).await?;
            entered(case, next, 2).await?;
            release(case, next, 2).await?;
            next
        }
    };
    entered(case, node, 3).await?;
    release(case, node, 3).await?;
    let outcome = support::follow(case, node, &turn.input).await?;
    let (kind, reply) = support::settled(&outcome);
    ensure!(
        kind == "completed" && reply.as_deref() == Some("S34 one logical answer"),
        "the turn settled {kind} {reply:?}: {outcome}"
    );
    case.stop(node).await?;
    let boot = if case.leg == Leg::Live { 1 } else { 2 };
    let last = shutdown_receipt(case, node, boot)?;
    case.evidence
        .effects
        .push(json!({"telemetry shutdown": last, "node": node}));
    ensure!(
        last["shutdown_error"].is_null() && last["flush_error"].is_null(),
        "the shutdown did not drain cleanly: {last}"
    );
    receipts.push(last);
    let spans = case.control.spans();
    let summaries: Vec<Value> = spans.iter().map(summary).collect();
    case.evidence
        .outputs
        .push(json!({"spans": summaries, "receipts": receipts}));
    let acknowledged: u64 = receipts
        .iter()
        .map(|receipt| receipt["acknowledged_spans"].as_u64().unwrap_or_default())
        .sum();
    ensure!(
        spans.len() as u64 == acknowledged,
        "the collector holds {} spans, the exporters acknowledged {acknowledged}",
        spans.len()
    );
    let ids: std::collections::BTreeSet<_> = summaries
        .iter()
        .map(|span| (span["trace"].to_string(), span["span"].to_string()))
        .collect();
    ensure!(
        ids.len() == spans.len(),
        "a span was delivered twice: {summaries:?}"
    );
    let named = |name: &str| summaries.iter().filter(|span| span["name"] == name).count();
    let chats = summaries
        .iter()
        .filter(|span| {
            span["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("chat "))
        })
        .count();
    match case.leg {
        Leg::Live => {
            // Seven spans in all: the send, its admission, the tool call's
            // admission, two model calls of the turn and one of each tool
            // body that ran.
            let attempted = receipts[0]["attempted_spans"].as_u64().unwrap_or_default();
            let dropped = receipts[0]["dropped_spans"].as_u64().unwrap_or_default();
            ensure!(
                attempted == 7 && acknowledged + dropped == 7,
                "the exporter attempted {attempted} spans, acknowledged {acknowledged} and dropped {dropped}"
            );
        }
        Leg::Resume => {
            ensure!(
                named("lash.send") == 1 && named("lash.turn.admitted") == 1,
                "the logical send and admission did not export once: {summaries:?}"
            );
            ensure!(
                named("lash.tool.admitted") == 1,
                "the one tool call's admission exported {} times across the handover: {summaries:?}",
                named("lash.tool.admitted")
            );
            ensure!(
                chats == 4,
                "{chats} model attempt spans, not one per model call that ran: {summaries:?}"
            );
            // The handover keeps the turn's trace: every model attempt of
            // either node hangs in the one admitted scope's trace.
            let admitted = summaries
                .iter()
                .find(|span| span["name"] == "lash.turn.admitted")
                .context("no admission span")?;
            ensure!(
                summaries
                    .iter()
                    .filter(|span| span["name"]
                        .as_str()
                        .is_some_and(|name| name.starts_with("chat ")))
                    .all(|span| span["trace"] == admitted["trace"]),
                "a model attempt left the turn's trace across the handover: {summaries:?}"
            );
        }
    }
    let session = support::session(case);
    ensure!(
        summaries
            .iter()
            .all(|span| span["session"].is_null() || span["session"] == json!(session))
            && summaries.iter().any(|span| span["turn"] == "turn-1"),
        "the spans do not name the one session and turn: {summaries:?}"
    );
    Ok(())
}
