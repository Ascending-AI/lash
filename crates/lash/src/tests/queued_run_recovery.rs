use super::*;

struct RetryProbe {
    provider_calls: AtomicUsize,
    hook_calls: AtomicUsize,
    hook_entered: tokio::sync::Notify,
    hook_release: tokio::sync::Semaphore,
    requests: StdMutex<Vec<LlmRequest>>,
}

impl Default for RetryProbe {
    fn default() -> Self {
        Self {
            provider_calls: AtomicUsize::new(0),
            hook_calls: AtomicUsize::new(0),
            hook_entered: tokio::sync::Notify::new(),
            hook_release: tokio::sync::Semaphore::new(0),
            requests: StdMutex::new(Vec::new()),
        }
    }
}

struct RetryHook(Arc<RetryProbe>);

impl lash_core::facade_support::PluginFactory for RetryHook {
    fn id(&self) -> &'static str {
        "queued-run-retry"
    }

    fn build(
        &self,
        _: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        Ok(Arc::new(Self(Arc::clone(&self.0))))
    }
}

impl lash_core::facade_support::SessionPlugin for RetryHook {
    fn id(&self) -> &'static str {
        "queued-run-retry"
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        let probe = Arc::clone(&self.0);
        reg.output().response(Arc::new(move |context| {
            let probe = Arc::clone(&probe);
            Box::pin(async move {
                let attempt = probe.hook_calls.fetch_add(1, Ordering::SeqCst);
                probe.hook_entered.notify_one();
                probe
                    .hook_release
                    .acquire()
                    .await
                    .expect("hook barrier remains open")
                    .forget();
                if attempt == 0 {
                    return Err(lash_core::PluginError::Invoke(
                        "transient response derivation".into(),
                    ));
                }
                Ok(lash_core::facade_support::AssistantResponseTransform {
                    response: context.response,
                    events: Vec::new(),
                })
            })
        }));
        Ok(())
    }
}

/// The effect journal of the backend rooted at `directory/sessions`.
fn effect_journal(directory: &std::path::Path) -> std::path::PathBuf {
    directory
        .join("sessions")
        .join(lash_sqlite_store::SqliteDatabase::EffectReplay.file_name())
}

