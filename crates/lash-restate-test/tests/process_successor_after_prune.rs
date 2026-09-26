//! A same-start-key process successor after prune, on Restate (FIG-3611).
//!
//! ADR 0106: a process's id is minted by the registrar and never reused; a
//! start key is idempotency only. Inside retention a process registered
//! under a key runs to terminal and is pruned; the next start under the same
//! key mints a new process id. This law watches the Restate tier that start
//! lands on: the successor submits its own `LashProcessWorkflow` key — the
//! minted id — rather than coalescing onto the retired workflow's key, its
//! body executes exactly once, it reaches its own terminal, and its engine
//! sees none of the pruned lifetime's session stores.
//!
//! Red before the registration cutover, where the process's name was its
//! identity: the successor re-derived the retired workflow key, and Restate
//! either refused the submission or coalesced it onto the completed
//! invocation, so the successor's body never ran.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions; a failed unwrap is the test failure"
)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_restate_test::{HandlerAttempt, RestateTestBackend, ServerConfig};

/// lash-restate's process workflow service name on the endpoint.
const PROCESS_WORKFLOW: &str = "LashProcessWorkflow";
/// The host that runs `run_in_handler` jobs.
const HANDLER_HOST: &str = "LashTestHandlerHost";
const START_KEY: &str = "fig-3611-successor-after-prune";
const ENGINE_KIND: &str = "fig-3611-successor-recorder";
const SESSION: &str = "fig-3611-successor-after-prune";

/// One engine kind for both lifetimes. `run` records the minted process id
/// it executed as and probes the session-store factory the worker hands it:
/// whether the pruned lifetime's session ids carry state it could read, and
/// whether its own derived ids are tombstoned. A successor that coalesced
/// onto the retired workflow never runs its body at all, so the record is
/// the coalescing detector.
struct RecordingEngine {
    runs: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl lash_core::ProcessEngine for RecordingEngine {
    fn kind(&self) -> &'static str {
        ENGINE_KIND
    }

    async fn run(
        &self,
        context: lash_core::ProcessEngineRunContext<'_>,
        payload: serde_json::Value,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
        let process_id = context.process_id().clone();
        let factory = context.session_store_factory();
        let mut predecessor_sessions_visible = Vec::new();
        let mut predecessor_bound_not_deleted = Vec::new();
        let mut own_sessions_bound = Vec::new();
        let mut own_sessions_tombstoned = Vec::new();
        if let Some(factory) = factory.as_ref() {
            for session_id in payload
                .get("predecessor_sessions")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
            {
                let session_id = lash_core::SessionId::from(session_id.to_string());
                if matches!(factory.read_session(&session_id).await, Ok(Some(_))) {
                    predecessor_sessions_visible.push(session_id.to_string());
                }
            }
            // `predecessor_bound` names only the ids the first lifetime
            // actually bound; every one of them must stay tombstoned.
            for session_id in payload
                .get("predecessor_bound")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
            {
                let session_id = lash_core::SessionId::from(session_id.to_string());
                if matches!(factory.session_was_deleted(&session_id).await, Ok(false)) {
                    predecessor_bound_not_deleted.push(session_id.to_string());
                }
            }
            // The durable catalog is the bound-or-tombstoned truth:
            // `read_session` answers `None` for a bound id whose store has
            // not committed yet, and `session_was_deleted` answers `false`
            // for an id that was never bound.
            let catalog = factory
                .list_sessions(&lash_core::SessionListFilter::default())
                .await
                .unwrap_or_default();
            for session_id in lash_core::facade_support::process_runtime_session_ids(&process_id) {
                match catalog
                    .iter()
                    .find(|summary| summary.session_id == session_id)
                {
                    Some(summary) if summary.deleted => {
                        own_sessions_tombstoned.push(session_id.to_string())
                    }
                    Some(_) => own_sessions_bound.push(session_id.to_string()),
                    None => {}
                }
            }
        }
        self.runs
            .lock()
            .unwrap()
            .push(process_id.as_str().to_string());
        Ok(lash_core::ProcessRunOutcome::Terminal {
            output: Box::new(lash_core::ProcessAwaitOutput::from_tool_output(
                lash_core::ToolCallOutput::success(serde_json::json!({
                    "process_id": process_id.as_str(),
                    "lifetime": payload
                        .get("lifetime")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown"),
                    "session_store_factory": factory.is_some(),
                    "predecessor_sessions_visible": predecessor_sessions_visible,
                    "predecessor_bound_not_deleted": predecessor_bound_not_deleted,
                    "own_sessions_bound": own_sessions_bound,
                    "own_sessions_tombstoned": own_sessions_tombstoned,
                })),
            )),
            prelude: Vec::new(),
        })
    }
}

