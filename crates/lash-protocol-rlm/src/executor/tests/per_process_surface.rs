use std::sync::Arc;

use lash_core::facade_support::{
    InMemoryProcessExecutionEnvStore, InMemorySessionStoreFactory, NativeEffectHost, PluginHost,
    PluginSessionContext, PluginSpec, PluginSpecFactory, RuntimeHostConfig,
    empty_trigger_source_key, watch_process_registry,
};
use lash_core::{
    ArtifactOwner, CommitBudget, LashSchema, NativeProcessWork, NoQueuedWork, PluginError,
    PluginOptions, ProcessExecutionEnvSpec, ProcessExecutionEnvStore, ProcessOriginator,
    ProcessRegistry, QueuedWorkBatchingConfig, SessionPolicy, TriggerCommand,
    TriggerCommandOutcome, TriggerOccurrenceRequest, TriggerOwnerScope, TriggerStore,
    TriggerSubscriptionDraft, TurnBudget,
};
use lash_core_worker::{DurableProcessWorker, DurableProcessWorkerConfig, WorkerProcessWork};
use lash_lashlang_runtime::{
    LashlangProcessEngine, LashlangProcessInput, LashlangSurface, LashlangSurfaceContribution,
    lashlang_process_engine_registration, lashlang_surface_extension,
};

const SURFACE_PLUGIN_ID: &str = "fig3344.trigger-surface";
const SOURCE_TYPE: &str = "ui.button.pressed";

#[derive(serde::Deserialize, serde::Serialize)]
struct SessionSurfaceOptions {
    grant_vocabulary: bool,
}

fn session_surface_resources() -> lashlang::LashlangHostCatalog {
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_named_data_type(
            lashlang::NamedDataType::object(
                "ui.ButtonPressed",
                vec![lashlang::TypeField {
                    name: "colour".into(),
                    ty: lashlang::TypeExpr::Str,
                    optional: false,
                }],
            )
            .expect("valid button event type"),
        )
        .expect("button event type is unique");
    resources
}

fn session_surface_contribution() -> LashlangSurfaceContribution {
    LashlangSurfaceContribution::new(
        lashlang::LashlangAbilities::default(),
        lashlang::LashlangLanguageFeatures::default(),
        session_surface_resources(),
    )
}

