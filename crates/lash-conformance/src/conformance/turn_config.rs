//! The session config a logical turn runs under (FIG-3600 S6, D3 §2): a run
//! whose recorded termination is missing refuses terminal assembly typed
//! (FIG-4508). The laws that drove a turn in process and redrove it were
//! deleted when the session actor took over running turns (FIG-5185); the
//! session actor's turn commit owes them (L3S-TURN).

use crate::ActorContext;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

use crate::admit;

/// The model the session starts on.
const FIRST_PROFILE: &str = "mock-model";
/// The model a config command moves the session to.
const SECOND_PROFILE: &str = "turn-config-second-model";

/// Everything a runtime for these laws is built from, shared by every
/// attempt so each is the same session on the same store.
#[derive(Clone)]
struct ConfigParts {
    session_id: SessionId,
    host: crate::RuntimeHostConfig,
    store: Arc<dyn crate::RuntimeStore>,
    /// The protocol the session runs: the standard fake unless the law
    /// needs another.
    protocol: Vec<Arc<dyn crate::plugin::PluginFactory>>,
    /// Plugins the law adds to the protocol.
    tools: Vec<Arc<dyn crate::plugin::PluginFactory>>,
}

async fn build_runtime(parts: ConfigParts) -> crate::LashRuntime {
    build_runtime_under(parts, crate::testing::mock_session_policy()).await
}

/// The law's runtime, opened with `policy` as its creation defaults: what a
/// session with no head yet starts from, and what its first commit records.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime_under(
    parts: ConfigParts,
    policy: crate::SessionPolicy,
) -> crate::LashRuntime {
    Box::pin(
        crate::LashRuntime::builder(parts.host, crate::testing::runtime_lease_owner())
            .with_session_id(&parts.session_id)
            .with_policy(policy)
            .with_plugin_factories(parts.protocol.into_iter().chain(parts.tools).collect())
            .with_store(crate::conformance::helpers::session_view(
                &parts.store,
                parts.session_id.clone(),
            ))
            .build(),
    )
    .await
    .expect("build the turn-config conformance runtime")
}

/// The host's models for these laws: [`FIRST_PROFILE`] and [`SECOND_PROFILE`],
/// both served by `provider`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: two distinct literal keys always register"
)]
fn turn_config_llm_profiles(provider: crate::ProviderHandle) -> Arc<crate::LlmProfileRegistry> {
    Arc::new(
        crate::LlmProfileRegistry::new()
            .register(
                FIRST_PROFILE,
                crate::RegisteredLlmProfile::new(
                    crate::testing::test_llm_profile_metadata(FIRST_PROFILE),
                    provider.clone(),
                ),
            )
            .and_then(|registry| {
                registry.register(
                    SECOND_PROFILE,
                    crate::RegisteredLlmProfile::new(
                        crate::testing::test_llm_profile_metadata(SECOND_PROFILE),
                        provider,
                    ),
                )
            })
            .expect("two distinct keys register"),
    )
}