fn recorded_effects(path: &std::path::Path) -> Vec<(String, String, serde_json::Value)> {
    let database = rusqlite::Connection::open(path).expect("inspect durable journal");
    let mut statement = database.prepare("SELECT scope_id, replay_key, envelope_json FROM runtime_effect_replay WHERE outcome_json IS NOT NULL ORDER BY rowid").unwrap();
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .map(|row| {
            let (scope, key, json) = row.unwrap();
            (
                scope,
                key,
                serde_json::from_str::<serde_json::Value>(
                    serde_json::from_str::<serde_json::Value>(&json).unwrap()["json"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap(),
            )
        })
        .collect()
}

fn recorded_provider_effects(path: &std::path::Path) -> Vec<(String, String, serde_json::Value)> {
    recorded_effects(path)
        .into_iter()
        .filter(|(_, _, envelope)| envelope["command"]["type"] == "llm_call")
        .collect()
}

struct ColdCommitBoundary {
    phase: lash_core::runtime::RuntimeTurnPhase,
    reached: Arc<tokio::sync::Notify>,
    skip: AtomicUsize,
}
impl lash_core::runtime::RuntimeTurnPhaseProbe for ColdCommitBoundary {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == self.phase {
            if self
                .skip
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return;
            }
            self.reached.notify_one();
            loop {
                std::thread::park();
            }
        }
    }
    fn end(&self, _: lash_core::runtime::RuntimeTurnPhase) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automatic_queued_retry_reuses_recorded_completion_before_new_arrivals() -> Result<()> {
    let directory = tempfile::tempdir().expect("temporary durable assembly");
    let probe = Arc::new(RetryProbe::default());
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let probe = Arc::clone(&probe);
            move |request| {
                let probe = Arc::clone(&probe);
                async move {
                    probe.provider_calls.fetch_add(1, Ordering::SeqCst);
                    probe.requests.lock_recover().push(request);
                    Ok(text_response("recorded completion"))
                }
            }
        })
        .build()
        .into_handle();
    let backend = Arc::new(
        lash_sqlite_store::SqliteBackend::open(directory.path().join("sessions"))
            .await
            .expect("open the SQLite backend"),
    );
    let core = explicit_ephemeral_facets_with_backend_work(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(provider)
    .model(mock_model_spec())
    .plugin(Arc::new(RetryHook(Arc::clone(&probe))))
    .native_substrate_config(lash_core::NativeSubstrateConfig {
        work_cadence: lash_core::WorkCadencePolicy {
            retry_initial: std::time::Duration::from_millis(50),
            retry_max: std::time::Duration::from_millis(50),
            ..Default::default()
        },
        ..Default::default()
    })
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("automatic-queued-retry").open().await?;
    session
        .durable()
        .enqueue(TurnInput::text("first admitted input"))
        .id("first")
        .send()
        .await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        probe.hook_entered.notified(),
    )
    .await
    .expect("automatic scheduler reaches first hook");
    assert_eq!(probe.provider_calls.load(Ordering::SeqCst), 1);
    let first_journal = recorded_provider_effects(&effect_journal(directory.path()));
    assert_eq!(
        first_journal.len(),
        1,
        "phase 1 is durable before the hook fails"
    );
    session
        .durable()
        .enqueue(TurnInput::text("later arrival"))
        .id("later")
        .send()
        .await?;
    probe.hook_release.add_permits(1);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        probe.hook_entered.notified(),
    )
    .await
    .expect("automatic scheduler retries the actual response hook");
    assert_eq!(
        probe.hook_calls.load(Ordering::SeqCst),
        2,
        "the hook recovers after its transient failure"
    );
    assert_eq!(
        probe.provider_calls.load(Ordering::SeqCst),
        1,
        "retry must reuse the recorded completion at the admitted physical position"
    );
    assert_eq!(
        recorded_provider_effects(&effect_journal(directory.path())),
        first_journal,
        "retry reaches phase 2 with the same recorded scope, replay key and physical attribution"
    );
    let first_request = serde_json::to_string(&probe.requests.lock_recover()[0].messages).unwrap();
    assert!(first_request.contains("first admitted input"));
    assert!(!first_request.contains("later arrival"));
    probe.hook_release.add_permits(16);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if session
                .durable()
                .pending_turn_inputs()
                .await
                .expect("pending inputs")
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "both admitted runs settle: {error}; provider calls {}",
            probe.provider_calls.load(Ordering::SeqCst)
        )
    });
    assert_eq!(
        probe.provider_calls.load(Ordering::SeqCst),
        2,
        "a distinct run executes independently"
    );
    session
        .durable()
        .enqueue(TurnInput::text("distinct submission"))
        .id("distinct")
        .send()
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if probe.provider_calls.load(Ordering::SeqCst) == 3
                && session
                    .durable()
                    .pending_turn_inputs()
                    .await
                    .unwrap()
                    .is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("new submission settles independently");
    let journal = recorded_provider_effects(&effect_journal(directory.path()));
    assert_eq!(journal.len(), 3);
    assert_ne!(
        journal[0].0, journal[2].0,
        "a later distinct run has a new admission identity"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "subprocess entry point for queued recovery"]