fn session_surface_factory() -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(PluginSpecFactory::new(
        SURFACE_PLUGIN_ID,
        Arc::new(|ctx: &PluginSessionContext| {
            let granted = ctx
                .plugin_options
                .decode::<SessionSurfaceOptions>(SURFACE_PLUGIN_ID)
                .map_err(|error| {
                    PluginError::Registration(format!("invalid session surface options: {error}"))
                })?
                .is_some_and(|options| options.grant_vocabulary);
            let spec = if granted {
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

fn session_policy() -> SessionPolicy {
    SessionPolicy {
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("trigger surface test model"),
        ..SessionPolicy::new(TurnBudget::Unbounded)
    }
}

fn plugin_options() -> PluginOptions {
    PluginOptions::typed(
        SURFACE_PLUGIN_ID,
        SessionSurfaceOptions {
            grant_vocabulary: true,
        },
    )
    .expect("session surface options encode")
}

/// `const requires = async (event: ui.ButtonPressed) => true;`
/// `const main = async () => "ok";`
///
/// `main` is the subscription target; `requires` is never executed but makes
/// the module's host requirements carry the event data type.
const MODULE_SOURCE: &str = r#"
const requires = async (event: ui.ButtonPressed) => true;
const main = async () => "ok";
"#;

#[tokio::test]
async fn trigger_fired_process_runs_under_session_contributed_event_type() {
    let artifact_store: Arc<dyn lashlang::LashlangArtifactStore> =
        Arc::new(lashlang::InMemoryLashlangArtifactStore::new());
    let factory = Arc::new(crate::RlmProtocolPluginFactory::new(
        crate::RlmProtocolPluginConfig::builder()
            .channel(crate::RlmChannel::Cell)
            .instruction_limit(crate::InstructionBound::instructions(1_000_000))
            .wall_clock(crate::WallClockBound::secs(30))
            .memory_limit(crate::MemoryBound::mebibytes(64))
            .build(),
        Arc::clone(&artifact_store),
    ));
    let plugin_host = PluginHost::new(vec![
        Arc::clone(&factory) as Arc<dyn lash_core::facade_support::PluginFactory>,
        session_surface_factory(),
    ]);

    let env_spec = ProcessExecutionEnvSpec::new(plugin_options(), session_policy());
    let surface = factory
        .lashlang_compile_surface(
            &plugin_host,
            true,
            crate::LashlangCompileSurfaceRequest::new("fig3344-trigger-compile", env_spec.clone()),
        )
        .expect("compile surface composes the session contribution");
    assert!(
        surface
            .host_environment
            .resources
            .resolve_named_data_type("ui.ButtonPressed")
            .is_some(),
        "the per-process contribution must reach the compile surface"
    );

    let compiled = factory
        .compile_lashlang_module(
            &plugin_host,
            true,
            crate::LashlangModuleCompileRequest::new(
                "fig3344-trigger-compile",
                MODULE_SOURCE,
                env_spec,
            ),
        )
        .expect("module compiles against the session-contributed surface");
    factory
        .publish_lashlang_module(&ArtifactOwner::host("fig3344-trigger"), &compiled.artifact)
        .await
        .expect("module artifact publishes");

    let target = compiled
        .introspection
        .exported_processes
        .iter()
        .find(|process| process.params.is_empty())
        .expect("the target process takes no arguments");
    let process_name = target.definition.process_name.clone();

    let process_input = LashlangProcessInput {
        module_ref: compiled.module_ref.clone(),
        process_ref: compiled
            .artifact
            .process_ref(&process_name)
            .expect("target process ref")
            .clone(),
        host_requirements_ref: compiled.host_requirements_ref.clone(),
        process_name,
        args: serde_json::Map::new(),
    };

    let env_store: Arc<dyn ProcessExecutionEnvStore> =
        Arc::new(InMemoryProcessExecutionEnvStore::new());
    let env_ref = lash_core::runtime::publish_process_execution_env(
        env_store.as_ref(),
        &ArtifactOwner::host("fig3344-trigger-env"),
        &ProcessExecutionEnvSpec::new(plugin_options(), session_policy()),
    )
    .await
    .expect("process execution env publishes");

    let trigger_store: Arc<dyn TriggerStore> =
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let source_key = empty_trigger_source_key(SOURCE_TYPE).expect("source key derives");
    let draft = TriggerSubscriptionDraft::for_process(
        "fig3344-fired",
        env_ref,
        SOURCE_TYPE,
        source_key.clone(),
        process_input
            .clone()
            .into_process_input()
            .expect("process input encodes"),
        process_input.process_identity(),
    )
    .with_payload_schema(LashSchema::any());
    let registration = trigger_store
        .execute_command(
            "fig3344-trigger-register",
            TriggerCommand::Register {
                owner_scope: TriggerOwnerScope::host("fig3344-trigger")
                    .expect("host trigger owner scope"),
                actor: ProcessOriginator::host_scoped("fig3344-trigger"),
                draft,
            },
        )
        .await
        .expect("register trigger command executes")
        .expect("trigger subscription registers");
    assert!(
        matches!(registration, TriggerCommandOutcome::Mutation { .. }),
        "trigger registration mutates the store"
    );

    let registry: Arc<dyn ProcessRegistry> =
        Arc::new(lash_core::TestLocalProcessRegistry::default());
    let engine = LashlangProcessEngine::new(
        Arc::clone(&artifact_store),
        LashlangSurface::new(
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangLanguageFeatures::default(),
            lashlang::LashlangHostCatalog::new(),
        ),
    );
    let runtime_host = RuntimeHostConfig::new(
        Arc::new(NativeEffectHost::default()),
        Arc::new(lash_core::facade_support::InMemoryAttachmentStore::new()),
        Arc::clone(&env_store),
        CommitBudget::bounded(1024 * 1024, 512),
        QueuedWorkBatchingConfig::new(1),
    )
    .with_process_engine_registration(lashlang_process_engine_registration(engine));
    let watched = watch_process_registry(Arc::clone(&registry));
    let worker = DurableProcessWorker::new(
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(vec![
                Arc::clone(&factory) as Arc<dyn lash_core::facade_support::PluginFactory>,
                session_surface_factory(),
            ])),
            runtime_host,
            Arc::new(InMemorySessionStoreFactory::new()),
            WorkerProcessWork::SelfNative(watched),
            Arc::new(NoQueuedWork::new()),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_trigger_store(Arc::clone(&trigger_store))
        .with_session_policy(session_policy()),
    )
    .expect("valid trigger surface worker");

    let ingress = trigger_store
        .ingest_occurrence(TriggerOccurrenceRequest::new(
            SOURCE_TYPE,
            source_key,
            serde_json::json!({ "colour": "blue" }),
            "fig3344-trigger-fire",
        ))
        .await
        .expect("ingest trigger occurrence");
    let delivery = ingress
        .reservations
        .into_iter()
        .next()
        .expect("occurrence reserves one delivery");

    let _report = worker
        .drive_pending_processes()
        .await
        .expect("worker reconciles and drives the trigger delivery");
    let terminal = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        NativeProcessWork::for_registry(Arc::clone(&registry)).await_terminal(&delivery.process_id),
    )
    .await
    .expect("trigger delivery reaches terminal state")
    .expect("await trigger delivery");
    assert!(
        matches!(
            terminal,
            lash_core::ProcessAwaitOutput::Settled { ref output } if output.is_success()
        ),
        "a trigger-fired process must run under the session-contributed event type: {terminal:?}"
    );
}