/// Contributes the recording engine to the core's process-engine registry,
/// the way a host plugin does.
struct EnginePluginFactory {
    engine: Arc<RecordingEngine>,
}

impl lash::plugins::PluginFactory for EnginePluginFactory {
    fn id(&self) -> &'static str {
        ENGINE_KIND
    }

    fn process_engine_contributions(
        &self,
        _context: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError> {
        Ok(vec![lash_core::ProcessEngineRegistration::accepting(
            Arc::clone(&self.engine) as Arc<dyn lash_core::ProcessEngine>,
        )])
    }

    fn build(
        &self,
        _context: &lash::plugins::PluginSessionContext,
    ) -> Result<Arc<dyn lash::plugins::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(EngineSessionPlugin))
    }
}

struct EngineSessionPlugin;

impl lash::plugins::SessionPlugin for EngineSessionPlugin {
    fn id(&self) -> &'static str {
        ENGINE_KIND
    }

    fn register(
        &self,
        _registrar: &mut lash::plugins::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        Ok(())
    }
}

fn process_env_spec() -> lash_core::ProcessExecutionEnvSpec {
    lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::default(),
        lash_core::SessionPolicy {
            model: lash_core::ModelSpec::builder("mock-model")
                .context_window_tokens(200_000)
                .build()
                .expect("model spec"),
            ..lash_core::SessionPolicy::new(lash::TurnBudget::Unbounded)
        },
    )
}

fn start_request(start_key: &str, payload: serde_json::Value) -> lash_core::ProcessStartRequest {
    lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::Engine {
            kind: ENGINE_KIND.to_string(),
            payload,
        },
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_start_key(Some(lash_core::StartKey::for_host(
        lash_core::StartKeyOwner::HOST,
        start_key,
    )))
    .with_env_spec(process_env_spec())
}