/// A model that answers `answer <n>` to its n-th call and records the model
/// every call named.
fn recording_model(
    calls: &Arc<AtomicUsize>,
    models: &Arc<std::sync::Mutex<Vec<String>>>,
) -> crate::ProviderHandle {
    let calls = Arc::clone(calls);
    let models = Arc::clone(models);
    crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |request| {
            let index = calls.fetch_add(1, Ordering::SeqCst);
            models
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.model.wire_model().to_string());
            async move {
                Ok(crate::LlmResponse {
                    parts: vec![crate::LlmOutputPart::Text {
                        text: format!("answer {}", index + 1),
                        response_meta: None,
                    }],
                    ..crate::LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

/// The parts of a turn-config law's session: a host whose models serve the
/// recording model, and the session's store.
async fn law_session(
    prefix: &str,
    name: &str,
    effect_host: &ActorContext,
    stores: &Arc<dyn crate::StoreSet>,
    models: Arc<dyn crate::LlmProfiles>,
) -> ConfigParts {
    law_session_created_with(prefix, name, effect_host, stores, models, Vec::new()).await
}

/// [`law_session`], except the session is created with `tools` installed:
/// the created head records the plugin configuration a creator on that
/// plugin set resolves — the protocol pointer and every installed owner's
/// namespace (FIG-4379) — so a runtime that opens it later reads exactly the
/// namespaces creation recorded (FIG-4764).
async fn law_session_created_with(
    prefix: &str,
    name: &str,
    effect_host: &ActorContext,
    stores: &Arc<dyn crate::StoreSet>,
    models: Arc<dyn crate::LlmProfiles>,
    tools: Vec<Arc<dyn crate::plugin::PluginFactory>>,
) -> ConfigParts {
    law_session_recording(
        prefix,
        name,
        effect_host,
        stores,
        models,
        crate::testing::mock_session_policy(),
        tools,
    )
    .await
}

/// [`law_session`], except the created head records `policy` and the session
/// is created with `tools` installed: the session is created under the
/// policy and plugin set a creating deployment would mint, so a runtime that
/// opens it later adopts exactly the config the law means to record
/// (FIG-4553, FIG-4764).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the law's own plugin set's creation config resolves"
)]
async fn law_session_recording(
    prefix: &str,
    name: &str,
    _effect_host: &ActorContext,
    stores: &Arc<dyn crate::StoreSet>,
    models: Arc<dyn crate::LlmProfiles>,
    policy: crate::SessionPolicy,
    tools: Vec<Arc<dyn crate::plugin::PluginFactory>>,
) -> ConfigParts {
    let session_id = SessionId::fixture(format!("{prefix}-turn-config-{name}-session"));
    let mut host = crate::LawBackend::over_stores(Arc::clone(stores)).host_config(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.models = models;
    // The created head records what a creator on the session's plugin set
    // resolves (FIG-4379). These laws run the standard fake protocol, which
    // owns no plugin configuration; the law's tools are installed at
    // creation because an owner installed only afterwards never reaches the
    // recorded head.
    let protocol = crate::testing::test_standard_protocol_factories();
    let mut config = crate::PersistedSessionConfig::from(&policy);
    config.plugin_config = crate::plugin::PluginHost::new(
        protocol
            .iter()
            .cloned()
            .chain(tools.iter().cloned())
            .collect(),
    )
    .resolve_creation_plugin_config(
        Some("test_protocol"),
        &crate::PluginOptions::default(),
        None,
        true,
        &crate::store::plugin_writers::PluginAdmission::default(),
    )
    .expect("the law's plugin set resolves its creation plugin config");
    let store =
        crate::conformance::law_session_store_with_config(stores.as_ref(), &session_id, config)
            .await;
    ConfigParts {
        session_id,
        host,
        store,
        protocol,
        tools,
    }
}

/// A missing run record refuses terminal assembly without panicking,
/// retrying, or losing its code at a plugin or host boundary (FIG-4508).
#[expect(clippy::expect_used, reason = "conformance fixture results must exist")]
pub async fn a_missing_recorded_termination_is_a_typed_terminal_refusal(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let parts = law_session(
        prefix,
        "missing-recorded-termination",
        &effect_host,
        &stores,
        turn_config_llm_profiles(recording_model(&calls, &models)),
    )
    .await;
    let run = TurnId::fixture(format!("{prefix}-missing-recorded-termination-run"));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn(
            admit(crate::ExecutionScope::turn(&parts.session_id, &run)),
            Arc::new(move |scope| {
                let parts = parts.clone();
                let run = run.clone();
                let tx = tx.clone();
                Box::pin(async move {
                    let mut runtime = build_runtime(parts).await;
                    let result = runtime
                        .finish_without_recorded_run_for_testing(
                            run,
                            crate::TurnOptions::new(
                                tokio_util::sync::CancellationToken::new(),
                                scope,
                            ),
                        )
                        .await;
                    let _ = tx.send(result);
                    crate::ConformanceTurnEnd::Settled
                })
            }),
        )
        .await;
    let error = rx
        .recv()
        .await
        .expect("the commit attempt returned")
        .expect_err("a run without its record cannot assemble a terminal");
    let expected = crate::RuntimeErrorCode::from_wire_code("recorded_termination_unavailable");
    assert_eq!(error.code, expected);
    assert!(!error.is_retryable());
    assert!(error.is_terminal());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let plugin = crate::plugin::PluginError::Runtime(error.clone());
    let encoded = serde_json::to_vec(&plugin).expect("encode the plugin refusal");
    let decoded: crate::plugin::PluginError =
        serde_json::from_slice(&encoded).expect("decode the plugin refusal");
    let returned = decoded.into_turn_failure(crate::RuntimeErrorCode::PluginFinalizeTurn);
    assert_eq!(
        returned.code, expected,
        "the plugin boundary retains the cause"
    );
    assert!(!returned.is_retryable());
    let host = crate::SessionError::Plugin(crate::plugin::PluginError::Runtime(returned));
    let crate::SessionError::Plugin(crate::plugin::PluginError::Runtime(returned)) = host else {
        panic!("the host retains the typed runtime refusal");
    };
    assert_eq!(returned.code, expected);
    let controller = crate::RuntimeEffectControllerError::from(error);
    let encoded = serde_json::to_vec(&controller).expect("encode the controller refusal");
    let decoded: crate::RuntimeEffectControllerError =
        serde_json::from_slice(&encoded).expect("decode the controller refusal");
    let returned = crate::plugin::PluginError::RuntimeEffectController(decoded)
        .into_turn_failure(crate::RuntimeErrorCode::PluginFinalizeTurn);
    assert_eq!(
        returned.code, expected,
        "the controller boundary retains the cause"
    );
    assert!(!returned.is_retryable());
}
