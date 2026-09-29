use std::sync::Arc;

use lash_core::facade_support::{
    PluginHost, PluginSessionContext, PluginSpec, PluginSpecFactory, RuntimeHostConfig,
    empty_trigger_source_key,
};
use lash_core::{
    CommitBudget, LashSchema, PluginError, PluginOptions, ProcessExecutionEnvSpec,
    ProcessExecutionEnvStore, ProcessOriginator, QueuedWorkBatchingConfig, SessionPolicy,
    TriggerCommand, TriggerCommandOutcome, TriggerOccurrenceRequest, TriggerOwnerScope,
    TriggerStore, TriggerSubscriptionDraft, TurnBudget,
};
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
    let table = crate::testing::DoubleProcesses::new(0x3344_0001).await;
    let backend = table.backend().clone();
    let artifact_store = lashlang::LashlangArtifacts::of_backend(&backend);
    let factory = Arc::new(crate::RlmProtocolPluginFactory::new(
        crate::RlmProtocolPluginConfig::builder()
            .channel(crate::RlmChannel::Cell)
            .instruction_limit(crate::InstructionBound::instructions(1_000_000))
            .memory_limit(crate::MemoryBound::mebibytes(64))
            .build(),
        &backend,
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
        .artifact_store()
        .publish_module_artifact(&host_pin_claim(), &compiled.artifact)
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

    let env_store: Arc<dyn ProcessExecutionEnvStore> = backend.process_env_store();
    let env_ref = lash_core::runtime::publish_process_execution_env(
        env_store.as_ref(),
        &host_pin_claim(),
        &ProcessExecutionEnvSpec::new(plugin_options(), session_policy()),
    )
    .await
    .expect("process execution env publishes");

    let trigger_store: Arc<dyn TriggerStore> = backend.trigger_store();
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

    let engine = || {
        LashlangProcessEngine::new(
            artifact_store.clone(),
            LashlangSurface::new(
                lashlang::LashlangAbilities::default(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            ),
        )
    };
    let runtime_host = RuntimeHostConfig::new(
        backend.clone(),
        CommitBudget::bounded(1024 * 1024, 512),
        QueuedWorkBatchingConfig::new(1),
    )
    .with_process_engine_registration(lashlang_process_engine_registration(engine()));
    let process_engines = runtime_host.process_engines.clone();
    table.install_worker(
        vec![
            Arc::clone(&factory) as Arc<dyn lash_core::facade_support::PluginFactory>,
            session_surface_factory(),
        ],
        runtime_host,
        session_policy(),
    );

    // The occurrence is emitted from a handler, as a deployment's tool
    // intent emits it: the router reserves the delivery and starts its
    // process under the handler's controller (ADR 0107).
    let router = lash_core::facade_support::TriggerRouter::new(
        Arc::clone(&trigger_store),
        backend.process_work(),
    )
    .with_process_artifacts(Arc::clone(&env_store), process_engines);
    let handler = table
        .open_handler(crate::testing::default_cell_scope())
        .await;
    let report = router
        .emit(
            TriggerOccurrenceRequest::new(
                SOURCE_TYPE,
                source_key,
                serde_json::json!({ "colour": "blue" }),
                "fig3344-trigger-fire",
            ),
            &handler.scoped(),
        )
        .await
        .expect("emit the trigger occurrence");
    handler.close().await.expect("close the emitting handler");
    let process_id = report
        .deliveries
        .into_iter()
        .find_map(|delivery| delivery.process_id)
        .expect("the occurrence's delivery started its process");
    let terminal = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        table.await_terminal(&process_id),
    )
    .await
    .expect("trigger delivery reaches terminal state");
    assert!(
        matches!(
            terminal,
            lash_core::ProcessAwaitOutput::Settled { ref output } if output.is_success()
        ),
        "a trigger-fired process must run under the session-contributed event type: {terminal:?}"
    );
}

/// A claim under a fresh host pin: the fixture publishes as a host would,
/// and never releases it.
fn host_pin_claim() -> lash_core::ReferrerClaim {
    lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
        lash_core::HostArtifactPin::mint(),
    ))
    .expect("a host pin is an unguarded referrer")
}