#[expect(
    clippy::disallowed_methods,
    reason = "cold-recovery harness owns worker processes and durable test files"
)]
async fn cold_queued_child_process() -> Result<()> {
    use std::io::Write as _;
    let Ok(action) = std::env::var("LASH_QUEUED_COLD_ACTION") else {
        return Ok(());
    };
    let directory = std::path::PathBuf::from(std::env::var("LASH_QUEUED_COLD_DIRECTORY").unwrap());
    let crash = action == "crash";
    let boundary = std::env::var("LASH_QUEUED_COLD_BOUNDARY").unwrap();
    let clock: Arc<dyn lash_core::Clock> = Arc::new(lash_core::testing::TestClock::new(if crash {
        1_800_000_000_000
    } else {
        1_800_000_600_000
    }));
    let probe = Arc::new(RetryProbe::default());
    if boundary != "completion" {
        probe.hook_calls.store(1, Ordering::SeqCst);
    }
    if !crash {
        probe.hook_calls.store(1, Ordering::SeqCst);
        probe.hook_release.add_permits(32);
    }
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let marker = directory.join("provider-count");
            let follow_on = boundary == "follow-on";
            move |_| {
                let marker = marker.clone();
                async move {
                    let previous = std::fs::read_to_string(&marker)
                        .unwrap_or_default()
                        .lines()
                        .count();
                    let mut file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&marker)
                        .unwrap();
                    writeln!(file, "completion").unwrap();
                    file.sync_all().unwrap();
                    if follow_on && previous == 0 {
                        Ok(LlmResponse {
                            parts: vec![lash_core::LlmOutputPart::ToolCall {
                                call_id: "cold-switch".into(),
                                tool_name: "switch_frame".into(),
                                input_json: r#"{"task":"cold continuation"}"#.into(),
                                replay: None,
                            }],
                            ..LlmResponse::default()
                        })
                    } else {
                        Ok(text_response("cold recorded completion"))
                    }
                }
            }
        })
        .build()
        .into_handle();
    let backend = lash_sqlite_store::SqliteBackend::open_with_options_and_clock(
        directory.join("sessions"),
        lash_sqlite_store::SqliteBackendOptions::default(),
        clock,
    )
    .await
    .expect("open the SQLite backend");
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        Arc::new(backend),
        crate::TurnBudget::Unbounded,
    ))
    .provider(provider)
    .model(mock_model_spec())
    .tools(Arc::new(AgentFrameSwitchTools))
    .plugin(Arc::new(RetryHook(Arc::clone(&probe))))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("cold-queued-recovery").open().await?;
    let boundary_reached = Arc::new(tokio::sync::Notify::new());
    if crash && boundary != "completion" {
        session
            .set_turn_phase_probe(Arc::new(ColdCommitBoundary {
                phase: if boundary == "checkpoint" || boundary == "follow-on" {
                    lash_core::runtime::RuntimeTurnPhase::PreparedTurn
                } else {
                    lash_core::runtime::RuntimeTurnPhase::PostCommitDelivery
                },
                reached: Arc::clone(&boundary_reached),
                skip: AtomicUsize::new(usize::from(boundary == "follow-on")),
            }))
            .await;
    }
    if crash {
        session
            .durable()
            .enqueue(TurnInput::text("cold input"))
            .id("cold-input")
            .send()
            .await?;
        let running_session = session.clone();
        let running = tokio::spawn(async move { running_session.queued_turn().run().await });
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            probe.hook_entered.notified(),
        )
        .await
        .expect("cold driver reaches journaled completion");
        let admission = session
            .durable()
            .pending_queued_run()
            .await?
            .expect("admitted run");
        std::fs::write(
            directory.join("admission.json"),
            serde_json::to_vec(&admission).unwrap(),
        )
        .unwrap();
        if boundary != "completion" {
            probe.hook_release.add_permits(32);
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                boundary_reached.notified(),
            )
            .await
            .expect("child reaches selected durable boundary");
        }
        if boundary == "follow-on" {
            let admission = session
                .durable()
                .pending_queued_run()
                .await?
                .expect("follow-on remains pending");
            assert_eq!(admission.position.physical_ordinal, 1);
            assert_eq!(admission.position.turn_index, 2);
            assert!(
                session.durable().pending_turn_inputs().await?.is_empty(),
                "first physical input already settled"
            );
            std::fs::write(
                directory.join("admission.json"),
                serde_json::to_vec(&admission).unwrap(),
            )
            .unwrap();
        }
        println!("crash_ready");
        std::io::stdout().flush().unwrap();
        running.await.unwrap()?;
        panic!("parent must kill the paused child");
    }
    let recorded: lash_core::store::QueuedRunAdmission =
        serde_json::from_slice(&std::fs::read(directory.join("admission.json")).unwrap()).unwrap();
    assert_eq!(
        recorded.origin,
        lash_core::store::QueuedRunOrigin::Anonymous
    );
    if boundary == "terminal" {
        assert!(session.durable().pending_queued_run().await?.is_none());
        let replay = session
            .queued_turn()
            .drain_id(recorded.scope.id())
            .run()
            .await?;
        assert!(
            matches!(replay, crate::QueuedTurnDrain::Replayed(ref receipt) if receipt.scope == recorded.scope && receipt.terminal.is_some())
        );
    } else {
        let before = session
            .durable()
            .pending_queued_run()
            .await?
            .expect("cold admission is discoverable");
        assert_eq!(before.scope, recorded.scope);
        assert_eq!(before.origin, recorded.origin);
        assert_eq!(before.position, recorded.position);
        session
            .queued_turn()
            .run()
            .await?
            .expect("cold drain resumes");
    }
    assert!(session.durable().pending_queued_run().await?.is_none());
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    println!("recovered");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_completion_survives_process_death() {
    run_cold_queued_boundary("completion").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_checkpoint_survives_process_death() {
    run_cold_queued_boundary("checkpoint").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_terminal_receipt_survives_process_death_before_reply() {
    run_cold_queued_boundary("terminal").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_follow_on_completion_survives_process_death() {
    run_cold_queued_boundary("follow-on").await;
}
#[expect(
    clippy::disallowed_methods,
    reason = "cold-recovery harness owns worker processes and durable test files"
)]
async fn run_cold_queued_boundary(boundary: &str) {
    use tokio::io::AsyncBufReadExt as _;
    let directory = tempfile::tempdir().unwrap();
    let command = |action: &str| {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "--ignored",
                "tests::queued_run_recovery::cold_queued_child_process",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("LASH_QUEUED_COLD_ACTION", action)
            .env("LASH_QUEUED_COLD_BOUNDARY", boundary)
            .env("LASH_QUEUED_COLD_DIRECTORY", directory.path())
            .kill_on_drop(true);
        command
    };
    let mut crashed = command("crash")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(crashed.stdout.take().unwrap()).lines();
    loop {
        let line = lines
            .next_line()
            .await
            .unwrap()
            .expect("child reaches completion boundary");
        if line.ends_with("crash_ready") {
            break;
        }
    }
    crashed.kill().await.unwrap();
    assert!(!crashed.wait().await.unwrap().success());
    let recorded = recorded_provider_effects(&effect_journal(directory.path()));
    if boundary == "checkpoint" {
        assert!(
            recorded_effects(&effect_journal(directory.path()))
                .iter()
                .any(|(_, _, envelope)| envelope["command"]["type"] == "checkpoint"),
            "checkpoint outcome is journaled before process death"
        );
    }
    let recovered = command("recover").output().await.unwrap();
    assert!(
        recovered.status.success(),
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&recovered.stdout),
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("provider-count"))
            .unwrap()
            .lines()
            .count(),
        if boundary == "follow-on" { 2 } else { 1 },
        "provider counter outside both workers proves cold replay"
    );
    assert_eq!(
        recorded_provider_effects(&effect_journal(directory.path())),
        recorded
    );
}

