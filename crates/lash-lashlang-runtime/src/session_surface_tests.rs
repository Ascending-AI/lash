use super::*;
use std::sync::Arc;

use lash_core::facade_support::{
    PluginHost, PluginSessionContext, PluginSpec, PluginSpecFactory, RuntimeHostConfig,
    watch_process_registry,
};
use lash_core::{
    AdmittedProcessIdentity, ArtifactOwner, CommitBudget, NativeProcessWork, NoSessionWork,
    OnParentEnd, ParentScope, PluginError, PluginOptions, ProcessExecutionEnvSpec,
    ProcessExecutionEnvStore, ProcessLifecyclePolicy, ProcessProvenance, ProcessRegistration,
    ProcessRegistry, QueuedWorkBatchingConfig, RecoveryContract, SessionPolicy, TurnBudget,
};
use lash_core_worker::{DurableProcessWorker, DurableProcessWorkerConfig, WorkerProcessWork};
use lashlang::testing::ast_builders as b;

const SURFACE_PLUGIN_ID: &str = "fig3344.session-surface";

#[derive(serde::Deserialize, serde::Serialize)]
struct SessionSurfaceOptions {
    grant_vocabulary: bool,
}

/// A catalog carrying a named data type and a value constructor absent from
/// the engine's static surface.
fn session_surface_resources() -> lashlang::LashlangHostCatalog {
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_named_data_type(
            lashlang::NamedDataType::object(
                "fig3344.Widget",
                vec![lashlang::TypeField {
                    name: "name".into(),
                    ty: lashlang::TypeExpr::Str,
                    optional: false,
                }],
            )
            .expect("valid widget type"),
        )
        .expect("widget type is unique");
    resources
        .add_value_constructor(
            ["fig3344", "Make"],
            lashlang::TypeExpr::Object(Vec::new()),
            lashlang::TypeExpr::Ref("fig3344.Widget".into()),
        )
        .expect("value constructor is unique");
    resources
}

fn session_surface_contribution() -> LashlangSurfaceContribution {
    LashlangSurfaceContribution::new(
        lashlang::LashlangAbilities::default(),
        lashlang::LashlangLanguageFeatures::default(),
        session_surface_resources(),
    )
}

/// `process unused() -> fig3344.Widget { finish fig3344.Make({}) }`
/// `process main() -> str { finish "ok" }`
///
/// `unused` is never executed; it is present so the module's host
/// requirements carry the named data type and value constructor.
fn module_requiring_session_surface() -> lashlang::Program {
    let unused = b::process_returning(
        "unused",
        Vec::new(),
        lashlang::TypeExpr::Ref("fig3344.Widget".into()),
        b::finish(b::receiver_call(
            b::resource(&["fig3344"]),
            "Make",
            vec![b::record(Vec::new())],
        )),
    );
    let main = b::process_returning(
        "main",
        Vec::new(),
        lashlang::TypeExpr::Str,
        b::finish(b::string("ok")),
    );
    b::module(vec![unused, main], Vec::new())
}

fn session_policy() -> SessionPolicy {
    SessionPolicy {
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("session surface test model"),
        ..SessionPolicy::new(TurnBudget::Unbounded)
    }
}

fn surface_plugin_factory() -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(PluginSpecFactory::new(
        SURFACE_PLUGIN_ID,
        Arc::new(|ctx: &PluginSessionContext| {
            let grant_vocabulary = ctx
                .plugin_options
                .decode::<SessionSurfaceOptions>(SURFACE_PLUGIN_ID)
                .map_err(|error| {
                    PluginError::Registration(format!("invalid session surface options: {error}"))
                })?
                .is_some_and(|options| options.grant_vocabulary);
            let spec = if grant_vocabulary {
                PluginSpec::new().with_extension_contribution(
                    lashlang_surface_extension(&session_surface_contribution())
                        .map_err(|error| PluginError::Registration(error.to_string()))?,
                )
            } else {
                PluginSpec::new()
            };
            Ok(spec)
        }),
    ))
}

