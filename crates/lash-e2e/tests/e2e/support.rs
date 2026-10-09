//! What the cases share: a turn on a consumer node, and the store facts the
//! controller reads back itself.

use anyhow::{Context as _, Result, ensure};
use lash_e2e::{Case, Leg, Store};
use serde_json::{Value, json};

/// The session every case runs its turns in.
pub fn session(case: &Case) -> String {
    format!("{}-session", case.name.replace('_', "-"))
}

/// One accepted turn: the input id its follower attaches by, and the run it
/// executes as (a send under a turn id runs as that turn).
pub struct Turn {
    pub input: String,
    pub run: String,
}

/// Submit `text` as turn `id` through node `node`.
pub async fn submit(case: &Case, node: &str, id: &str, text: &str) -> Result<Turn> {
    let session = session(case);
    let host = case.node(node)?;
    let receipt = host
        .post(
            &format!("/sessions/{session}/inputs"),
            &json!({"id": id, "text": format!("{} {text}", case.name)}),
        )
        .await?;
    case.record_barrier(
        json!({"barrier": "accepted", "node": node, "input": id, "receipt": receipt}),
    );
    let input = receipt["input_id"]
        .as_str()
        .context("the send answered no input id")?
        .to_owned();
    Ok(Turn {
        input,
        run: id.to_owned(),
    })
}

/// Follow turn `id` on node `node` to its settled outcome.
pub async fn follow(case: &mut Case, node: &str, id: &str) -> Result<Value> {
    let session = session(case);
    let outcome = case
        .node(node)?
        .get(&format!("/sessions/{session}/inputs/{id}"))
        .await?;
    case.evidence
        .outputs
        .push(json!({"node": node, "input": id, "outcome": outcome}));
    Ok(outcome)
}

/// Cancel turn `id` through node `node`, answering the receipt.
pub async fn cancel(case: &mut Case, node: &str, id: &str) -> Result<Value> {
    let session = session(case);
    let receipt = case
        .node(node)?
        .post(
            &format!("/sessions/{session}/inputs/{id}/cancel"),
            &json!({}),
        )
        .await?;
    case.evidence
        .faults
        .push(json!({"fault": "cancel", "node": node, "input": id, "receipt": receipt}));
    Ok(receipt)
}

/// The settled outcome's kind and assistant reply. A settled turn is
/// `completed`, `cancelled` or `failed:<stop>`; any other answer is its
/// own type (`withdrawn`, `refused`, `not_accepted`, ...).
pub fn settled(outcome: &Value) -> (String, Option<String>) {
    use lash::{TurnFinish, TurnOutcome, TurnStop};

    let kind = outcome["type"].as_str().unwrap_or("unknown");
    if kind != "settled" {
        return (kind.to_owned(), None);
    }
    // Both host attach routes serialize the native report (ADR 0136).
    // Decode its outcome with the same type instead of assuming DTO tags.
    let turn =
        match serde_json::from_value::<TurnOutcome>(outcome["output"]["result"]["outcome"].clone())
        {
            Ok(turn) => turn,
            Err(error) => return (format!("unknown:{error}"), None),
        };
    match turn {
        TurnOutcome::Finished(finish) => {
            let reply = match finish {
                TurnFinish::AssistantMessage { text } => Some(text),
                TurnFinish::FinalValue { value } => value.as_str().map(ToOwned::to_owned),
                TurnFinish::ToolValue { .. } => None,
            };
            ("completed".to_owned(), reply)
        }
        TurnOutcome::AgentFrameSwitch { .. } => ("completed".to_owned(), None),
        TurnOutcome::Stopped(TurnStop::Cancelled { .. }) => ("cancelled".to_owned(), None),
        TurnOutcome::Stopped(stop) => {
            let kind = match stop {
                TurnStop::Incomplete => "incomplete",
                TurnStop::InvalidInput => "invalid_input",
                TurnStop::MaxTurns => "max_turns",
                TurnStop::ToolFailure => "tool_failure",
                TurnStop::ToolPanicked { .. } => "tool_panicked",
                TurnStop::ProviderError => "provider_error",
                TurnStop::ContextOverflow => "context_overflow",
                TurnStop::PluginAbort => "plugin_abort",
                TurnStop::RuntimeError => "runtime_error",
                TurnStop::AgentFrameSwitchLimit => "agent_frame_switch_limit",
                TurnStop::SubmittedError { .. } => "submitted_error",
                TurnStop::ToolError { .. } => "tool_error",
                TurnStop::Cancelled { .. } => unreachable!("cancelled turns are handled above"),
            };
            (format!("failed:{kind}"), None)
        }
    }
}