struct ExhaustedWake(Arc<tokio::sync::Notify>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ExhaustedWake {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct EventName(bool);
        impl tracing::field::Visit for EventName {
            fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "event" && value == "queued_work.wake_exhausted" {
                    self.0 = true;
                }
            }
        }
        let mut name = EventName(false);
        event.record(&mut name);
        if name.0 {
            self.0.notify_one();
        }
    }
}

#[tokio::test]
async fn exhausted_input_root_resumes_or_is_withdrawn_without_new_input() -> Result<()> {
    use tracing_subscriber::prelude::*;
    for abandon in [false, true] {
        let exhausted = Arc::new(tokio::sync::Notify::new());
        let subscriber = tracing_subscriber::registry().with(ExhaustedWake(Arc::clone(&exhausted)));
        let _dispatch = tracing::subscriber::set_default(subscriber);
        let directory = tempfile::tempdir().unwrap();
        let probe = Arc::new(RetryProbe::default());
        let provider = crate::testing::TestProvider::builder()
            .kind("embed-test")
            .complete({
                let probe = Arc::clone(&probe);
                move |request| {
                    let probe = Arc::clone(&probe);
                    async move {
                        probe.provider_calls.fetch_add(1, Ordering::SeqCst);
                        probe.requests.lock_recover().push(request);
                        Ok(text_response("recovered after exhaustion"))
                    }
                }
            })
            .build()
            .into_handle();
        let backend = Arc::new(
            lash_sqlite_store::SqliteBackend::open(directory.path().join("sessions"))
                .await
                .expect("open the SQLite backend"),
        );
        let core = explicit_ephemeral_facets_with_backend_work(LashCore::standard_builder(
            backend.clone(),
            crate::TurnBudget::Unbounded,
        ))
        .provider(provider)
        .model(mock_model_spec())
        .plugin(Arc::new(RetryHook(Arc::clone(&probe))))
        .native_substrate_config(lash_core::NativeSubstrateConfig {
            work_cadence: lash_core::WorkCadencePolicy {
                max_transient_attempts: std::num::NonZeroU32::new(1).unwrap(),
                retry_initial: std::time::Duration::from_millis(50),
                retry_max: std::time::Duration::from_millis(50),
                ..Default::default()
            },
            ..Default::default()
        })
        .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("exhausted-queued-run").open().await?;
        session
            .durable()
            .enqueue(TurnInput::text("single submission"))
            .id("original")
            .send()
            .await?;
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            probe.hook_entered.notified(),
        )
        .await
        .unwrap();
        let recorded = recorded_provider_effects(&effect_journal(directory.path()));
        probe.hook_release.add_permits(1);
        tokio::time::timeout(std::time::Duration::from_secs(10), exhausted.notified())
            .await
            .expect("scheduler exhausts its physical retry budget");
        // Exhaustion leaves the accepted input pending: the input root keeps
        // its recorded history, and the next drive of the session resumes it
        // (FIG-3600). The host may withdraw it instead.
        let pending = session.durable().pending_turn_inputs().await?;
        assert_eq!(pending.len(), 1, "exhaustion keeps the accepted input");
        assert_eq!(probe.provider_calls.load(Ordering::SeqCst), 1);
        probe.hook_release.add_permits(16);
        if abandon {
            let cancelled = session
                .durable()
                .cancel_pending_turn_input(&pending[0].input.input_id)
                .await?;
            assert!(cancelled.is_cancelled(), "{cancelled:?}");
        } else {
            // The host asks the engine to drive the session and waits for the
            // drive to resume the exhausted root.
            let ports = core.substrate_slot.ports().await;
            ports.queued.schedule_drive(
                &SessionId::from("exhausted-queued-run"),
                lash_core::engine::DriveRequestId::new("host resumes exhausted admission"),
            );
            for _ in 0..1_000 {
                if session.durable().pending_turn_inputs().await?.is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(
                probe.provider_calls.load(Ordering::SeqCst),
                1,
                "the resumed root replays its recorded model call"
            );
            assert_eq!(
                recorded_provider_effects(&effect_journal(directory.path())),
                recorded
            );
            assert_eq!(probe.hook_calls.load(Ordering::SeqCst), 2);
        }
        assert!(session.durable().pending_queued_run().await?.is_none());
        assert!(session.durable().pending_turn_inputs().await?.is_empty());
        // The engine's drive may still be releasing the session's lane.
        let mut attempts = 0;
        loop {
            match session
                .turn(TurnInput::text("direct work after disposition"))
                .turn_id("direct-after-disposition")
                .run()
                .await
            {
                Ok(_) => break,
                Err(crate::EmbedError::Runtime(error))
                    if error.code == lash_core::RuntimeErrorCode::SessionExecutionLaneBusy
                        && attempts < 500 =>
                {
                    attempts += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(error) => return Err(error),
            }
        }
        assert_eq!(probe.provider_calls.load(Ordering::SeqCst), 2);
    }
    Ok(())
}

#[cfg(feature = "rlm")]
struct StopQueuedTool;

#[cfg(feature = "rlm")]
#[async_trait]
impl ToolProvider for StopQueuedTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        AppTools.tool_manifests()
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        AppTools.resolve_contract(name)
    }
    async fn execute(&self, _: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({}))
            .with_control(lash_core::ToolControl::Fail {
                failure: lash_core::ToolFailure::tool(
                    lash_core::ToolFailureClass::Execution,
                    "stopped",
                    "stop this physical turn",
                ),
            })
            .into()
    }
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn stopped_queued_turn_runs_withheld_input_in_a_follow_on() -> Result<()> {
    let durable = Arc::new(StdMutex::new(None::<crate::DurableSession>));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let durable = Arc::clone(&durable);
            let requests = Arc::clone(&requests);
            move |request| {
                let durable = Arc::clone(&durable);
                let requests = Arc::clone(&requests);
                async move {
                    let call = {
                        let mut requests = requests.lock_recover();
                        requests.push(request);
                        requests.len()
                    };
                    let source = if call == 1 {
                        let session = durable.lock_recover().clone().unwrap();
                        session
                            .enqueue(TurnInput::text("withheld after tool stop"))
                            .id("withheld-input")
                            .ingress(lash_core::TurnInputIngress::active_turn(
                                lash_core::TurnId::from("stopped-withheld"),
                                lash_core::TurnInputCheckpointBoundary::BeforeCompletion,
                            ))
                            .send()
                            .await
                            .unwrap();
                        "await tools.app_lookup({});"
                    } else {
                        assert_eq!(call, 2, "one follow-on consumes the withheld input");
                        "finish('withheld completed');"
                    };
                    Ok(text_response(&typescript_block(source)))
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(rlm_core_builder_over(memory_backend().await))
        .provider(provider)
        .model(mock_model_spec())
        .tools(Arc::new(StopQueuedTool))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("stopped-withheld").open().await?;
    *durable.lock_recover() = Some(session.durable());
    session
        .durable()
        .enqueue(TurnInput::text("start tool stop"))
        .send()
        .await?;
    let output = session
        .queued_turn()
        .drain_id("stopped-withheld")
        .run()
        .await?
        .expect("queued run executes");
    assert_eq!(
        requests.lock_recover().len(),
        2,
        "stopped physical turn must not cancel withheld work: {output:?}"
    );
    assert!(request_text(&requests.lock_recover()[1]).contains("withheld after tool stop"));
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!("withheld completed"))
    );
    assert!(
        session
            .durable()
            .turn_input_applications()
            .await?
            .iter()
            .any(|application| application.source_key.as_deref() == Some("host:withheld-input"))
    );
    assert!(session.durable().pending_queued_run().await?.is_none());
    Ok(())
}