/// Runs `main` through a durable worker whose engine surface lacks the named
/// data type and value constructor, granting them (when `grant`) only through
/// the per-process plugin options the session plugin reads.
async fn run_session_surface_case(grant: bool) -> lash_core::ProcessAwaitOutput {
    let artifact_store: LashlangArtifacts = crate::lib_tests::memory_artifact_store().await;
    let environment = lashlang::LashlangHostEnvironment::new(
        session_surface_resources(),
        lashlang::LashlangAbilities::default(),
    );
    let linked = lashlang::LinkedModule::link(module_requiring_session_surface(), &environment)
        .expect("module links against the granted surface");
    artifact_store
        .publish_module_artifact(
            &ArtifactOwner::host("fig3344-session-surface"),
            &linked.artifact,
        )
        .await
        .expect("module artifact publishes");

    let process_input = LashlangProcessInput {
        module_ref: linked.artifact.module_ref().clone(),
        process_ref: linked
            .artifact
            .process_ref("main")
            .expect("main process ref")
            .clone(),
        host_requirements_ref: linked.artifact.host_requirements_ref().clone(),
        process_name: "main".to_string(),
        args: serde_json::Map::new(),
    };
    let process_identity = process_input.process_identity();

    let sqlite_backend = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open a SQLite memory backend");
    let backend: lash_core::Backend = Arc::new(sqlite_backend.clone()).into();
    let env_store: Arc<dyn ProcessExecutionEnvStore> = backend.process_env_store();
    let plugin_options = if grant {
        PluginOptions::typed(
            SURFACE_PLUGIN_ID,
            SessionSurfaceOptions {
                grant_vocabulary: true,
            },
        )
        .expect("session surface options encode")
    } else {
        PluginOptions::empty()
    };
    let env_ref = lash_core::runtime::publish_process_execution_env(
        env_store.as_ref(),
        &ArtifactOwner::host("fig3344-session-surface-env"),
        &ProcessExecutionEnvSpec::new(plugin_options, session_policy()),
    )
    .await
    .expect("process execution env publishes");

    let registry: Arc<dyn ProcessRegistry> = backend.process_registry();
    let watched = watch_process_registry(Arc::clone(&registry));
    let engine = LashlangProcessEngine::new(
        artifact_store.clone(),
        LashlangSurface::new(
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangLanguageFeatures::default(),
            lashlang::LashlangHostCatalog::new(),
        ),
    );
    let runtime_host = RuntimeHostConfig::new(
        backend.clone(),
        CommitBudget::bounded(1024 * 1024, 512),
        QueuedWorkBatchingConfig::new(1),
    )
    .with_process_engine_registration(lashlang_process_engine_registration(engine));

    let mut factories = lash_core::testing::test_code_protocol_factories();
    factories.push(surface_plugin_factory());
    let worker = DurableProcessWorker::new(
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(factories)),
            runtime_host,
            WorkerProcessWork::SelfNative(watched),
            Arc::new(NoSessionWork::new()),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_policy(session_policy()),
    )
    .expect("valid session surface worker");

    let registration = ProcessRegistration::new(
        process_input
            .into_process_input()
            .expect("process input encodes"),
        RecoveryContract::Rerunnable,
        ProcessProvenance::host(),
        ProcessLifecyclePolicy::new(ParentScope::Host, OnParentEnd::Abandon),
    )
    .with_admitted_identity(AdmittedProcessIdentity::for_testing(process_identity))
    .with_execution_env_ref(Some(env_ref));
    let process_id = registry
        .register_process(registration)
        .await
        .expect("process registers")
        .id;
    let _report = worker
        .drive_pending_processes()
        .await
        .expect("worker drives the process");

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        NativeProcessWork::for_registry(Arc::clone(&registry)).await_terminal(&process_id),
    )
    .await
    .expect("process reaches terminal state")
    .expect("await session surface process")
}

