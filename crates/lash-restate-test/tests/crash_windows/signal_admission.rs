//! A signal's identity is its append key (FIG-4299).
//!
//! A remote signal request names its process, its signal and its signal id,
//! and nothing else: a host converts it and hands it to the public signal
//! API. The deployment dies after the signal's append committed and before
//! its run result reached the journal; the redrive runs the append again,
//! and the store serves it the event the first run admitted. The log holds
//! one event, the process is woken once, a redelivery of the same signal is
//! served that same event, and the same signal id under changed content is a
//! typed conflict. The law runs on the server double and on a live
//! `restate-server` (the `crash-windows` Restate suite).

use super::*;

const SIGNAL: &str = "go";
/// The recorded step a signal's append runs as.
const APPEND_STEP_SUFFIX: &str = ".process-signal-append:v1";

fn worker(core: &lash::LashCore) -> lash::durability::DurableProcessWorker {
    lash::durability::DurableProcessWorker::new(
        core.durable_process_worker_config().expect("worker config"),
    )
    .expect("fresh process worker")
}

/// ```text
/// process main() signals { go: any } {
///   signal = wait_signal("go")
///   finish signal
/// }
/// ```
async fn waiting_request(engine: &Engine) -> lash_core::ProcessStartRequest {
    let program = b::module(
        vec![b::process_with_signals(
            PROCESS,
            Vec::new(),
            vec![b::signal(SIGNAL, lashlang::TypeExpr::Any)],
            b::block(vec![
                b::assign("signal", b::wait_signal(SIGNAL)),
                b::finish(b::var("signal")),
            ]),
        )],
        Vec::new(),
    );
    let input = publish_program(engine, program, lashlang::LashlangAbilities::default()).await;
    lash_core::ProcessStartRequest::new(
        input.into_process_input().expect("process input"),
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_env_ref(
        lash_core::publish_process_execution_env(
            engine.lash_backend().process_env_store().as_ref(),
            &lash_core::testing::host_pin_claim_for_testing(),
            &(process_env_spec()),
        )
        .await
        .expect("publish captured environment"),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_extra_event_types([lash_core::ProcessEventType {
        name: format!("signal.{SIGNAL}"),
        payload_schema: lash_core::LashSchema::any(),
        semantics: lash_core::ProcessEventSemanticsSpec::default(),
    }])
}

/// Run `job` in a host handler and answer what it returned, or why the
/// handler did not complete.
async fn try_in_handler<T: Send + 'static>(
    engine: &Engine,
    label: &str,
    job: impl for<'a> Fn(
        lash_core::ScopedEffectController<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>
    + Send
    + Sync
    + 'static,
) -> Result<T, String> {
    let answer = Arc::new(Mutex::new(None));
    let attempt: HandlerAttempt = {
        let answer = Arc::clone(&answer);
        let job = Arc::new(job);
        Arc::new(move |scoped| {
            let answer = Arc::clone(&answer);
            let job = Arc::clone(&job);
            Box::pin(async move {
                let value = job(scoped).await;
                *answer.lock().unwrap() = Some(value);
            })
        })
    };
    tokio::time::timeout(
        BOUND,
        engine.run_in_handler(
            lash_core::AdmittedScope::runtime_operation(run_tag(label)),
            attempt,
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("`{label}` finishes"))?;
    Ok(answer
        .lock()
        .unwrap()
        .take()
        .unwrap_or_else(|| panic!("`{label}` answered")))
}

async fn in_handler<T: Send + 'static>(
    engine: &Engine,
    label: &str,
    job: impl for<'a> Fn(
        lash_core::ScopedEffectController<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>
    + Send
    + Sync
    + 'static,
) -> T {
    try_in_handler(engine, label, job)
        .await
        .unwrap_or_else(|error| panic!("`{label}` handler: {error}"))
}

/// Deliver `remote` the way a host serves the remote protocol: convert it,
/// then hand it to the public signal API. Answers the handler's own failure
/// as the outer error.
async fn try_deliver(
    engine: &Engine,
    core: &lash::LashCore,
    label: &str,
    remote: lash::remote::processes::RemoteProcessSignalRequest,
) -> Result<Result<lash_core::ProcessEvent, String>, String> {
    let core = core.clone();
    try_in_handler(engine, label, move |scoped| {
        let core = core.clone();
        let remote = remote.clone();
        Box::pin(async move {
            let signal = lash_core::ProcessSignal::try_from(remote.clone())
                .map_err(|error| error.to_string())?;
            // Only the typed conflict is an answer; any other error fails the
            // attempt, so a retry never journals a return the first attempt
            // did not.
            match core.processes().signal(signal, scoped).await {
                Ok(event) => Ok(event),
                Err(lash::EmbedError::Plugin(error))
                    if lash_core::is_durable_identity_conflict(&error) =>
                {
                    Err(format!("{error:?}"))
                }
                Err(error) => panic!("`{}` signal attempt failed: {error:?}", remote.signal_id),
            }
        })
    })
    .await
}

/// [`try_deliver`] whose handler must complete.
async fn deliver(
    engine: &Engine,
    core: &lash::LashCore,
    label: &str,
    remote: lash::remote::processes::RemoteProcessSignalRequest,
) -> Result<lash_core::ProcessEvent, String> {
    try_deliver(engine, core, label, remote)
        .await
        .unwrap_or_else(|error| panic!("`{label}` handler: {error}"))
}

async fn signal_events(engine: &Engine, id: &ProcessId) -> Vec<lash_core::ProcessEvent> {
    lash_core::ProcessEventLogTestSupport::full_event_window(
        engine.lash_backend().process_registry().as_ref(),
        id,
        0,
    )
    .await
    .expect("read the event log")
    .into_iter()
    .filter(|event| event.event_type == format!("signal.{SIGNAL}"))
    .collect()
}

pub(super) async fn signal_id_deduplicates_append_without_caller_replay_key(engine: Engine) {
    let executions = Arc::new(AtomicUsize::new(0));
    let core = process_core(&engine, &executions);
    engine.install_process_worker(worker(&core));
    let request = waiting_request(&engine).await;
    let process_id = {
        let core = core.clone();
        in_handler(&engine, "signal-admission-start", move |scoped| {
            let core = core.clone();
            let request = request.clone();
            Box::pin(async move {
                core.processes()
                    .start(request, scoped)
                    .await
                    .expect("start the process")
                    .process_id
            })
        })
        .await
    };
    tokio::time::timeout(BOUND, async {
        loop {
            let record = engine
                .lash_backend()
                .process_registry()
                .get_process(&process_id)
                .await
                .expect("process read")
                .expect("process exists");
            if matches!(record.wait.as_ref().map(|wait| &wait.kind), Some(lash_core::WaitKind::Signal { name, .. }) if name == SIGNAL)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the process waits for the signal");

    let remote = |payload: serde_json::Value| lash::remote::processes::RemoteProcessSignalRequest {
        process_id: process_id.clone(),
        signal_name: SIGNAL.to_owned(),
        signal_id: "signal-s".to_owned(),
        payload,
    };
    // The append commits; the deployment dies before its run result reaches
    // the journal, and the append runs again.
    let crashes_before = engine.crashes();
    engine.crash_on(CrashRule::new(CrashPoint::BeforeRunResultEnding {
        suffix: APPEND_STEP_SUFFIX.to_owned(),
    }));
    let first = match try_deliver(
        &engine,
        &core,
        "signal-admission-s",
        remote(json!({"go": 1})),
    )
    .await
    {
        // The server double redrives the crashed handler into the same
        // invocation, whose append runs again.
        Ok(answer) => answer.expect("deliver S"),
        // A live deployment's death takes its host's parked handler jobs with
        // it, so that invocation fails for good; the host that comes back
        // delivers S again from a new owning invocation, whose append runs
        // again.
        Err(_) if matches!(engine, Engine::Live { .. }) => deliver(
            &engine,
            &core,
            "signal-admission-s-redelivered",
            remote(json!({"go": 1})),
        )
        .await
        .expect("redeliver S after the deployment died"),
        Err(error) => panic!("`signal-admission-s` handler: {error}"),
    };
    assert!(
        engine.crashes() > crashes_before,
        "the deployment died between the append and its journal"
    );
    let events = signal_events(&engine, &process_id).await;
    assert_eq!(events.len(), 1, "S is appended exactly once: {events:#?}");
    assert_eq!(events[0].sequence, first.sequence);
    assert_eq!(events[0].payload, json!({"go": 1}));
    assert_eq!(
        events[0]
            .invocation
            .replay
            .as_ref()
            .map(|replay| replay.key.clone()),
        Some(
            lash_core::ProcessSignalIdentity::new(process_id.clone(), SIGNAL, "signal-s")
                .expect("signal identity")
                .append_key()
        ),
        "the append is keyed by the signal's identity"
    );
    assert_eq!(
        events[0].semantics.signal_wait,
        Some(lash_core::ProcessSignalWaitBinding { ordinal: 1 }),
        "S is bound to the wait it was admitted to"
    );

    let output = tokio::time::timeout(BOUND, core.processes().await_output(&process_id))
        .await
        .expect("the process finishes")
        .expect("the process output");
    let lash_core::ToolCallOutcome::Success(value) = output.into_tool_output().outcome else {
        panic!("the process finishes with the signal it was woken by");
    };
    assert_eq!(value.to_json_value(), json!({"go": 1}), "one wake, by S");

    // Redelivering S is served the event it admitted.
    let again = deliver(
        &engine,
        &core,
        "signal-admission-s-again",
        remote(json!({"go": 1})),
    )
    .await
    .expect("redeliver S");
    assert_eq!(format!("{again:?}"), format!("{first:?}"));
    // S under changed content is a typed conflict, never a second event.
    let changed = deliver(
        &engine,
        &core,
        "signal-admission-s-changed",
        remote(json!({"go": 2})),
    )
    .await
    .expect_err("S under changed content conflicts");
    assert!(changed.contains("DurableIdentityConflict"), "{changed}");
    assert_eq!(signal_events(&engine, &process_id).await.len(), 1);
    engine.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signal_id_deduplicates_append_without_caller_replay_key_on_the_double() {
    signal_id_deduplicates_append_without_caller_replay_key(Engine::double(0x4299, None).await)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the crash-windows Restate suite runs it"]
async fn live_restate_signal_id_deduplicates_append_without_caller_replay_key() {
    signal_id_deduplicates_append_without_caller_replay_key(
        Engine::live("signal-admission", None).await,
    )
    .await;
}