/// FIG-5482: the hosts serialize native turn outcomes inside their settled
/// envelope; reading that encoding preserves completion, reply and stop kind.
#[test]
fn settled_reads_the_host_native_turn_outcome() {
    use lash::{TurnCancellationEvidence, TurnFinish, TurnOutcome, TurnStop};

    let cases = [
        (
            TurnOutcome::Finished(TurnFinish::AssistantMessage {
                text: "standard reply".into(),
            }),
            "completed",
            Some("standard reply"),
        ),
        (
            TurnOutcome::Finished(TurnFinish::FinalValue {
                value: json!("cell reply"),
            }),
            "completed",
            Some("cell reply"),
        ),
        (
            TurnOutcome::Finished(TurnFinish::FinalValue {
                value: json!({"answer": 42}),
            }),
            "completed",
            None,
        ),
        (
            TurnOutcome::Finished(TurnFinish::ToolValue {
                tool_name: "echo".into(),
                value: json!("tool reply"),
            }),
            "completed",
            None,
        ),
        (
            TurnOutcome::AgentFrameSwitch {
                frame_key: lash::FrameKey::from_caller_material("e2e-outcome-law").unwrap(),
                task: "next frame".into(),
                initial_nodes: Vec::new(),
            },
            "completed",
            None,
        ),
        (
            TurnOutcome::Stopped(TurnStop::Cancelled {
                evidence: TurnCancellationEvidence::internal("e2e-outcome-law"),
            }),
            "cancelled",
            None,
        ),
        (
            TurnOutcome::Stopped(TurnStop::ProviderError),
            "failed:provider_error",
            None,
        ),
        (
            TurnOutcome::Stopped(TurnStop::ToolError {
                tool_name: "echo".into(),
                value: json!("refused"),
            }),
            "failed:tool_error",
            None,
        ),
    ];
    for (turn_outcome, kind, reply) in cases {
        // This is the same serialization used by both host attach routes.
        let envelope = json!({"type": "settled", "output": {"result": {"outcome": turn_outcome}}});
        assert_eq!(
            settled(&envelope),
            (kind.to_owned(), reply.map(ToOwned::to_owned)),
            "{envelope}"
        );
    }
    assert_eq!(
        settled(&json!({"type": "withdrawn"})),
        ("withdrawn".into(), None)
    );
}

/// What the store holds for turn `run`: its unfinished row's phase, its run
/// records and its end, read by the controller over the store itself.
pub async fn turn_facts(case: &Case, run: &str) -> Result<Value> {
    use lash::durable::domain::OwnerKey;
    let stores = case.open_store().await?;
    let durable = stores.durable_store();
    let session = lash::SessionId::parse(session(case))?;
    let turn = lash::TurnId::parse(run)?;
    let records = durable
        .run_records(&OwnerKey::Turn(session.clone(), turn.clone()))
        .await?
        .into_iter()
        .map(|row| {
            json!({
                "run": row.run.0,
                "ordinal": row.ordinal.0,
                "kind": row.kind.as_str(),
                "call": row.call.map(|call| call.to_string()),
                "epoch": row.written_epoch.0,
                "record": serde_json::from_str::<Value>(&row.record_json).unwrap_or(Value::String(row.record_json)),
            })
        })
        .collect::<Vec<_>>();
    let end = durable.turn_end(&session, &turn).await?;
    let unfinished = durable.turn(&session).await?;
    Ok(json!({
        "run": run,
        "records": records,
        "unfinished": unfinished.map(|row| json!({"run": row.run.to_string(), "model_calls": row.model_calls, "epoch": row.written_epoch.0})),
        "end": end.map(|end| json!({"kind": format!("{:?}", end.kind()), "head_revision": end.head_revision})),
    }))
}

/// Read the turn's store facts while no node serves the store (or, on
/// PostgreSQL, while they do), and keep them as evidence.
pub async fn record_facts(case: &mut Case, run: &str, at: &str) -> Result<Value> {
    let facts = turn_facts(case, run)
        .await
        .with_context(|| format!("read the store {at}"))?;
    case.evidence.stores.push(json!({"at": at, "facts": facts}));
    Ok(facts)
}

/// The resume leg's second node name, or the first node's for a live leg
/// that restarts in place.
pub fn successor(case: &Case) -> &'static str {
    match case.leg {
        Leg::Live => "node-a",
        Leg::Resume => "node-b",
    }
}

/// Whether the controller may read the store while a node serves it.
pub fn store_readable_live(case: &Case) -> bool {
    case.store == Store::Postgresql
}