#[tokio::test(flavor = "current_thread")]
async fn session_plugin_surface_is_admitted_and_runs() {
    let terminal = run_session_surface_case(true).await;
    assert!(
        matches!(
            terminal,
            lash_core::ProcessAwaitOutput::Settled { ref output } if output.is_success()
        ),
        "session-contributed surface must admit and run the process: {terminal:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn absent_session_surface_is_refused_at_admission() {
    let terminal = run_session_surface_case(false).await;
    let lash_core::ProcessAwaitOutput::Settled { output } = terminal else {
        panic!("refusal must be a settled durable process failure");
    };
    let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
        panic!("refusal must map to a durable failure: {output:?}");
    };
    assert_eq!(
        failure.code,
        LashlangProcessFailureCode::ProcessHostEnvironmentIncompatible.as_str()
    );
    assert!(
        failure.message.contains("fig3344.Widget") || failure.message.contains("fig3344.Make"),
        "refusal must name the missing vocabulary: {failure:?}"
    );
}

struct RecoveryEchoTool {
    executions: Arc<std::sync::atomic::AtomicUsize>,
}

impl RecoveryEchoTool {
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:recovery_echo",
            "recovery_echo",
            "Echo once after process recovery.",
            serde_json::json!({"type":"object","properties":{"line":{"type":"string"}},"required":["line"],"additionalProperties":false}),
            serde_json::json!({"type":"object"}),
        )
        .with_tool_binding(ToolBinding::new(["tools"], "recovery_echo"))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for RecoveryEchoTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "recovery_echo").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if call.args.get("line").and_then(serde_json::Value::as_str) == Some("deny") {
            return lash_core::ToolOutcome::failure(lash_core::ToolFailure {
                class: lash_core::ToolFailureClass::PermissionDenied,
                code: "approval_denied".to_owned(),
                message: "approval was denied".to_owned(),
                source: lash_core::ToolFailureSource::Policy,
                retry: lash_core::ToolRetryStatus::Exhausted { attempts: 3 },
                raw: None,
            })
            .into();
        }
        lash_core::ToolAttemptOutcome::done_without_intents(lash_core::ToolOutcomeDone::ok(
            serde_json::json!({
                "echo": call.args.get("line").and_then(serde_json::Value::as_str)
            }),
        ))
    }
}

fn recovery_echo_catalog() -> lashlang::LashlangHostCatalog {
    let contract = RecoveryEchoTool::definition().contract();
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_contract(
            ["tools"],
            "Tools",
            "recovery_echo",
            "tool:recovery_echo",
            &lashlang::OperationContract::new(
                contract.input_schema.canonical().clone(),
                contract.output_schema.canonical().clone(),
            ),
        )
        .expect("recovery echo catalog");
    catalog
}

struct CrashAfterFirstNodeCompleted {
    graphs: Arc<lash_trace::TraceLashlangGraphStore>,
    crashed: std::sync::atomic::AtomicBool,
    notified: tokio::sync::Notify,
}

impl lash_trace::TraceSink for CrashAfterFirstNodeCompleted {
    fn append(&self, record: &lash_trace::TraceRecord) -> Result<(), lash_trace::TraceSinkError> {
        self.graphs.append(record)?;
        if matches!(
            &record.event,
            lash_trace::TraceEvent::LanguageExecution {
                event: lash_trace::TraceLanguageExecution {
                    payload: lash_trace::TraceLanguageExecutionPayload::NodeCompleted { .. },
                    ..
                },
                ..
            }
        ) && !self.crashed.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.notified.notify_one();
            panic!("injected worker crash after NodeCompleted");
        }
        Ok(())
    }
}

