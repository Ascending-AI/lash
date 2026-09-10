use super::*;
use lash_core::TurnInputStore;
use std::sync::atomic::{AtomicBool, Ordering};

struct FailOnceRetirementHost {
    inner: lash_core::facade_support::NativeEffectHost,
    fail_next_retirement: AtomicBool,
}

impl Default for FailOnceRetirementHost {
    fn default() -> Self {
        Self {
            inner: lash_core::facade_support::NativeEffectHost::default(),
            fail_next_retirement: AtomicBool::new(true),
        }
    }
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for FailOnceRetirementHost {
    async fn revoke_await_events_for_session(
        &self,
        session_id: &str,
    ) -> std::result::Result<(), lash_core::RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl lash_core::EffectHost for FailOnceRetirementHost {
    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self
    }

    fn scoped<'a>(
        &'a self,
        scope: lash_core::ExecutionScope,
    ) -> std::result::Result<lash_core::ScopedEffectController<'a>, lash_core::RuntimeError> {
        self.inner.scoped(scope)
    }

    async fn retire_effect_journal(
        &self,
        retirement: lash_core::EffectJournalRetirement,
    ) -> std::result::Result<usize, lash_core::RuntimeError> {
        if self.fail_next_retirement.swap(false, Ordering::SeqCst) {
            return Err(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::RuntimeStore,
                "injected journal retirement failure",
            ));
        }
        self.inner.retire_effect_journal(retirement).await
    }
}

#[tokio::test]
async fn facade_refuses_to_open_without_an_explicit_session_store() {
    let core = core_without_session_store();
    let error = match core.session("store-less-single-use").open().await {
        Ok(_) => panic!("facade session open requires an explicit store"),
        Err(error) => error,
    };
    assert!(matches!(error, EmbedError::MissingSessionStore));
}

#[tokio::test]
async fn catalog_and_administration_are_typed_unavailable_without_a_root_catalog() {
    let core = core_without_session_store();
    assert!(matches!(
        core.turn_work_driver(),
        Err(EmbedError::SessionCatalogUnavailable {
            operation: "turn_work_driver"
        })
    ));
    assert!(matches!(
        core.session_administration().await,
        Err(EmbedError::SessionCatalogUnavailable {
            operation: "session_administration"
        })
    ));
}

#[tokio::test]
async fn every_created_session_requires_a_store_regardless_of_relation() -> Result<()> {
    let core = core_without_session_store();
    let parent = core
        .session("explicit-parent-store")
        .store(Arc::new(
            lash_core::facade_support::InMemorySessionStore::default(),
        ))
        .open()
        .await?;

    for (session_id, relation) in [
        (
            "created-root-without-catalog",
            lash_core::SessionRelation::Root,
        ),
        (
            "created-child-without-catalog",
            lash_core::SessionRelation::Child {
                parent_session_id: parent.session_id(),
                caused_by: None,
            },
        ),
    ] {
        let error = parent
            .admin()
            .children()
            .create_session(SessionCreateRequest {
                session_id: Some(session_id.to_string()),
                relation,
                start: lash_core::SessionStartPoint::Empty,
                policy: None,
                plugin_source: lash_core::SessionPluginSource::CurrentSessionFork,
                initial_nodes: Vec::new(),
                observed_processes: Vec::new(),
                tool_access: lash_core::SessionToolAccess::default(),
                subagent: None,
                context_overlay: lash_core::SessionContextOverlay::default(),
                plugin_options: lash_core::PluginOptions::default(),
                usage_source: None,
            })
            .await
            .expect_err("session creation without a catalog must be refused");
        assert!(matches!(
            error,
            EmbedError::Plugin(lash_core::PluginError::MissingSessionStore {
                session_id: ref missing,
            }) if missing == session_id
        ));
    }
    Ok(())
}

#[tokio::test]
async fn resume_preserves_the_parked_lifecycle_owner_with_the_same_lease_identity() -> Result<()> {
    let owner = crate::testing::runtime_lease_owner();
    let source_host = Arc::new(lash_core::facade_support::NativeEffectHost::default());
    let receiving_host = Arc::new(lash_core::facade_support::NativeEffectHost::default());
    let source_catalog = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let receiving_catalog = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let source =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(text_provider(
                "resume-provider",
                "resume-model",
                "source-provider",
            ))
            .model(model_spec("resume-model", None, 200_000))
            .store_factory(source_catalog.clone())
            .effect_host(source_host.clone())
            .build(owner.clone())?;
    let receiving =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(text_provider(
                "resume-provider",
                "resume-model",
                "receiving-provider",
            ))
            .model(model_spec("resume-model", None, 200_000))
            .store_factory(receiving_catalog)
            .effect_host(receiving_host)
            .build(owner)?;

    let parked = source
        .session("owner-preserved")
        .open()
        .await?
        .park()
        .await?;
    let resumed = receiving.resume(parked).await?;
    let result = resumed
        .turn(TurnInput::text("use receiving core live configuration"))
        .run()
        .await?;
    assert_eq!(assistant_prose(&result.activities), "receiving-provider");

    let turn_id = lash_sansio::TurnId::from("owner-preserved-turn");
    resumed
        .request_turn_cancel(
            &turn_id,
            "resume-owner-request",
            Some("test".to_string()),
            None,
        )
        .await?;

    let store = source_catalog
        .open_existing_store_by_id("owner-preserved")
        .await
        .expect("read source catalog")
        .expect("source session store");
    let source_driver = lash_core::facade_support::TurnWorkDriver::for_session(
        source_host,
        "owner-preserved",
        store,
    );
    let duplicate = source_driver
        .request_cancel(crate::TurnCancelRequest::new(
            crate::TurnAddress::new("owner-preserved", &turn_id),
            "resume-owner-probe",
            Some("test".to_string()),
        ))
        .await?;
    assert!(matches!(
        duplicate.outcome,
        crate::TurnCancelOutcome::AlreadyRequested(_)
    ));
    Ok(())
}

