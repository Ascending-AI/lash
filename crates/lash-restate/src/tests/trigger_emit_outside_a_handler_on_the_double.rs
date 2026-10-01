//! An emission outside a Restate handler is refused whole (FIG-4513).
//!
//! An emission journals its ingest as a recorded step (FIG-4503), and the
//! deployment's effect host runs a step only inside a handler. A scope the
//! host lends outside one therefore refuses the emission at that first step,
//! before the trigger store is reached: whether or not a subscription
//! matches, the refusal is the typed handler-scope one and no occurrence or
//! delivery is written. Every production emitter runs inside a handler and
//! passes its handler's controller: a turn's intent drain, the tool-intent
//! ingress (which on Restate only runs inside a handler scope), and a host's
//! own handlers through `LashCore::triggers().emit`.

use super::*;

const SOURCE_TYPE: &str = "ui.button.pressed";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_emission_outside_a_handler_is_refused_and_writes_nothing() {
    let double =
        lash_restate_test::backend(0x4513_0000, lash_restate_test::ServerConfig::default())
            .await
            .expect("build the Restate test backend");
    let backend = double.lash_backend();
    let triggers = backend.trigger_store();
    let registry = backend.process_registry();
    lash_core::testing::process_execution_env_fixture(backend.process_env_store().as_ref()).await;
    super::restate_redrive::register_fig811_subscription(
        triggers.as_ref(),
        "fig4513-register",
        "fig4513-target",
        "fig4513-matched-source",
    )
    .await;
    let router = lash_core::facade_support::TriggerRouter::new(
        Arc::clone(&triggers),
        registry_process_wiring(Arc::clone(&registry)),
    )
    .with_process_artifacts(
        backend.process_env_store(),
        lash_core::testing::process_engine_fixture(),
    );
    let host = backend.effect_host();

    for (case, source_key) in [
        ("a matched emission", "fig4513-matched-source"),
        ("a zero-match emission", "fig4513-unmatched-source"),
    ] {
        let scoped = host
            .scoped(lash_core::AdmittedScope::runtime_operation(format!(
                "fig4513:{source_key}"
            )))
            .expect("the host lends a scope outside a handler");
        let refusal = router
            .emit(
                lash_core::TriggerOccurrenceRequest::new(
                    SOURCE_TYPE,
                    source_key,
                    serde_json::json!({ "button": "Blue" }),
                    format!("fig4513-occurrence:{source_key}"),
                ),
                &scoped,
            )
            .await
            .expect_err("an emission outside a handler is refused");
        assert!(
            matches!(
                &refusal,
                lash_core::PluginError::RuntimeEffectController(error)
                    if error.code
                        == lash_core::RuntimeErrorCode::EngineEffectHostRequiresHandlerScope
            ),
            "{case} is refused as outside a handler: {refusal:?}"
        );
        assert_eq!(
            triggers
                .list_occurrences(lash_core::TriggerOccurrenceFilter::default())
                .await
                .expect("list occurrences"),
            Vec::new(),
            "{case} wrote an occurrence before its refusal"
        );
        assert_eq!(
            triggers.list_deliveries().await.expect("list deliveries"),
            Vec::new(),
            "{case} reserved a delivery before its refusal"
        );
    }
}