#[tokio::test]
async fn fig3463_crashed_worker_retry_keeps_both_telemetry_attempts_but_executes_effect_once() {
    let artifact_store: LashlangArtifacts = crate::lib_tests::memory_artifact_store().await;
    let environment = lashlang::LashlangHostEnvironment::new(
        recovery_echo_catalog(),
        lashlang::LashlangAbilities::default(),
    );
    let module = b::module(
        vec![b::process(
            "main",
            Vec::new(),
            b::finish(b::receiver_call(
                b::resource(&["tools"]),
                "recovery_echo",
                vec![b::record(vec![("line", b::string("once"))])],
            )),
        )],
        Vec::new(),
    );
    let linked = lashlang::LinkedModule::link(module, &environment).expect("link recovery process");
    artifact_store
        .publish_module_artifact(&ArtifactOwner::host("fig3463-recovery"), &linked.artifact)
        .await
        .expect("publish recovery process");
    let process_input = LashlangProcessInput {
        module_ref: linked.artifact.module_ref().clone(),
        process_ref: linked
            .artifact
            .process_ref("main")
            .expect("main process")
            .clone(),
        host_requirements_ref: linked.artifact.host_requirements_ref().clone(),
        process_name: "main".to_string(),
        args: serde_json::Map::new(),
    };
    let process_identity = process_input.process_identity();
    let sqlite_backend = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open a SQLite memory backend");
    let backend: lash_core::Backend = Arc::new(sqlite_backend.clone()).into();
    let env_store: Arc<dyn ProcessExecutionEnvStore> = backend.process_env_store();
    let env_ref = lash_core::runtime::publish_process_execution_env(
        env_store.as_ref(),
        &ArtifactOwner::host("fig3463-recovery-env"),
        &ProcessExecutionEnvSpec::new(PluginOptions::empty(), session_policy()),
    )
    .await
    .expect("publish process env");
    let registry: Arc<dyn ProcessRegistry> = backend.process_registry();
    let graphs = Arc::new(lash_trace::TraceLashlangGraphStore::default());
    let crash_sink = Arc::new(CrashAfterFirstNodeCompleted {
        graphs: Arc::clone(&graphs),
        crashed: std::sync::atomic::AtomicBool::new(false),
        notified: tokio::sync::Notify::new(),
    });
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // The retry is a second worker over the same backend: a fresh handle on
    // the crashed worker's databases.
    let backend_b: lash_core::Backend = Arc::new(
        sqlite_backend
            .reopen()
            .await
            .expect("reopen the backend for the retry"),
    )
    .into();
    let tool_factory: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "fig3463-recovery-echo",
            PluginSpec::new().with_tool_provider(Arc::new(RecoveryEchoTool {
                executions: Arc::clone(&executions),
            })),
        ));
    let worker = |sink: Arc<dyn lash_trace::TraceSink>, backend: lash_core::Backend| {
        let engine = LashlangProcessEngine::new(artifact_store.clone(), LashlangSurface::default())
            .with_execution_trace(Some(sink), lash_trace::TraceContext::default());
        let runtime_host = RuntimeHostConfig::new(
            backend,
            CommitBudget::bounded(1024 * 1024, 512),
            QueuedWorkBatchingConfig::new(1),
        )
        .with_lease_timings(
            lash_core::facade_support::LeaseTimings::from_ttl(std::time::Duration::from_millis(
                120,
            ))
            .expect("short crash lease"),
        )
        .with_process_engine_registration(lashlang_process_engine_registration(engine));
        let mut factories = lash_core::testing::test_code_protocol_factories();
        factories.push(Arc::clone(&tool_factory));
        DurableProcessWorker::new(
            DurableProcessWorkerConfig::new(
                Arc::new(PluginHost::new(factories)),
                runtime_host,
                WorkerProcessWork::SelfNative(watch_process_registry(Arc::clone(&registry))),
                Arc::new(NoSessionWork::new()),
                lash_core::testing::runtime_lease_owner(),
            )
            .with_session_policy(session_policy()),
        )
        .expect("recovery worker")
    };
    let process_id = registry
        .register_process(
            ProcessRegistration::new(
                process_input.into_process_input().expect("process input"),
                RecoveryContract::Rerunnable,
                ProcessProvenance::host(),
                ProcessLifecyclePolicy::new(ParentScope::Host, OnParentEnd::Abandon),
            )
            .with_admitted_identity(AdmittedProcessIdentity::for_testing(process_identity))
            .with_execution_env_ref(Some(env_ref)),
        )
        .await
        .expect("register recovery process")
        .id;
    let worker_a = worker(crash_sink.clone(), backend.clone());
    let first_report = worker_a
        .drive_pending_processes()
        .await
        .expect("admit first attempt");
    if tokio::time::timeout(
        std::time::Duration::from_secs(5),
        crash_sink.notified.notified(),
    )
    .await
    .is_err()
    {
        panic!(
            "first attempt must crash after NodeCompleted: report={first_report:?}, process={:?}, graphs={:?}",
            registry.get_process(&process_id).await,
            graphs.graphs()
        );
    }
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the effect must already be journaled before the crash"
    );
    drop(worker_a);
    let worker_b = worker(graphs.clone(), backend_b);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let _ = worker_b
                .drive_pending_processes()
                .await
                .expect("drive retry when the crashed worker lease expires");
            if registry
                .get_process(&process_id)
                .await
                .expect("read retried process")
                .is_some_and(|record| record.is_terminal())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("retry is admitted after lease expiry");
    let terminal = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        NativeProcessWork::for_registry(Arc::clone(&registry)).await_terminal(&process_id),
    )
    .await
    .expect("retry reaches terminal")
    .expect("await retried process");
    assert!(
        matches!(terminal, lash_core::ProcessAwaitOutput::Settled { ref output } if output.is_success())
    );
    assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 1);
    let graphs = graphs.graphs();
    assert_eq!(graphs.len(), 2, "both attempts must remain visible");
    let mut attempts = graphs
        .iter()
        .map(|graph| {
            graph
                .history
                .first()
                .expect("attempt has trace history")
                .event
                .identity
                .attempt()
                .expect("process attempt")
        })
        .collect::<Vec<_>>();
    attempts.sort_unstable();
    assert_eq!(attempts, [1, 2]);
    let call_ids = graphs
        .iter()
        .map(|graph| {
            graph
                .history
                .iter()
                .find_map(|record| match &record.event.payload {
                    lash_trace::TraceLanguageExecutionPayload::NodeStarted {
                        call_id: Some(call_id),
                        ..
                    } => Some(call_id.clone()),
                    _ => None,
                })
                .expect("resource node records its effect key")
        })
        .collect::<Vec<_>>();
    assert_eq!(call_ids.len(), 2);
    assert_eq!(
        call_ids[0], call_ids[1],
        "telemetry retry must reuse the journaled effect key"
    );
    assert!(!call_ids[0].contains(":attempt:"));
    for attempt in [1, 2] {
        let graph = graphs
            .iter()
            .find(|graph| graph.history[0].event.identity.attempt() == Some(attempt))
            .expect("one graph per attempt");
        assert!(graph.nodes.iter().any(|node| matches!(
            node.observation,
            lash_trace::TraceLashlangNodeObservation::Completed { occurrence: 1, .. }
        )));
    }
}