/// The body entries of `tool`, as (call id, attempt) pairs in order.
pub fn entries(case: &Case, tool: &str) -> Result<Vec<(String, u64)>> {
    Ok(case
        .bodies()?
        .into_iter()
        .filter(|line| line["tool"] == tool)
        .map(|line| {
            (
                line["call_id"].as_str().unwrap_or_default().to_owned(),
                line["attempt"].as_u64().unwrap_or_default(),
            )
        })
        .collect())
}

/// Count the ledger lines of node `node` applied under `label`.
pub fn applied(case: &Case, node: &str, label: &str) -> Result<usize> {
    let ledger = case.dir.join(format!("commits-{node}.jsonl"));
    Ok(lash_e2e::read_jsonl(&ledger)?
        .into_iter()
        .filter(|line| line["label"] == label && line["applied"] == true)
        .count())
}

/// Ensure the case's final model call presented exactly `expected` tool
/// results, by call id.
pub fn presented(case: &Case, expected: &Value) -> Result<()> {
    let calls = case.model_calls()?;
    let last = calls.last().context("no model call was answered")?;
    ensure!(
        last["results"] == *expected,
        "the final model call saw {} instead of {expected}",
        last["results"]
    );
    Ok(())
}

/// Each call's committed outcome and the epoch that wrote it, from the store
/// facts of [`turn_facts`]. A call with two outcomes is refused by the store's
/// uniqueness, so a second one here is a failed oracle.
pub fn outcome_epochs(facts: &Value) -> std::collections::BTreeMap<String, i64> {
    let mut epochs = std::collections::BTreeMap::new();
    for record in facts["records"].as_array().into_iter().flatten() {
        if record["kind"] == "x_outcome"
            && let Some(call) = record["call"].as_str()
        {
            epochs.insert(
                call.to_owned(),
                record["epoch"].as_i64().unwrap_or_default(),
            );
        }
    }
    epochs
}

/// A `Repeatable` execution policy of `attempts` attempts with a fixed
/// backoff of `delay_ms`.
pub fn repeatable(attempts: u32, delay_ms: u64) -> Value {
    json!({"type": "repeatable", "retry": {"max_attempts": attempts, "backoff": {"base_delay_ms": delay_ms, "max_delay_ms": delay_ms}}})
}

/// The run records of kind `kind` in store facts.
pub fn records<'a>(facts: &'a Value, kind: &str) -> Vec<&'a Value> {
    facts["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|record| record["kind"] == kind)
        .collect()
}

/// The wait a completion key names, as the store holds it: its scope and
/// its lifecycle.
pub async fn wait_facts(case: &Case, key: &str) -> Result<Value> {
    let stores = case.open_store().await?;
    let id = lash::durable::domain::WaitId::parse_hex(key).context("not a wait key")?;
    let row = stores.durable_store().wait(&id).await?;
    Ok(row.map_or(
        Value::Null,
        |row| json!({"scope": row.scope.stored(), "lifecycle": format!("{:?}", row.lifecycle)}),
    ))
}

/// The namespace rows the store holds for unfinished turn `run`: what its
/// committed phases changed (FIG-5301).
pub async fn namespaces(case: &Case, run: &str) -> Result<Vec<Value>> {
    let stores = case.open_store().await?;
    let rows = stores
        .durable_store()
        .turn_namespaces(
            &lash::SessionId::parse(session(case))?,
            &lash::TurnId::parse(run)?,
        )
        .await?;
    rows.into_iter()
        .map(|row| Ok(json!({"plugin": row.plugin, "entry": serde_json::to_value(&row.entry)?})))
        .collect()
}

/// Whether plugin `plugin`'s namespace in `rows` has applied a publication:
/// a reduced resolution sets the frontier and records its receipt; the
/// namespace a run seeds at admission has neither.
pub fn published(rows: &[Value], plugin: &str) -> Result<bool> {
    let row = rows
        .iter()
        .find(|row| row["plugin"] == plugin)
        .with_context(|| format!("no {plugin} namespace in {rows:?}"))?;
    let frontier = &row["entry"]["publication"];
    Ok(!frontier["applied"].is_null()
        || frontier["receipts"]
            .as_object()
            .is_some_and(|receipts| !receipts.is_empty()))
}

/// The waits the store holds pending for the case's session actor.
pub async fn pending_waits(case: &Case) -> Result<Vec<Value>> {
    let stores = case.open_store().await?;
    let actor = lash::durable::ActorKey::session(&session(case))?;
    Ok(stores
        .durable_store()
        .pending_waits(&actor)
        .await?
        .into_iter()
        .map(
            |row| json!({"scope": row.scope.stored(), "lifecycle": format!("{:?}", row.lifecycle)}),
        )
        .collect())
}