#[tokio::test]
async fn session_delete_context_retries_after_storage_tombstone() -> Result<()> {
    let factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let effect_host = Arc::new(FailOnceRetirementHost::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(factory)
        .effect_host(effect_host)
        .build(crate::testing::runtime_lease_owner())?;
    drop(core.session("delete-retry").open().await?);
    let administration = core.session_administration().await?;

    let first = LashCore::delete_session(administration.delete_context("delete-retry")?)
        .await
        .expect_err("first retirement fails after the tombstone commits");
    assert!(matches!(first, EmbedError::SessionDeleteProcess { .. }));
    assert!(core.session_was_deleted("delete-retry").await?);

    let report = LashCore::delete_session(administration.delete_context("delete-retry")?).await?;
    assert_eq!(report.session_id, "delete-retry");
    assert!(core.session_was_deleted("delete-retry").await?);
    Ok(())
}

#[tokio::test]
async fn exact_opened_store_and_session_creation_catalog_remain_distinct() -> Result<()> {
    let root_catalog = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let creation_catalog = Arc::new(RecordingStoreFactory::default());
    let explicit_store = Arc::new(lash_core::facade_support::InMemorySessionStore::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(root_catalog.clone())
        .session_creation_store_factory(creation_catalog.clone())
        .build(crate::testing::runtime_lease_owner())?;
    drop(core.session("catalog-root").open().await?);
    let session = core
        .session("explicit-root-store")
        .store(explicit_store.clone())
        .open()
        .await?;
    assert!(matches!(
        session.session_administration(),
        Err(EmbedError::SessionCatalogUnavailable {
            operation: "session_administration"
        })
    ));

    let explicit_turn = lash_sansio::TurnId::from("explicit-root-turn");
    session
        .request_turn_cancel(
            &explicit_turn,
            "explicit-root-cancel",
            Some("test".to_string()),
            None,
        )
        .await?;
    let explicit_address = crate::TurnAddress::new("explicit-root-store", &explicit_turn);
    assert!(
        explicit_store
            .turn_cancel_request(&explicit_address)
            .await?
            .is_some(),
        "the opened root records cancellation in its exact explicit store"
    );

    let catalog_address = crate::TurnAddress::new("catalog-root", "catalog-turn");
    core.turn_work_driver()?
        .request_cancel(crate::TurnCancelRequest::new(
            catalog_address.clone(),
            "catalog-cancel",
            Some("test".to_string()),
        ))
        .await?;
    let catalog_store = root_catalog
        .open_existing_store_by_id("catalog-root")
        .await
        .expect("read root catalog")
        .expect("catalog root store");
    assert!(
        catalog_store
            .turn_cancel_request(&catalog_address)
            .await?
            .is_some()
    );

    session
        .admin()
        .children()
        .create_session(SessionCreateRequest {
            session_id: Some("explicit-root-child".to_string()),
            relation: lash_core::SessionRelation::Child {
                parent_session_id: "explicit-root-store".to_string(),
                caused_by: None,
            },
            start: lash_core::SessionStartPoint::Empty,
            policy: None,
            plugin_source: lash_core::SessionPluginSource::CurrentSessionFork,
            initial_nodes: Vec::new(),
            observed_processes: Vec::new(),
            tool_access: lash_core::SessionToolAccess::default(),
            subagent: None,
            context_overlay: lash_core::SessionContextOverlay::default(),
            plugin_options: lash_core::PluginOptions::default(),
            usage_source: None,
        })
        .await?;

    session
        .admin()
        .children()
        .create_session(SessionCreateRequest {
            session_id: Some("explicit-root-related-root".to_string()),
            relation: lash_core::SessionRelation::Root,
            start: lash_core::SessionStartPoint::Empty,
            policy: None,
            plugin_source: lash_core::SessionPluginSource::CurrentSessionFork,
            initial_nodes: Vec::new(),
            observed_processes: Vec::new(),
            tool_access: lash_core::SessionToolAccess::default(),
            subagent: None,
            context_overlay: lash_core::SessionContextOverlay::default(),
            plugin_options: lash_core::PluginOptions::default(),
            usage_source: None,
        })
        .await?;

    assert_eq!(
        creation_catalog.session_ids(),
        vec![
            "explicit-root-child".to_string(),
            "explicit-root-related-root".to_string(),
        ],
        "the creation catalog is selected by the creation boundary, not relation kind"
    );
    Ok(())
}
