use super::*;
use std::sync::Arc;

use lash_core::facade_support::{
    DurableProcessWorker, DurableProcessWorkerConfig, InMemoryProcessExecutionEnvStore,
    InMemorySessionStoreFactory, PluginHost, PluginSessionContext, PluginSpec, PluginSpecFactory,
    RuntimeHostConfig, watch_process_registry,
};
use lash_core::{
    AdmittedProcessIdentity, ArtifactOwner, CommitBudget, NativeProcessWork, NoQueuedWork,
    OnParentEnd, ParentScope, PluginError, PluginOptions, ProcessExecutionEnvSpec,
    ProcessExecutionEnvStore, ProcessLifecyclePolicy, ProcessProvenance, ProcessRegistration,
    ProcessRegistry, QueuedWorkBatchingConfig, RecoveryContract, SessionPolicy, TurnBudget,
    WorkerProcessWork,
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
    let artifact_store: Arc<dyn LashlangArtifactStore> =
        Arc::new(InMemoryLashlangArtifactStore::new());
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
        module_ref: linked.module_ref.clone(),
        process_ref: linked
            .artifact
            .process_ref("main")
            .expect("main process ref")
            .clone(),
        host_requirements_ref: linked.host_requirements_ref.clone(),
        process_name: "main".to_string(),
        args: serde_json::Map::new(),
    };
    let process_identity = process_input.process_identity();
    let process_id = lash_sansio::ProcessId::from("fig3344-session-surface-process");

    let env_store: Arc<dyn ProcessExecutionEnvStore> =
        Arc::new(InMemoryProcessExecutionEnvStore::new());
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

    let registry: Arc<dyn ProcessRegistry> =
        Arc::new(lash_core::TestLocalProcessRegistry::default());
    let watched = watch_process_registry(Arc::clone(&registry));
    let engine = LashlangProcessEngine::new(
        Arc::clone(&artifact_store),
        LashlangSurface::new(
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangLanguageFeatures::default(),
            lashlang::LashlangHostCatalog::new(),
        ),
    );
    let runtime_host = RuntimeHostConfig::new(
        Arc::new(lash_core::facade_support::NativeEffectHost::default()),
        Arc::new(lash_core::facade_support::InMemoryAttachmentStore::new()),
        Arc::clone(&env_store),
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
            Arc::new(InMemorySessionStoreFactory::new()),
            WorkerProcessWork::SelfNative(watched),
            Arc::new(NoQueuedWork::new()),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_policy(session_policy()),
    )
    .expect("valid session surface worker");

    let registration = ProcessRegistration::new(
        process_id.clone(),
        process_input
            .into_process_input()
            .expect("process input encodes"),
        RecoveryContract::Rerunnable,
        ProcessProvenance::host(),
        ProcessLifecyclePolicy::new(ParentScope::Host, OnParentEnd::Abandon),
    )
    .with_admitted_identity(AdmittedProcessIdentity::for_testing(process_identity))
    .with_execution_env_ref(Some(env_ref));
    registry
        .register_process(registration)
        .await
        .expect("process registers");
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
