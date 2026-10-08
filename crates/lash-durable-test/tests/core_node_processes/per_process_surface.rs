//! Per-process surface: a process runs under the event types its
//! environment's plugin config contributes.

use super::*;

const SURFACE_PLUGIN_ID: &str = "fig3344.trigger-surface";
const BUTTON_SOURCE: &str = "ui.button.pressed";

/// The plugin that contributes the `ui.ButtonPressed` event type to a
/// session, or a process, whose plugin config grants its vocabulary.
fn session_surface_factory() -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(lash_core::facade_support::PluginSpecFactory::new(
        lash_core::plugin::PluginDeclaration::initial(SURFACE_PLUGIN_ID),
        Arc::new(|ctx: &lash_core::facade_support::PluginSessionContext| {
            let granted = ctx
                .plugin_config
                .decode::<serde_json::Value>(SURFACE_PLUGIN_ID)
                .map_err(|error| {
                    lash_core::PluginError::Registration(format!(
                        "invalid session surface options: {error}"
                    ))
                })?
                .is_some_and(|options| options["grant_vocabulary"] == serde_json::json!(true));
            if !granted {
                return Ok(lash_core::facade_support::PluginSpec::new());
            }
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
                    .expect("a valid button event type"),
                )
                .expect("the button event type is unique");
            let contribution = lash_lashlang_runtime::lashlang_surface_extension(
                &lash_lashlang_runtime::LashlangSurfaceContribution::new(
                    lashlang::LashlangAbilities::default(),
                    lashlang::LashlangLanguageFeatures::default(),
                    resources,
                ),
            )
            .map_err(|error| lash_core::PluginError::Registration(error.to_string()))?;
            Ok(lash_core::facade_support::PluginSpec::new()
                .with_extension_contribution(contribution))
        }),
    ))
}

/// The environment a granted process runs in.
fn granted_environment() -> lash_core::ProcessExecutionEnvSpec {
    let mut config = lash_core::PluginConfig::default();
    config.insert(
        SURFACE_PLUGIN_ID,
        serde_json::json!({ "grant_vocabulary": true }),
    );
    lash_core::ProcessExecutionEnvSpec {
        plugin_config: lash_core::AdmittedPluginConfig::new(config, 0),
        ..environment()
    }
}

/// `requires` is never run; it makes the module's host requirements carry
/// the session-contributed event type. `main` is the subscription's target.
const BUTTON_MODULE: &str = r#"
const requires = async (event: ui.ButtonPressed) => true;
const main = async () => "ok";
"#;

/// A process a trigger occurrence starts runs under the event type its
/// environment's plugin config contributes: the module compiles against the
/// contributed surface, the started process records the type in its
/// resource catalog at creation, and the core's node runs it to success
/// (FIG-3344, ported by FIG-5307 from the deleted lash-protocol-rlm
/// `per_process_surface.rs`).
async fn trigger_fired_process_runs_under_session_contributed_event_type(tier: Tier) {
    let deployment = deploy_with(tier, Vec::new(), |backend| {
        rlm_core(backend).plugin(session_surface_factory())
    })
    .await;
    let core = &deployment.core;
    let compiler = {
        use lash::rlm::Dialect as _;
        lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                .build(),
            Arc::new(lash::rlm::TypescriptDialect),
            &deployment.backend,
        )
        .with_worker_service(lash::rlm::TypescriptDialect.worker_service())
    };
    let compiler = Arc::new(compiler);
    let plugin_host = lash_core::facade_support::PluginHost::new(vec![
        Arc::clone(&compiler) as Arc<dyn lash_core::facade_support::PluginFactory>,
        session_surface_factory(),
    ]);
    let compiled = compiler
        .compile_lashlang_module(
            &plugin_host,
            true,
            lash::rlm::LashlangModuleCompileRequest::new(
                "fig3344-trigger-compile",
                BUTTON_MODULE,
                granted_environment(),
            ),
        )
        .await
        .expect("the module compiles against the session-contributed surface");
    compiler
        .artifact_store()
        .publish_module_artifact(
            &lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
                lash_core::HostArtifactPin::mint(),
            ))
            .expect("a host pin is unguarded"),
            &compiled.artifact,
        )
        .await
        .expect("the module publishes");
    let target = compiled
        .introspection
        .exported_processes
        .iter()
        .find(|process| process.params.is_empty())
        .expect("the target process takes no arguments");
    let input = lash_lashlang_runtime::LashlangProcessInput {
        module_ref: compiled.module_ref.clone(),
        process_ref: target.definition.process_ref.clone(),
        host_requirements_ref: compiled.host_requirements_ref.clone(),
        process_name: compiled
            .artifact
            .process_name_for_ref(&target.definition.process_ref)
            .expect("the target process export")
            .to_owned(),
        args: serde_json::Map::new(),
    };
    let env_ref = core
        .host_artifacts()
        .publish_process_env(&lash_core::HostArtifactPin::mint(), &granted_environment())
        .await
        .expect("the environment is published");
    let source_key =
        lash_core::facade_support::empty_trigger_source_key(BUTTON_SOURCE).expect("a source key");
    deployment
        .backend
        .trigger_store()
        .execute_command(
            "fig3344-trigger-register",
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::host("fig3344-trigger")
                    .expect("a host scope"),
                actor: lash_core::ProcessOriginator::host_scoped("fig3344-trigger"),
                draft: lash_core::TriggerSubscriptionDraft::for_process(
                    "fig3344-fired",
                    env_ref,
                    BUTTON_SOURCE,
                    source_key.clone(),
                    input
                        .clone()
                        .into_process_input()
                        .expect("the input encodes"),
                    input.process_identity(),
                )
                .with_payload_schema(lash_sansio::JsonSchema::any()),
            },
        )
        .await
        .expect("the subscription is registered")
        .expect("the subscription is admitted");
    let report = core
        .triggers()
        .emit(
            lash_core::TriggerOccurrenceRequest::new(
                BUTTON_SOURCE,
                source_key,
                serde_json::json!({ "colour": "blue" }),
                "fig3344-trigger-fire",
            ),
            core.effect_host(),
        )
        .await
        .expect("the occurrence is emitted");
    let started = report.started_process_ids();
    assert_eq!(
        started.len(),
        1,
        "the occurrence started its process: {report:?}"
    );
    let record = deployment
        .backend
        .process_registry()
        .get_process(&started[0])
        .await
        .expect("the started process is read")
        .expect("the started process exists");
    let resources: lashlang::LashlangHostCatalog = serde_json::from_value(
        record
            .engine_config
            .as_ref()
            .and_then(|config| config.get("resources"))
            .expect("the process records its resource catalog")
            .clone(),
    )
    .expect("the recorded catalog decodes");
    assert!(
        resources
            .resolve_named_data_type("ui.ButtonPressed")
            .is_some(),
        "the event type is captured at creation"
    );
    assert_eq!(
        success(&ended(core, &started[0]).await),
        serde_json::json!("ok")
    );
}

on_every_tier!(trigger_fired_process_runs_under_session_contributed_event_type);