/// FIG-3586 (T12, nested process path): a process body whose redrive meets a
/// journal entry that is not the one it issues — here the crashed attempt's
/// recorded tool attempt, rewritten under it — refuses at that entry. Nothing
/// is dispatched, nothing terminal is written, and the process stays
/// claimable, so every sweep refuses again until an operator acts.
#[tokio::test]
async fn a_process_body_whose_journal_diverges_is_refused_and_stays_non_terminal() {
    let artifact_store: LashlangArtifacts = crate::lib_tests::memory_artifact_store().await;
    let environment = lashlang::LashlangHostEnvironment::new(
        recovery_echo_catalog(),
        lashlang::LashlangAbilities::default(),
    );
    let module = b::module(
        vec![b::process(
            "main",
            Vec::new(),
            b::finish(b::receiver_call(
                b::resource(&["tools"]),
                "recovery_echo",
                vec![b::record(vec![("line", b::string("once"))])],
            )),
        )],
        Vec::new(),
    );
    let linked = lashlang::LinkedModule::link(module, &environment).expect("link recovery process");
    artifact_store
        .publish_module_artifact(&ArtifactOwner::host("fig3463-recovery"), &linked.artifact)
        .await
        .expect("publish recovery process");
    let process_input = LashlangProcessInput {
        module_ref: linked.artifact.module_ref().clone(),
        process_ref: linked
            .artifact
            .process_ref("main")
            .expect("main process")
            .clone(),
        host_requirements_ref: linked.artifact.host_requirements_ref().clone(),
        process_name: "main".to_string(),
        args: serde_json::Map::new(),
    };
    let process_identity = process_input.process_identity();
    // A file backend: the law rewrites a row of its effect journal on disk.
    let backend_dir = tempfile::tempdir().expect("backend directory");
    let journal_path = backend_dir
        .path()
        .join(lash_sqlite_store::SqliteDatabase::EffectReplay.file_name());
    let sqlite_backend = lash_sqlite_store::SqliteBackend::open(backend_dir.path())
        .await
        .expect("open a SQLite file backend");
    let backend: lash_core::Backend = Arc::new(sqlite_backend.clone()).into();
    let env_store: Arc<dyn ProcessExecutionEnvStore> = backend.process_env_store();
    let env_ref = lash_core::runtime::publish_process_execution_env(
        env_store.as_ref(),
        &ArtifactOwner::host("fig3463-recovery-env"),
        &ProcessExecutionEnvSpec::new(PluginOptions::empty(), session_policy()),
    )
    .await
    .expect("publish process env");
    let registry: Arc<dyn ProcessRegistry> = backend.process_registry();
    let graphs = Arc::new(lash_trace::TraceLashlangGraphStore::default());
    let crash_sink = Arc::new(CrashAfterFirstNodeCompleted {
        graphs: Arc::clone(&graphs),
        crashed: std::sync::atomic::AtomicBool::new(false),
        notified: tokio::sync::Notify::new(),
    });
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // The retry is a second worker over the same backend: a fresh handle on
    // the crashed worker's databases.
    let backend_b: lash_core::Backend = Arc::new(
        sqlite_backend
            .reopen()
            .await
            .expect("reopen the backend for the retry"),
    )
    .into();
    let tool_factory: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "fig3463-recovery-echo",
            PluginSpec::new().with_tool_provider(Arc::new(RecoveryEchoTool {
                executions: Arc::clone(&executions),
            })),
        ));
    let worker = |sink: Arc<dyn lash_trace::TraceSink>, backend: lash_core::Backend| {
        let engine = LashlangProcessEngine::new(artifact_store.clone(), LashlangSurface::default())
            .with_execution_trace(Some(sink), lash_trace::TraceContext::default());
        let runtime_host = RuntimeHostConfig::new(
            backend,
            CommitBudget::bounded(1024 * 1024, 512),
            QueuedWorkBatchingConfig::new(1),
        )
        .with_lease_timings(
            lash_core::facade_support::LeaseTimings::from_ttl(std::time::Duration::from_millis(
                120,
            ))
            .expect("short crash lease"),
        )
        .with_process_engine_registration(lashlang_process_engine_registration(engine));
        let mut factories = lash_core::testing::test_code_protocol_factories();
        factories.push(Arc::clone(&tool_factory));
        DurableProcessWorker::new(
            DurableProcessWorkerConfig::new(
                Arc::new(PluginHost::new(factories)),
                runtime_host,
                WorkerProcessWork::SelfNative(watch_process_registry(Arc::clone(&registry))),
                Arc::new(NoSessionWork::new()),
                lash_core::testing::runtime_lease_owner(),
            )
            .with_session_policy(session_policy()),
        )
        .expect("recovery worker")
    };
    let process_id = registry
        .register_process(
            ProcessRegistration::new(
                process_input.into_process_input().expect("process input"),
                RecoveryContract::Rerunnable,
                ProcessProvenance::host(),
                ProcessLifecyclePolicy::new(ParentScope::Host, OnParentEnd::Abandon),
            )
            .with_admitted_identity(AdmittedProcessIdentity::for_testing(process_identity))
            .with_execution_env_ref(Some(env_ref))
            // The crashed attempt spends one of two; every refused sweep
            // after it runs past the budget, which a park does not spend.
            .with_max_attempts(Some(2)),
        )
        .await
        .expect("register recovery process")
        .id;
    let worker_a = worker(crash_sink.clone(), backend.clone());
    let first_report = worker_a
        .drive_pending_processes()
        .await
        .expect("admit first attempt");
    if tokio::time::timeout(
        std::time::Duration::from_secs(5),
        crash_sink.notified.notified(),
    )
    .await
    .is_err()
    {
        panic!(
            "first attempt must crash after NodeCompleted: report={first_report:?}, process={:?}, graphs={:?}",
            registry.get_process(&process_id).await,
            graphs.graphs()
        );
    }
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the effect must already be journaled before the crash"
    );
    drop(worker_a);
    // The journal now holds another command at the body's first ordinal: the
    // recorded tool attempt's row is re-keyed as a journaled value, as a
    // build whose body issued a value there would have written it.
    let journal = rusqlite::Connection::open(&journal_path).expect("open the journal");
    let rewritten = journal
        .execute(
            "UPDATE runtime_effect_replay
                SET replay_key = substr(replay_key, 1, length(replay_key) - length(':attempt:1'))
              WHERE replay_key LIKE '%:lk2:0000000000:attempt:1'",
            [],
        )
        .expect("rewrite the recorded tool attempt");
    assert_eq!(
        rewritten, 1,
        "the crashed attempt journaled its tool attempt"
    );
    drop(journal);
    let worker_b = worker(graphs.clone(), backend_b);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    while std::time::Instant::now() < deadline {
        let _ = worker_b.drive_pending_processes().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // The park stays on the record across every sweep's re-run (FIG-3659
    // NOW-B): a re-run only stops it refusing until it refuses again. The
    // last sweep's re-run may still be in flight once driving stops: read the
    // record once that re-run has refused again.
    let settle_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let record = loop {
        let record = registry
            .get_process(&process_id)
            .await
            .expect("read the refused process")
            .expect("the process is still registered");
        if record.is_refusing_park()
            || record.is_terminal()
            || std::time::Instant::now() >= settle_deadline
        {
            break record;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert!(
        !record.is_terminal(),
        "a refused body is parked, never settled: {record:?}"
    );
    let park = record
        .park
        .as_deref()
        .expect("the refusal is recorded as the process's park");
    assert!(park.refusing, "the settled re-run refused again: {park:?}");
    assert!(
        park.attempts >= 2,
        "every refused sweep re-parks the same park and counts it: {park:?}"
    );
    // One park, however many sweeps refused: the feed opened it once and
    // never closed it, so there is no clear-then-rewrite flicker.
    let feed = registry
        .process_park_feed(
            lash_core::store::ParkFeedCursor::initial(),
            std::num::NonZeroUsize::new(64).expect("non-zero"),
        )
        .await
        .expect("read the process park feed");
    let transitions = feed
        .events
        .iter()
        .filter(|event| event.target == process_id)
        .map(|event| (event.park_id, event.kind.kind_code()))
        .collect::<Vec<_>>();
    assert_eq!(
        transitions,
        vec![(park.park_id, "parked")],
        "a re-parked process writes exactly one `Parked` event"
    );
    let attempt = record
        .first_started
        .as_deref()
        .map(|started| started.attempt)
        .expect("the process started");
    assert!(
        attempt > 2,
        "the parked process kept being re-run past its attempt budget without being \
         abandoned: attempt {attempt}"
    );
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the redrive dispatches nothing"
    );
}

#[tokio::test]
async fn fig3463_process_scalar_and_batch_failures_keep_the_recorded_effect_provenance() {
    let call = || {
        b::receiver_call(
            b::resource(&["tools"]),
            "recovery_echo",
            vec![b::record(vec![("line", b::string("deny"))])],
        )
    };
    let module = b::module(
        vec![
            b::process("scalar", Vec::new(), b::finish(b::unwrap(call()))),
            b::process(
                "batch",
                Vec::new(),
                b::finish(b::await_expr(b::list(vec![b::unwrap(call())]))),
            ),
        ],
        Vec::new(),
    );
    let artifact_store: LashlangArtifacts = crate::lib_tests::memory_artifact_store().await;
    let linked = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            recovery_echo_catalog(),
            lashlang::LashlangAbilities::default(),
        ),
    )
    .expect("link failing process calls");
    artifact_store
        .publish_module_artifact(&ArtifactOwner::host("fig3463-failures"), &linked.artifact)
        .await
        .expect("publish failing processes");
    let sqlite_backend = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open a SQLite memory backend");
    let backend: lash_core::Backend = Arc::new(sqlite_backend.clone()).into();
    let env_store: Arc<dyn ProcessExecutionEnvStore> = backend.process_env_store();
    let env_ref = lash_core::runtime::publish_process_execution_env(
        env_store.as_ref(),
        &ArtifactOwner::host("fig3463-failures-env"),
        &ProcessExecutionEnvSpec::new(PluginOptions::empty(), session_policy()),
    )
    .await
    .expect("publish failing process env");
    let registry: Arc<dyn ProcessRegistry> = backend.process_registry();
    let graphs = Arc::new(lash_trace::TraceLashlangGraphStore::default());
    let engine = LashlangProcessEngine::new(artifact_store, LashlangSurface::default())
        .with_execution_trace(Some(graphs.clone()), lash_trace::TraceContext::default());
    let runtime_host = RuntimeHostConfig::new(
        backend.clone(),
        CommitBudget::bounded(1024 * 1024, 512),
        QueuedWorkBatchingConfig::new(1),
    )
    .with_process_engine_registration(lashlang_process_engine_registration(engine));
    let tool_factory: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "fig3463-failure-tool",
            PluginSpec::new().with_tool_provider(Arc::new(RecoveryEchoTool {
                executions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            })),
        ));
    let mut factories = lash_core::testing::test_code_protocol_factories();
    factories.push(tool_factory);
    let worker = DurableProcessWorker::new(
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(factories)),
            runtime_host,
            WorkerProcessWork::SelfNative(watch_process_registry(Arc::clone(&registry))),
            Arc::new(NoSessionWork::new()),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_policy(session_policy()),
    )
    .expect("failure worker");
    let mut process_ids = std::collections::BTreeMap::new();
    for name in ["scalar", "batch"] {
        let input = LashlangProcessInput {
            module_ref: linked.artifact.module_ref().clone(),
            process_ref: linked
                .artifact
                .process_ref(name)
                .expect("process ref")
                .clone(),
            host_requirements_ref: linked.artifact.host_requirements_ref().clone(),
            process_name: name.to_owned(),
            args: serde_json::Map::new(),
        };
        let identity = input.process_identity();
        let process_id = registry
            .register_process(
                ProcessRegistration::new(
                    input.into_process_input().expect("process input"),
                    RecoveryContract::Rerunnable,
                    ProcessProvenance::host(),
                    ProcessLifecyclePolicy::new(ParentScope::Host, OnParentEnd::Abandon),
                )
                .with_admitted_identity(AdmittedProcessIdentity::for_testing(identity))
                .with_execution_env_ref(Some(env_ref.clone())),
            )
            .await
            .expect("register failure process")
            .id;
        process_ids.insert(name, process_id);
    }
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("drive failed calls");
    for name in ["scalar", "batch"] {
        let process_id = &process_ids[name];
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            NativeProcessWork::for_registry(Arc::clone(&registry)).await_terminal(process_id),
        )
        .await
        .expect("failed process settles")
        .expect("await failed process");
        let graph = graphs
            .graphs()
            .into_iter()
            .find(|graph| {
                graph.history.first().is_some_and(|record| {
                    matches!(&record.event.identity.subject,
                    lash_trace::TraceRuntimeSubject::Process { process_id: id } if id == process_id)
                })
            })
            .expect("failed process graph");
        let (call_id, failure) = graph
            .history
            .iter()
            .find_map(|record| match &record.event.payload {
                lash_trace::TraceLanguageExecutionPayload::NodeFailed {
                    call_id: Some(call_id),
                    failure:
                        lash_trace::TraceLanguageExecutionFailure::Effect {
                            replay_key,
                            class,
                            code,
                            source,
                            retry,
                            ..
                        },
                    ..
                } => Some((call_id, (replay_key, class, code, source, retry))),
                _ => None,
            })
            .unwrap_or_else(|| {
                panic!(
                    "failed {name} leaf retains typed effect provenance: {:?}",
                    graph
                        .history
                        .iter()
                        .map(|record| &record.event.payload)
                        .collect::<Vec<_>>()
                )
            });
        // The leaf's call id and its recorded replay key name the same issue
        // ordinal: the key under the body's `lk2` namespace (FIG-3586).
        assert_eq!(
            call_id.replacen(":0000000000", ":lk2:0000000000", 1),
            *failure.0,
            "leaf owns the recorded replay key"
        );
        assert_eq!(*failure.1, lash_core::ToolFailureClass::PermissionDenied);
        assert_eq!(failure.2, "approval_denied");
        assert_eq!(*failure.3, lash_core::ToolFailureSource::Policy);
        assert_eq!(
            *failure.4,
            lash_core::ToolRetryStatus::Exhausted { attempts: 3 }
        );
    }
}