/// Starts one process under `start_key` inside a handler — where a
/// deployment's starts run — and returns the minted process id.
async fn start_process(
    backend: &RestateTestBackend,
    core: &lash::LashCore,
    session: &lash::LashSession,
    turn_id: &str,
    start_key: &str,
    payload: serde_json::Value,
) -> lash_core::ProcessId {
    let admitted = lash_core::AdmittedScope::new(session.turn_scope(lash::TurnId::from(turn_id)));
    let request = start_request(start_key, payload);
    let started = Arc::new(Mutex::new(None));
    let attempt: HandlerAttempt = {
        let core = core.clone();
        let started = Arc::clone(&started);
        Arc::new(move |scoped| {
            let request = request.clone();
            let core = core.clone();
            let started = Arc::clone(&started);
            Box::pin(async move {
                let record = core
                    .processes()
                    .start(request, scoped)
                    .await
                    .expect("the start commits");
                *started.lock().unwrap() = Some(record.id);
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(20),
        backend.run_in_handler(admitted, attempt),
    )
    .await
    .expect("the start's handler finishes")
    .expect("the start's handler completes");
    started
        .lock()
        .unwrap()
        .clone()
        .expect("the start recorded its minted process id")
}

/// Awaits the process's terminal through the same `await_terminal` workflow
/// a host's await uses. A timeout dumps the server's invocation table: a
/// workflow that never ran or that failed shows there.
async fn await_terminal(
    backend: &RestateTestBackend,
    core: &lash::LashCore,
    process_id: &lash_core::ProcessId,
) -> lash_core::ProcessAwaitOutput {
    let waited = tokio::time::timeout(
        Duration::from_secs(20),
        core.processes().await_output(process_id),
    )
    .await;
    match waited {
        Ok(output) => output.expect("the terminal resolves"),
        Err(_) => {
            backend.server().settle().await;
            for view in backend.server().invocations() {
                eprintln!(
                    "invocation {} target={} status={} attempts={} last_failure={:?} outcome={:?}",
                    view.id,
                    view.target,
                    view.status,
                    view.attempts,
                    view.last_failure,
                    backend.server().outcome(&view.id).map(|outcome| outcome
                        .map(|bytes| { String::from_utf8_lossy(&bytes).into_owned() }))
                );
            }
            panic!("the process {process_id} reached no terminal in 20s")
        }
    }
}

/// The success payload a `RecordingEngine` run returned.
fn settled_payload(output: &lash_core::ProcessAwaitOutput) -> serde_json::Value {
    let lash_core::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process settled to a non-output terminal: {output:?}")
    };
    let lash_core::ToolCallOutcome::Success(value) = &output.outcome else {
        panic!("the process output is not a success: {output:?}")
    };
    value.to_json_value()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_same_start_key_successor_after_prune_runs_its_own_workflow() {
    let backend = lash_restate_test::backend(0x3611, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let provider = lash_core::testing::TestProvider::builder()
        .kind("process-successor")
        .complete(|_request| async move {
            Ok::<_, lash_core::llm::transport::LlmTransportError>(Default::default())
        })
        .build()
        .into_handle();
    let engine = Arc::new(RecordingEngine {
        runs: Mutex::new(Vec::new()),
    });
    let core =
        lash::LashCore::standard_builder(backend.lash_backend(), lash::TurnBudget::Unbounded)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .provider(provider)
            .model(
                lash_core::ModelSpec::builder("mock-model")
                    .context_window_tokens(200_000)
                    .build()
                    .expect("model spec"),
            )
            .plugin(Arc::new(EnginePluginFactory {
                engine: Arc::clone(&engine),
            }))
            .build(lash_core::LeaseOwnerIdentity::opaque(
                "lash-restate-test",
                "process-successor-after-prune",
            ))
            .expect("build the lash core");
    backend.install_process_worker(
        lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .expect("the core's process worker configuration"),
        )
        .expect("build the process worker"),
    );
    let session = core
        .session(SESSION)
        .open()
        .await
        .expect("open the session");

    // First lifetime: register under the key and run to terminal.
    let first_id = start_process(
        &backend,
        &core,
        &session,
        "turn-start-1",
        START_KEY,
        serde_json::json!({"lifetime": "first"}),
    )
    .await;
    let first_terminal = await_terminal(&backend, &core, &first_id).await;
    let first_payload = settled_payload(&first_terminal);
    assert_eq!(
        first_payload["lifetime"],
        serde_json::json!("first"),
        "the first lifetime reached its own terminal"
    );
    assert_eq!(
        first_payload["own_sessions_tombstoned"],
        serde_json::json!([]),
        "the first lifetime ran on live session ids"
    );
    assert!(
        first_payload["own_sessions_bound"]
            .as_array()
            .is_some_and(|bound| !bound.is_empty()),
        "the first lifetime bound at least its own process-env session: {first_payload}"
    );

    // Retire it: the row leaves retention, its scope fence lands, and its
    // completed `LashProcessWorkflow` invocation stays on the server.
    let first_sessions: Vec<String> =
        lash_core::facade_support::process_runtime_session_ids(&first_id)
            .into_iter()
            .map(|session_id| session_id.to_string())
            .collect();
    let first_bound = first_payload["own_sessions_bound"].clone();
    let report = core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect("prune the first process");
    assert_eq!(report.pruned_processes, 1, "the first process was pruned");
    assert!(
        core.processes().get(&first_id).await.is_err(),
        "the pruned process id refuses; it never resolves to the successor"
    );

    // A start under the same key mints a successor: its own id, its own
    // workflow key, a body that runs.
    let second_id = start_process(
        &backend,
        &core,
        &session,
        "turn-start-2",
        START_KEY,
        serde_json::json!({
            "lifetime": "successor",
            "predecessor_sessions": first_sessions,
            "predecessor_bound": first_bound,
        }),
    )
    .await;
    assert_ne!(
        second_id, first_id,
        "the key started a new process: a minted id is never reused"
    );
    let second_terminal = await_terminal(&backend, &core, &second_id).await;
    let second_payload = settled_payload(&second_terminal);
    assert_eq!(
        second_payload["lifetime"],
        serde_json::json!("successor"),
        "the successor's terminal is its own body's output"
    );
    assert_eq!(
        second_payload["process_id"],
        serde_json::json!(second_id.as_str()),
        "the body that settled ran as the successor's process id"
    );
    assert_eq!(
        second_payload["session_store_factory"],
        serde_json::json!(true),
        "the run had its session-store factory"
    );
    assert_eq!(
        second_payload["predecessor_sessions_visible"],
        serde_json::json!([]),
        "the successor sees none of the pruned lifetime's session state"
    );
    assert_eq!(
        second_payload["own_sessions_tombstoned"],
        serde_json::json!([]),
        "none of the successor's derived session ids is tombstoned"
    );

    // Every session id the first lifetime bound stayed tombstoned through
    // the prune.
    assert_eq!(
        second_payload["predecessor_bound_not_deleted"],
        serde_json::json!([]),
        "every session id the first lifetime bound stayed deleted"
    );

    // Restate saw two distinct process workflows, each run once: the
    // successor submitted under its own minted key, never the retired one.
    let runs = engine.runs.lock().unwrap().clone();
    assert_eq!(
        runs,
        vec![first_id.to_string(), second_id.to_string()],
        "each lifetime's body ran exactly once, in order"
    );
    backend.server().settle().await;
    let workflow_runs: Vec<String> = backend
        .server()
        .invocations()
        .into_iter()
        .filter_map(|view| {
            view.target
                .strip_prefix(&format!("{PROCESS_WORKFLOW}/"))
                .and_then(|rest| rest.strip_suffix("/run"))
                .map(str::to_string)
        })
        .collect();
    assert_eq!(
        workflow_runs,
        vec![first_id.to_string(), second_id.to_string()],
        "each lifetime submitted its own workflow key"
    );
    for key in [&first_id, &second_id] {
        let target = format!("{PROCESS_WORKFLOW}/{key}/run");
        let view = backend
            .server()
            .invocations()
            .into_iter()
            .find(|view| view.target == target)
            .unwrap_or_else(|| panic!("no invocation for {target}"));
        assert_eq!(
            view.attempts, 1,
            "{target} ran once — no retry, no re-invocation"
        );
        let outcome = backend
            .server()
            .outcome(&view.id)
            .unwrap_or_else(|| panic!("{target} has no recorded outcome"));
        assert!(outcome.is_ok(), "{target} completed: {outcome:?}");
    }

    // The handler host ran only the two starts' jobs.
    let handler_jobs = backend
        .server()
        .invocations()
        .into_iter()
        .filter(|view| view.target.starts_with(&format!("{HANDLER_HOST}/")))
        .count();
    assert_eq!(handler_jobs, 2, "one handler job per start");

    // The pruned id still refuses, and the successor stays retained at its
    // own terminal.
    assert!(
        core.processes().get(&first_id).await.is_err(),
        "the pruned id still refuses after the successor ran"
    );
    let successor = core
        .processes()
        .get(&second_id)
        .await
        .expect("read the successor")
        .expect("the successor stays retained");
    assert!(
        successor.lifecycle.is_terminal(),
        "the successor is at its own terminal: {:?}",
        successor.lifecycle
    );
}
