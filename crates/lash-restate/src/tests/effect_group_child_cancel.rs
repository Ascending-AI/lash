//! D20 (FIG-3904): an effect-group child's cancel race is recorded.
//!
//! The dispatched child handler learns of its cancel from the group's durable
//! cancel fact: the child's cancel wait, which the index resolves when it
//! decides the child's cancel (a `close(Cancel)` or a retirement) and ends
//! `Settled` once the child's settlement is seated. These laws drive the
//! deployed `EffectGroupDispatch/child` handler through the endpoint protocol,
//! with the index's answers scripted and the deployment's ingress replaced by
//! a transport the law controls:
//!
//! - a wait child races its wait against a journaled arm over that fact, so a
//!   replay whose journal holds both completions takes the arm the live run
//!   took, and a child whose live run was cancelled settles `Cancelled` again;
//! - an atomic child's execution-side watch retries a transient fault on the
//!   shared ladder, so the fault never drops the running body.

use crate::EffectGroupDispatch as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::BytesMut;
use lash_core::{
    EffectAddress, ExecutionScope, GroupExecutors, GroupWakePolicy, LoserPolicy,
    RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
};
use lash_http_transport::{
    HttpRequest, HttpResponse, HttpResponseBody, HttpTransport, LlmTransportError,
};
use restate_sdk::prelude::Endpoint;
use std::time::Duration;

use super::endpoint_protocol::{
    RecordedCommand, encode_recorded_commands_replay, encode_sleep_completion,
    invoke_endpoint_body, invoke_endpoint_with_named_call_responses, restate_call_parameters,
    restate_error_message, restate_message_types, restate_recorded_commands,
};
use crate::effect_group::{
    EffectGroupChildRequest, EffectGroupRecordSettlementRequest, EffectGroupSettlementTerminal,
    EffectGroupShape, EffectGroupWaitResolution,
};

const DISPATCH: &str = "EffectGroupDispatch";
const GROUP: &str = "fig-3904-group";
/// `ProposeRunCompletionMessage`: a `ctx.run` body's recorded result.
const PROPOSE_RUN_COMPLETION: u16 = 0x0005;
/// `SleepCommandMessage`: a durable timer.
const SLEEP_COMMAND: u16 = 0x040C;

fn scope() -> ExecutionScope {
    ExecutionScope::runtime_operation("fig-3904")
}

/// A single-child group running `command` at position 0.
fn child_request(command: RuntimeEffectCommand) -> EffectGroupChildRequest {
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(scope(), format!("{GROUP}:child:0")).expect("valid child address"),
            RuntimeAttribution::none(),
            "effect",
        ),
        command,
    );
    EffectGroupChildRequest {
        group_key: GROUP.to_owned(),
        shape: EffectGroupShape {
            wake: GroupWakePolicy::All,
            loser_disposition: LoserPolicy::Cancel,
            replay_keys: vec![envelope.invocation.effect_replay_key().to_string()],
            wait_scope: scope(),
            opener: lash_core::AdmittedScope::runtime_operation("fig-3904"),
        },
        position: 0,
        envelope,
    }
}

/// What one answer of the deployment's ingress does.
#[derive(Clone, Copy, Debug)]
enum WatchReply {
    /// Answers the cancel wait's resolution `Cancel` at once.
    Cancel,
    /// Answers `Cancel` after a delay: a watch whose round trip is slower
    /// than a replayed journal.
    CancelAfter(Duration),
    /// A transient transport fault.
    Fault,
    /// Never answers: a healthy long-poll on a wait nobody resolved.
    Hang,
}

/// The deployment's ingress as the law scripts it: the `n`th request gets
/// the `n`th reply, and every later one the last.
#[derive(Debug)]
struct ScriptedIngress {
    replies: Vec<WatchReply>,
    requests: AtomicUsize,
    notify: tokio::sync::Notify,
}

impl ScriptedIngress {
    fn new(replies: Vec<WatchReply>) -> Arc<Self> {
        Arc::new(Self {
            replies,
            requests: AtomicUsize::new(0),
            notify: tokio::sync::Notify::new(),
        })
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    async fn until_requests(&self, count: usize) {
        loop {
            let notified = self.notify.notified();
            if self.requests() >= count {
                return;
            }
            notified.await;
        }
    }
}

fn cancel_resolution_body() -> HttpResponseBody {
    let resolution = lash_core::Resolution::Ok(
        serde_json::to_value(EffectGroupWaitResolution::Cancel).expect("encode the cancel wake"),
    );
    HttpResponseBody::buffered(crate::wire::reply_json(&resolution))
}

#[async_trait::async_trait]
impl HttpTransport for ScriptedIngress {
    async fn send(
        &self,
        _request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        let index = self.requests.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
        let reply = self
            .replies
            .get(index)
            .or(self.replies.last())
            .copied()
            .unwrap_or(WatchReply::Hang);
        let cancelled = || {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: cancel_resolution_body(),
            })
        };
        match reply {
            WatchReply::Cancel => cancelled(),
            WatchReply::CancelAfter(delay) => {
                tokio::time::sleep(delay).await;
                cancelled()
            }
            WatchReply::Fault => Err(LlmTransportError::new(
                "the cancel watch's connection was reset",
            )),
            WatchReply::Hang => std::future::pending().await,
        }
    }
}

