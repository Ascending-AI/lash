//! Process-observer intent settlement conformance.

use crate::conformance::DeploymentViewExt as _;
use crate::{
    ProcessLifecycle as _, ProcessObserverRegistry as _, ProcessRegistrar as _,
    ProcessRetention as _,
};
use lash_sansio::SessionId;

/// A transient registry failure during fork-observer publication is best
/// effort: it does not fail session creation and the durable intent is
/// retained until publication succeeds.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn fork_observer_transient_failure_retains_intent_until_publication(
    backend: crate::Backend,
) {
    const SESSION_ID: &str = "fork-observer-transient-session";

    let factory = backend.session_store_factory();
    let registry = crate::testing::ProcessRegistryFaults::new(backend.process_registry());
    let session_id = SessionId::from(SESSION_ID);
    let process_id = registry
        .register_process(crate::ProcessRegistration::new(
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register fork observer process")
        .id;

    let store = factory
        .admit_view(&crate::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: vec![crate::SessionObserverIntent::host_requested(
                process_id.clone(),
            )],
            session_id: SessionId::from(SESSION_ID.to_string()),
            relation: crate::SessionRelation::Fork {
                source_session_id: SessionId::from("fork-observer-transient-source"),
                source_node_id: "fork-observer-transient-node".into(),
            },
            config: crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            )
            .into(),
            head: crate::SessionCreationHead::CommittedByCreator,
        })
        .await
        .expect("create fork session with pending observer intent");

    registry.set_process_read_error(Some(crate::PluginError::Session(
        "transient registry read failure".to_string(),
    )));
    crate::runtime::reconcile_session_process_observer_intents(
        Some(&registry),
        &SessionId::from(SESSION_ID),
        crate::runtime::SessionObserverIntentSource::Persisted(store.store().as_ref()),
    )
    .await
    .expect("transient registry failure must not fail fork observer settlement");
    registry.set_process_read_error(None);

    assert!(
        registry
            .list_observed_by(
                &SessionId::from(SESSION_ID),
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                }
            )
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
        meta.pending_observer_intents
            == vec![crate::SessionObserverIntent::host_requested(
                process_id.clone()
            )],
        "a transient failure must preserve the durable host choice"
    );

    let second = registry
        .register_process(crate::ProcessRegistration::new(
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register second selected process")
        .id;
    let pruned = registry
        .register_process(crate::ProcessRegistration::new(
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register process selected before pruning")
        .id;
    let terminal = registry
        .complete_process(
            &pruned,
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete pruned selection");
    registry
        .prune_terminal_processes(
            terminal.updated_at_ms.saturating_add(1),
            None,
            crate::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune selection");
    let mut meta = meta;
    let unresolved = vec![
        crate::SessionObserverIntent::host_requested(process_id.clone()),
        crate::SessionObserverIntent::host_requested(second.clone()),
    ];
    meta.pending_observer_intents = unresolved.clone();
    meta.pending_observer_intents.extend([
        crate::SessionObserverIntent::host_requested(lash_sansio::ProcessId::fixture("missing")),
        crate::SessionObserverIntent::host_requested(pruned),
    ]);
    store
        .save_session_meta(meta)
        .await
        .expect("retain full selector");
    registry.set_process_read_error_after(
        1,
        crate::PluginError::Session("second observer temporarily unavailable".into()),
    );
    let receipts = crate::runtime::reconcile_session_process_observer_intents(
        Some(&registry),
        &session_id,
        crate::runtime::SessionObserverIntentSource::Persisted(store.store().as_ref()),
    )
    .await
    .expect("partial publication is best effort");
    use crate::test_support::SessionObservedProcessOutcome;
    assert!(matches!(
        receipts[0].outcome,
        SessionObservedProcessOutcome::Observed
    ));
    assert!(matches!(
        receipts[1].outcome,
        SessionObservedProcessOutcome::Unavailable { .. }
    ));
    assert!(matches!(
        receipts[2].outcome,
        SessionObservedProcessOutcome::NotFound
    ));
    assert!(matches!(
        receipts[3].outcome,
        SessionObservedProcessOutcome::NoLongerRetained { .. }
    ));
    assert_eq!(
        store
            .load_session_meta()
            .await
            .expect("load partial selector")
            .expect("fork retained")
            .pending_observer_intents,
        unresolved,
        "keep published selections while any publication needs retry; settle missing and pruned separately"
    );

    // Publication committed, but the final metadata write did not: cold replay
    // must reassert the selector, including an edge removed before the clear.
    let crash_store = CrashBeforeClear {
        inner: store.store().clone(),
    };
    assert!(
        crate::runtime::reconcile_session_process_observer_intents(
            Some(&registry),
            &session_id,
            crate::runtime::SessionObserverIntentSource::Persisted(&crash_store),
        )
        .await
        .is_err()
    );
    assert_eq!(
        store
            .load_session_meta()
            .await
            .expect("read pending choice")
            .expect("fork exists")
            .pending_observer_intents,
        unresolved
    );
    registry
        .remove_observer(
            &session_id,
            &process_id,
            crate::ProcessObserverBy::host("remove-before-clear"),
        )
        .await
        .expect("remove edge");
    let reopened = factory
        .live_view(&session_id)
        .await
        .expect("reopen fork")
        .expect("fork exists");
    let receipts = crate::runtime::reconcile_session_process_observer_intents(
        Some(backend.process_registry().as_ref()),
        &session_id,
        crate::runtime::SessionObserverIntentSource::Persisted(reopened.store().as_ref()),
    )
    .await
    .expect("replay publication");
    assert_eq!(receipts.len(), 2);
    assert!(
        receipts
            .iter()
            .all(|receipt| matches!(receipt.outcome, SessionObservedProcessOutcome::Observed))
    );
    assert_eq!(
        registry
            .list_observed_by(
                &session_id,
                &crate::ProcessListFilter {
                    status: crate::ProcessStatusFilter::Any,
                    ..Default::default()
                }
            )
            .await
            .expect("read reasserted edge")
            .len(),
        2
    );
    assert!(
        reopened
            .load_session_meta()
            .await
            .expect("read settled choice")
            .expect("fork exists")
            .pending_observer_intents
            .is_empty()
    );
}

struct CrashBeforeClear {
    inner: std::sync::Arc<dyn crate::RuntimeStore>,
}

#[async_trait::async_trait]
impl crate::store::RuntimeStoreDecorator for CrashBeforeClear {
    type Inner = dyn crate::RuntimeStore;
    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }
    async fn save_session_meta(
        &self,
        _meta: crate::store::SessionMeta,
    ) -> Result<(), crate::StoreError> {
        Err(crate::StoreError::Backend(
            "crash before observer intent clear".to_string(),
        ))
    }
}
