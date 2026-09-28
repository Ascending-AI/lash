use super::*;
use std::sync::Arc;

use lash_core::facade_support::{PluginSessionContext, PluginSpec, PluginSpecFactory};
use lash_core::{
    AdmittedProcessIdentity, ArtifactOwner, Lifetime, PluginError, PluginOptions,
    ProcessExecutionEnvSpec, ProcessExecutionEnvStore, ProcessProvenance, ProcessRegistration,
    ProcessRegistry, SessionPolicy, TurnBudget,
};
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

    let harness = crate::lib_tests::double_process_harness().await;
    let backend = harness.backend().clone();
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

    let engine = LashlangProcessEngine::new(
        artifact_store.clone(),
        LashlangSurface::new(
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangLanguageFeatures::default(),
            lashlang::LashlangHostCatalog::new(),
        ),
    );
    harness.install_lashlang_worker(engine, vec![surface_plugin_factory()]);

    let registration = ProcessRegistration::new(
        process_input
            .into_process_input()
            .expect("process input encodes"),
        ProcessProvenance::host(),
        Lifetime::Detached,
    )
    .with_admitted_identity(AdmittedProcessIdentity::for_testing(process_identity))
    .with_execution_env_ref(Some(env_ref));
    let process_id = harness.admit(registration).await;

    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        harness.await_terminal(&process_id),
    )
    .await
    .expect("process reaches terminal state")
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
async fn fig3463_a_crashed_segment_replays_its_journaled_effect_under_one_attempt() {
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
    let harness = crate::lib_tests::double_process_harness().await;
    let backend = harness.backend().clone();
    let env_store: Arc<dyn ProcessExecutionEnvStore> = backend.process_env_store();
    let env_ref = lash_core::runtime::publish_process_execution_env(
        env_store.as_ref(),
        &ArtifactOwner::host("fig3463-recovery-env"),
        &ProcessExecutionEnvSpec::new(PluginOptions::empty(), session_policy()),
    )
    .await
    .expect("publish process env");
    let registry: Arc<dyn ProcessRegistry> = harness.registry();
    let graphs = Arc::new(lash_trace::TraceLashlangGraphStore::default());
    let crash_sink = Arc::new(CrashAfterFirstNodeCompleted {
        graphs: Arc::clone(&graphs),
        crashed: std::sync::atomic::AtomicBool::new(false),
        notified: tokio::sync::Notify::new(),
    });
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tool_factory: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "fig3463-recovery-echo",
            PluginSpec::new().with_tool_provider(Arc::new(RecoveryEchoTool {
                executions: Arc::clone(&executions),
            })),
        ));
    // The first attempt's segment dies after its effect completed; the
    // engine retries the segment on the same deployment, whose sink no
    // longer crashes.
    let crash: Arc<dyn lash_trace::TraceSink> = crash_sink.clone();
    let engine = LashlangProcessEngine::new(artifact_store.clone(), LashlangSurface::default())
        .with_execution_trace(Some(crash), lash_trace::TraceContext::default());
    harness.install_lashlang_worker(engine, vec![tool_factory]);
    let process_id = harness
        .admit(
            ProcessRegistration::new(
                process_input.into_process_input().expect("process input"),
                ProcessProvenance::host(),
                Lifetime::Detached,
            )
            .with_admitted_identity(AdmittedProcessIdentity::for_testing(process_identity))
            .with_execution_env_ref(Some(env_ref)),
        )
        .await;
    if tokio::time::timeout(
        std::time::Duration::from_secs(30),
        crash_sink.notified.notified(),
    )
    .await
    .is_err()
    {
        panic!(
            "first attempt must crash after NodeCompleted: process={:?}, graphs={:?}",
            registry.get_process(&process_id).await,
            graphs.graphs()
        );
    }
    let terminal = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        harness.await_terminal(&process_id),
    )
    .await
    .expect("retry reaches terminal");
    assert!(
        matches!(terminal, lash_core::ProcessAwaitOutput::Settled { ref output } if output.is_success())
    );
    assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 1);
    // The engine resumes the crashed segment by replaying its journal
    // (ADR 0110): the retry is the same lash attempt, and the replayed
    // resource node names the effect key the crashed run journaled.
    let graphs = graphs.graphs();
    assert!(!graphs.is_empty(), "the process's run is traced");
    let attempts = graphs
        .iter()
        .flat_map(|graph| graph.history.iter())
        .map(|record| record.event.identity.attempt().expect("process attempt"))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        attempts,
        std::collections::BTreeSet::from([1]),
        "a replayed segment is not a new attempt"
    );
    let call_ids = graphs
        .iter()
        .flat_map(|graph| graph.history.iter())
        .filter_map(|record| match &record.event.payload {
            lash_trace::TraceLanguageExecutionPayload::NodeStarted {
                call_id: Some(call_id),
                ..
            } => Some(call_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !call_ids.is_empty(),
        "the resource node records its effect key"
    );
    assert!(
        call_ids.iter().all(|call_id| *call_id == call_ids[0]),
        "the replay must reuse the journaled effect key: {call_ids:?}"
    );
    assert!(!call_ids[0].contains(":attempt:"));
    assert!(
        graphs
            .iter()
            .any(|graph| graph.nodes.iter().any(|node| matches!(
                node.observation,
                lash_trace::TraceLashlangNodeObservation::Completed { occurrence: 1, .. }
            )))
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
    let harness = crate::lib_tests::double_process_harness().await;
    let backend = harness.backend().clone();
    let env_store: Arc<dyn ProcessExecutionEnvStore> = backend.process_env_store();
    let env_ref = lash_core::runtime::publish_process_execution_env(
        env_store.as_ref(),
        &ArtifactOwner::host("fig3463-failures-env"),
        &ProcessExecutionEnvSpec::new(PluginOptions::empty(), session_policy()),
    )
    .await
    .expect("publish failing process env");
    let registry: Arc<dyn ProcessRegistry> = harness.registry();
    let graphs = Arc::new(lash_trace::TraceLashlangGraphStore::default());
    let engine = LashlangProcessEngine::new(artifact_store, LashlangSurface::default())
        .with_execution_trace(Some(graphs.clone()), lash_trace::TraceContext::default());
    let tool_factory: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "fig3463-failure-tool",
            PluginSpec::new().with_tool_provider(Arc::new(RecoveryEchoTool {
                executions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            })),
        ));
    harness.install_lashlang_worker(engine, vec![tool_factory]);
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
                    ProcessProvenance::host(),
                    Lifetime::Detached,
                )
                .with_admitted_identity(AdmittedProcessIdentity::for_testing(identity))
                .with_execution_env_ref(Some(env_ref.clone())),
            )
            .await
            .expect("register failure process")
            .id;
        process_ids.insert(name, process_id);
    }
    for process_id in process_ids.values() {
        harness.deliver_start(process_id).await;
    }
    for name in ["scalar", "batch"] {
        let process_id = &process_ids[name];
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            harness.await_terminal(process_id),
        )
        .await
        .expect("failed process settles");
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
