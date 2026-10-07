//! FIG-4275: one worker slot, shared by a cell and the process engine as a
//! production registration shares it: resolving a process definition's
//! artifact waits for the slot without holding the runtime thread, and a
//! checkout that times out crosses the plugin boundary as a retryable fault.

use super::*;

/// A worker service of exactly one slot, with a checkout deadline long
/// enough for any legitimate wait and short enough to report a deadlock.
fn one_slot_workers() -> lash_vm_client::service::Service {
    let mut config = lash_vm_client::service::Service::default().config().clone();
    config.min_workers = 1;
    config.max_workers = 1;
    config.deadlines.checkout = std::time::Duration::from_secs(10);
    lash_vm_client::service::Service::new(config)
}

async fn published_definition_fixture(
    checkout_deadline: std::time::Duration,
) -> (
    crate::testing::DurableHost,
    lash_vm_client::service::Service,
    lashlang::LashlangArtifacts,
    lash_vm_client::service::CreatedDefinition,
) {
    use lash_vm_client::service::{Request, Response};
    let host = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
    let mut config = one_slot_workers().config().clone();
    config.deadlines.checkout = checkout_deadline;
    let workers = lash_vm_client::service::Service::new(config);
    let Response::Definition(created) = workers
        .request_accounted(Request::CreateDefinition {
            source: "const answer = async (): Promise<number> => { return 42; };".into(),
            environment: lashlang::LashlangHostEnvironment::default(),
        })
        .await
        .expect("compile the definition")
    else {
        panic!("a compiled definition");
    };
    let artifacts = lashlang::LashlangArtifacts::new(host.backend().module_artifacts());
    let claim = lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
        lash_core::HostArtifactPin::mint(),
    ))
    .expect("host claim");
    artifacts
        .store()
        .publish_module_artifact(
            &claim,
            &created.module.module_ref,
            created.module.bytes.as_bytes(),
        )
        .await
        .expect("publish the module");
    (host, workers, artifacts, created)
}

fn held_worker(workers: &lash_vm_client::service::Service) -> lash_vm_client::Checkout {
    use lash_vm_client::WorkerPoolRuntimeOps as _;
    workers
        .pool()
        .expect("pool")
        .checkout(
            4096,
            lash_vm_protocol::OwnerEpoch(0),
            lash_vm_protocol::FrameEpoch(0),
            lash_vm_client::ExecutionBudget::default(),
        )
        .expect("hold the sole worker")
}

fn definition_engines(
    workers: &lash_vm_client::service::Service,
    artifacts: lashlang::LashlangArtifacts,
) -> lash_core::ProcessEngineRegistry {
    let engine = lash_lashlang_runtime::LashlangProcessEngine::new(
        artifacts,
        lash_lashlang_runtime::LashlangSurface::default(),
    )
    .with_worker_service(workers.clone());
    lash_core::ProcessEngineRegistry::new().with_registration(
        lash_lashlang_runtime::lashlang_process_engine_registration(engine),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn artifact_resolution_yields_to_the_task_releasing_the_only_slot() {
    let (_host, workers, artifacts, created) =
        published_definition_fixture(std::time::Duration::from_secs(10)).await;
    let engines = definition_engines(&workers, artifacts);
    let held = held_worker(&workers);
    // Poll resolution first. Its checkout must yield so the cell can park
    // and release the only worker on this same runtime thread.
    let finished = std::sync::atomic::AtomicBool::new(false);
    let (resolved, ()) = tokio::join!(biased;
        async {
            let result = engines.derive_definition(&created.draft).await;
            finished.store(true, std::sync::atomic::Ordering::SeqCst);
            result
        },
        async {
            while workers.pool().expect("pool").stats().queued_items == 0
                && !finished.load(std::sync::atomic::Ordering::SeqCst)
            {
                tokio::task::yield_now().await;
            }
            held.release().expect("release the cell's slot");
        },
    );
    resolved.expect("artifact resolution must not block the slot's owner");
}

#[tokio::test(flavor = "current_thread")]
async fn artifact_checkout_timeout_crosses_the_plugin_boundary_as_a_retryable_fault() {
    let (host, workers, artifacts, created) =
        published_definition_fixture(std::time::Duration::from_millis(250)).await;
    let engines = definition_engines(&workers, artifacts.clone());
    let held = held_worker(&workers);
    let artifact_error = workers
        .inspect_artifact(
            &artifacts,
            &lashlang::ProcessDefinitionIdentity::from_process_value(
                created.draft.value().as_json(),
            )
            .expect("definition identity")
            .module_ref,
        )
        .await
        .expect_err("the only worker remains held");
    let artifact_plugin: lash_core::PluginError = artifact_error.into();
    assert!(artifact_plugin.is_retryable(), "{artifact_plugin:?}");
    let refusal = engines
        .derive_definition(&created.draft)
        .await
        .expect_err("engine resolution must time out");
    let plugin: lash_core::PluginError = refusal.into();
    assert!(plugin.is_retryable(), "{plugin:?}");
    assert!(
        matches!(&plugin, lash_core::PluginError::RuntimeEffectController(error)
        if error.code.as_str() == "worker_checkout_timed_out" && error.is_attempt_fault())
    );
    let claim = lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
        lash_core::HostArtifactPin::mint(),
    ))
    .expect("host claim");
    let error = lash_core::ArtifactReferrerPorts::of_backend(host.backend())
        .publish_definition(&engines, &claim, &created.draft)
        .await
        .expect_err("definition inspection must time out");
    held.release().expect("release the held slot");
    assert!(error.is_retryable(), "{error:?}");
    assert!(
        matches!(&error, lash_core::PluginError::RuntimeEffectController(error)
        if error.is_attempt_fault()),
        "the cell must not seal this fault: {error:?}"
    );
    let error: lash_core::PluginError =
        serde_json::from_value(serde_json::to_value(error).expect("encode the plugin fault"))
            .expect("decode the plugin fault");
    let lash_core::PluginError::RuntimeEffectController(error) = error else {
        panic!("the typed controller fault must cross the plugin boundary");
    };
    assert_eq!(error.code.as_str(), "worker_checkout_timed_out");
    assert!(error.code.is_retryable());
    assert_eq!(
        error.turn_failure_cause(),
        lash_core::TurnFailureCause::LiveFault
    );
    let runtime = lash_core::PluginError::RuntimeEffectController(error)
        .into_turn_failure(lash_core::RuntimeErrorCode::Plugin);
    assert_eq!(runtime.code.as_str(), "worker_checkout_timed_out");
    assert_eq!(
        runtime.turn_failure_cause(),
        lash_core::TurnFailureCause::LiveFault
    );
}
