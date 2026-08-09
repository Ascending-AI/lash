//! Process-observer intent settlement conformance.

use super::*;

/// A transient registry failure during fork-observer publication is best
/// effort: it does not fail session creation and the durable intent is
/// consumed.
pub async fn fork_observer_intent_transient_failure(factory: Arc<dyn crate::SessionStoreFactory>) {
    const SESSION_ID: &str = "fork-observer-transient-session";
    const PROCESS_ID: &str = "fork-observer-transient-process";

    let registry = crate::TestLocalProcessRegistry::default();
    registry
        .register_process(crate::ProcessRegistration::new(
            PROCESS_ID,
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::RecoveryDisposition::ExternallyOwned,
            crate::ProcessProvenance::host(),
        ))
        .await
        .expect("register fork observer process");

    let store = factory
        .create_store(&crate::SessionStoreCreateRequest {
            session_id: SESSION_ID.to_string(),
            relation: crate::SessionRelation::Fork {
                source_session_id: "fork-observer-transient-source".to_string(),
                source_node_id: "fork-observer-transient-node".to_string(),
                observer_inheritance: crate::ObserverInheritance::All,
                pending_observer_process_ids: vec![PROCESS_ID.to_string()],
            },
            policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        })
        .await
        .expect("create fork session with pending observer intent");

    registry
        .set_process_read_error(Some(crate::PluginError::Session(
            "transient registry read failure".to_string(),
        )))
        .await;
    crate::runtime::reconcile_session_process_observer_intents(
        Some(&registry),
        SESSION_ID,
        crate::runtime::SessionObserverIntentSource::Persisted(store.as_ref()),
    )
    .await
    .expect("transient registry failure must not fail fork observer settlement");
    registry.set_process_read_error(None).await;

    assert!(
        registry
            .list_observed_by(SESSION_ID)
            .await
            .expect("list observers after transient failure")
            .is_empty(),
        "an unavailable process must not gain an observer edge"
    );
    let meta = store
        .load_session_meta()
        .await
        .expect("load settled fork metadata")
        .expect("settled fork metadata exists");
    assert!(
        matches!(
            meta.relation,
            crate::SessionRelation::Fork {
                ref pending_observer_process_ids,
                ..
            } if pending_observer_process_ids.is_empty()
        ),
        "best-effort settlement must consume a transiently unavailable fork observer intent"
    );
}