/// The deployment's resolver: a timer child waits on the controller, an
/// atomic child runs once the law opens `gate`, counting its runs.
struct ChildExecutors {
    gate: Arc<tokio::sync::Semaphore>,
    runs: Arc<AtomicUsize>,
}

impl ChildExecutors {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            gate: Arc::new(tokio::sync::Semaphore::new(0)),
            runs: Arc::new(AtomicUsize::new(0)),
        })
    }
}

impl GroupExecutors for ChildExecutors {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        match &envelope.command {
            RuntimeEffectCommand::Sleep { .. } => Some(
                RuntimeEffectLocalExecutor::sleep(tokio_util::sync::CancellationToken::new())
                    .with_turn_cancel_observation(false),
            ),
            RuntimeEffectCommand::LanguageRuntimeValue { .. } => {
                let (gate, runs) = (Arc::clone(&self.gate), Arc::clone(&self.runs));
                Some(RuntimeEffectLocalExecutor::testing(move |_| async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    let _permit = gate.acquire().await.expect("the gate never closes");
                    Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                        value: serde_json::json!({ "atomic": "done" }),
                    })
                }))
            }
            _ => None,
        }
    }
}

async fn endpoint(ingress: Arc<ScriptedIngress>, executors: Arc<ChildExecutors>) -> Endpoint {
    let host = crate::RestateEffectHost::new_for_test("http://ingress.invalid");
    host.register_group_executors(executors as Arc<dyn GroupExecutors>)
        .expect("register the law's resolver");
    let connection = crate::RestateConnection::with_transport(
        "http://ingress.invalid",
        ingress as Arc<dyn HttpTransport>,
    );
    Endpoint::builder()
        .bind(
            crate::EffectGroupDispatchImpl::new(
                &host,
                crate::RestateIngressClient::new(connection),
                restate_sdk::context::RunRetryPolicy::new(),
                super::memory_session_store_factory().await,
                crate::services::DEFAULT_NAMESPACE.stable(crate::LashService::EffectGroupDispatch),
                lash_core::engine::BuildGeneration::for_test("fig-3904"),
            )
            .serve(),
        )
        .build()
}

/// The index's and the durable wait's answers a child's live run reads.
fn index_answer(handler: &str) -> Option<serde_json::Value> {
    Some(match handler {
        "admit_child" => serde_json::json!({ "type": "admitted" }),
        "record_group_child" => serde_json::json!(true),
        // The child's cancel wait, resolved `Cancel` by the deciding index.
        "await_resolution" => serde_json::to_value(lash_core::Resolution::Ok(
            serde_json::to_value(EffectGroupWaitResolution::Cancel).expect("encode the wake"),
        ))
        .expect("encode the resolution"),
        "commit_child" => serde_json::json!({
            "type": "committed",
            "commit_seq": 0,
            "blocking_positions": [],
        }),
        "put" => serde_json::json!({ "type": "written" }),
        "record_settlement" => serde_json::json!({ "type": "recorded", "rank": 1 }),
        _ => return None,
    })
}

fn named_answers(handlers: &[&str]) -> Vec<(String, serde_json::Value)> {
    handlers
        .iter()
        .map(|handler| {
            (
                (*handler).to_owned(),
                index_answer(handler).expect("an answer the law scripts"),
            )
        })
        .collect()
}

/// The terminal `record_settlement` carried, when the child recorded one.
fn settled_terminal(output: &[u8]) -> Option<EffectGroupSettlementTerminal> {
    restate_call_parameters(output)
        .expect("decode the child's calls")
        .into_iter()
        .find(|(handler, _)| handler == "record_settlement")
        .map(|(_, parameter)| {
            serde_json::from_value::<EffectGroupRecordSettlementRequest>(parameter)
                .expect("decode the settlement")
                .terminal
        })
}

