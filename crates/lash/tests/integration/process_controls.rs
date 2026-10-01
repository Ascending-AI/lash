//! A host installs the first-party process-control tools through the facade
//! alone (FIG-4375): `lash::process_controls` names the factory, and
//! `lash::process::lifetime` names the lifetime policy its starts take.
#![expect(
    clippy::expect_used,
    reason = "integration fixture setup and assertions"
)]

use std::sync::Arc;

use lash::LashCore;
use lash::process_controls::SessionProcessAdminPluginFactory;

const SEED: u64 = 0x4375;

async fn core(
    install: Option<SessionProcessAdminPluginFactory>,
) -> (LashCore, lash_restate_test::RestateTestBackend) {
    let provider = lash_core::testing::TestProvider::builder()
        .complete_error("process-control installation opens no turn")
        .build()
        .into_handle();
    let double = crate::support::restate_double(SEED).await;
    let mut builder = LashCore::standard_builder(double.lash_backend())
        .models(Arc::new(
            lash::ModelRegistry::new()
                .register(
                    "mock-model",
                    lash::RegisteredModel::new(
                        lash::ModelMetadata::builder("mock-model")
                            .context_window_tokens(16_000)
                            .build()
                            .expect("valid model metadata"),
                        provider,
                    ),
                )
                .expect("one model registers"),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024));
    if let Some(factory) = install {
        builder = builder.plugin(Arc::new(factory));
    }
    let core = builder
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "process-controls-test-worker",
            "process-controls-test-boot",
        ))
        .expect("core");
    (core, double)
}

async fn tool_names(core: &LashCore, session_id: &str) -> Vec<String> {
    let session = crate::created_session(core, "mock-model", session_id)
        .await
        .open()
        .await
        .expect("session");
    let mut names: Vec<String> = session
        .admin()
        .tools()
        .active_manifests()
        .await
        .expect("active tool manifests")
        .into_iter()
        .map(|definition| definition.name.to_string())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn a_host_installs_the_process_controls_through_the_facade() {
    let (core, _double) = core(Some(SessionProcessAdminPluginFactory::new(
        lash::process::lifetime::session_or_starter,
    )))
    .await;
    let names = tool_names(&core, "process-controls-installed").await;
    for tool in [
        "await_process",
        "cancel_process",
        "emit_process_event",
        "get_process_definition",
        "list_process_handles",
        "signal_process",
        "start_process",
    ] {
        assert!(
            names.iter().any(|name| name == tool),
            "`{tool}` is installed by the facade's process-control factory: {names:?}"
        );
    }
}

#[tokio::test]
async fn a_host_can_install_the_process_controls_without_cancel_process() {
    let (core, _double) = core(Some(
        SessionProcessAdminPluginFactory::without_cancel_process(
            lash::process::lifetime::session_or_starter,
        ),
    ))
    .await;
    let names = tool_names(&core, "process-controls-without-cancel").await;
    assert!(
        names.iter().any(|name| name == "start_process"),
        "{names:?}"
    );
    assert!(
        !names.iter().any(|name| name == "cancel_process"),
        "{names:?}"
    );
}

#[tokio::test]
async fn a_core_without_the_process_controls_has_no_start_process() {
    let (core, _double) = core(None).await;
    let names = tool_names(&core, "process-controls-absent").await;
    assert!(
        !names.iter().any(|name| name == "start_process"),
        "{names:?}"
    );
}