/// A wait child whose live run lost its wait to its cancel is replayed with
/// the timer fired too: the journal holds the cancel's completion first and
/// the timer's after it. The replay takes the arm the journal completed
/// first, issues no command the journal does not hold and settles
/// `Cancelled` again.
///
/// Red before D20: the child raced its wait against a live ingress watch, so
/// the replay, whose timer was ready before the watch's round trip came back,
/// took the wait, settled `Completed` and proposed a payload `put` where the
/// journal held the recorded `Cancelled` settlement.
#[tokio::test]
async fn a_replayed_wait_child_whose_live_run_was_cancelled_settles_cancelled() {
    // The first watch answers at once; a replay's watch comes back only after
    // its round trip, well after the journal's timer completion.
    let ingress = ScriptedIngress::new(vec![
        WatchReply::Cancel,
        WatchReply::CancelAfter(Duration::from_millis(500)),
    ]);
    let executors = ChildExecutors::new();
    let endpoint = endpoint(ingress, Arc::clone(&executors)).await;
    let request = child_request(RuntimeEffectCommand::Sleep {
        spec: lash_core::SleepSpec::For {
            duration_ms: 3_600_000,
        },
    });

    // The live run: the index decides the child's cancel while its hour-long
    // timer waits.
    let live = invoke_endpoint_with_named_call_responses(
        &endpoint,
        DISPATCH,
        "child",
        GROUP,
        &request,
        named_answers(&[
            "admit_child",
            "record_group_child",
            "await_resolution",
            "commit_child",
            "record_settlement",
        ]),
    )
    .await
    .expect("the live run settles");
    assert_eq!(
        restate_error_message(&live),
        None,
        "the live run ends without an error"
    );
    let terminal = settled_terminal(&live);
    assert!(
        matches!(terminal, Some(EffectGroupSettlementTerminal::Cancelled)),
        "the live run lost its wait to its cancel and settles cancelled: {terminal:?}"
    );

    // The replay: every command the live run journaled with its recorded
    // answer, and the timer fired afterwards.
    let recorded = restate_recorded_commands(&live).expect("decode the live journal");
    let timer = recorded
        .iter()
        .find(|command| command.message_type == SLEEP_COMMAND)
        .and_then(|command| command.completion_id);
    let mut body = BytesMut::from(
        &encode_recorded_commands_replay(GROUP, &request, &[&live], |command: &RecordedCommand| {
            command
                .call
                .as_ref()
                .and_then(|(_, handler)| index_answer(handler))
        })
        .expect("encode the replay")[..],
    );
    if let Some(timer) = timer {
        body.extend_from_slice(&encode_sleep_completion(timer));
    }
    let replay = invoke_endpoint_body(&endpoint, DISPATCH, "child", body.freeze())
        .await
        .expect("the replay settles");
    assert_eq!(
        restate_error_message(&replay),
        None,
        "the replay issues exactly the commands its journal holds"
    );
    assert!(
        restate_recorded_commands(&replay)
            .expect("decode the replay's frames")
            .is_empty(),
        "the replay journals nothing new: {:?}",
        restate_message_types(&replay)
    );
}

/// An atomic child's execution-side watch on its cancel fact faults once,
/// transiently, while the child's recorded body runs. The watch retries on
/// the shared ladder and the body runs to its end: its outcome is recorded
/// and the child settles it.
///
/// Red before D20: the watch fault ended the attempt at once and dropped the
/// running body, so a connection blip re-ran the child's effect.
#[tokio::test]
async fn a_transient_cancel_watch_fault_does_not_drop_an_atomic_childs_body() {
    let ingress = ScriptedIngress::new(vec![WatchReply::Fault, WatchReply::Hang]);
    let executors = ChildExecutors::new();
    let endpoint = endpoint(Arc::clone(&ingress), Arc::clone(&executors)).await;
    let request = child_request(RuntimeEffectCommand::LanguageRuntimeValue {
        operation: "atomic".to_owned(),
    });

    let run = tokio::spawn({
        let endpoint = endpoint.clone();
        async move {
            invoke_endpoint_with_named_call_responses(
                &endpoint,
                DISPATCH,
                "child",
                GROUP,
                &request,
                named_answers(&[
                    "admit_child",
                    "record_group_child",
                    "commit_child",
                    "put",
                    "record_settlement",
                ]),
            )
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(10), ingress.until_requests(1))
        .await
        .expect("the child watches its cancel fact while its body runs");
    // Give an attempt that fails on the fault time to end before the body
    // finishes.
    tokio::time::sleep(Duration::from_millis(200)).await;
    executors.gate.add_permits(1);

    let output = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the child ends")
        .expect("join the child")
        .expect("drive the child invocation");
    assert_eq!(
        restate_error_message(&output),
        None,
        "a transient watch fault does not end the attempt"
    );
    assert!(
        restate_message_types(&output)
            .expect("decode the child's frames")
            .contains(&PROPOSE_RUN_COMPLETION),
        "the body ran to its end and its outcome was recorded"
    );
    let terminal = settled_terminal(&output);
    assert!(
        matches!(terminal, Some(EffectGroupSettlementTerminal::StoredPayload)),
        "the child settles the body's outcome: {terminal:?}"
    );
    assert_eq!(
        executors.runs.load(Ordering::SeqCst),
        1,
        "the body ran once"
    );
}
