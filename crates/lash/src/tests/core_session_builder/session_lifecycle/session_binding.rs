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
        session_id: &lash_core::SessionId,
    ) -> std::result::Result<(), lash_core::RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl lash_core::EffectHost for FailOnceRetirementHost {
    fn turn_control_binding_id(&self) -> String {
        "fail-once-retirement-host".to_string()
    }

    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self
    }

    fn scoped<'a>(
        &'a self,
        scope: lash_core::AdmittedScope,
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

/// FIG-3373: the store requirement is admission law on the ordinary open
/// path, not a property of a creation API — a `.parent(..)` open is refused
/// exactly as a root open is when the core has no session store.
#[tokio::test]
async fn every_opened_session_requires_a_store_regardless_of_relation() -> Result<()> {
    let core = core_without_session_store();
    let _parent = core
        .session("explicit-parent-store")
        .store(Arc::new(
            lash_core::facade_support::InMemorySessionStore::default(),
        ))
        .open()
        .await?;

    let root_error = match core.session("created-root-without-catalog").open().await {
        Ok(_) => panic!("root open without a store must be refused"),
        Err(error) => error,
    };
    assert!(matches!(root_error, EmbedError::MissingSessionStore));

    let child_error = match core
        .session("created-child-without-catalog")
        .parent("explicit-parent-store")
        .open()
        .await
    {
        Ok(_) => panic!("related open without a store must be refused"),
        Err(error) => error,
    };
    assert!(matches!(child_error, EmbedError::MissingSessionStore));
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

    let parked = Box::pin(source.session("owner-preserved").open().await?.park()).await?;
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
        .open_existing_store_by_id(&lash_core::SessionId::from("owner-preserved"))
        .await
        .expect("read source catalog")
        .expect("source session store");
    let source_driver = lash_core::facade_support::TurnWorkDriver::for_session(
        source_host,
        lash_core::SessionId::from("owner-preserved"),
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
    assert!(
        core.session("delete-retry")
            .durable()
            .await?
            .was_deleted()
            .await?
    );

    let report = LashCore::delete_session(administration.delete_context("delete-retry")?).await?;
    assert_eq!(report.session_id, "delete-retry");
    assert!(
        core.session("delete-retry")
            .durable()
            .await?
            .was_deleted()
            .await?
    );
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
        .open_existing_store_by_id(&lash_core::SessionId::from("catalog-root"))
        .await
        .expect("read root catalog")
        .expect("catalog root store");
    assert!(
        catalog_store
            .turn_cancel_request(&catalog_address)
            .await?
            .is_some()
    );

    let child = core
        .session("explicit-root-child")
        .parent("explicit-root-store")
        .open()
        .await?;
    assert_eq!(child.parent_session_id(), Some("explicit-root-store"));

    assert!(
        root_catalog
            .open_existing_store_by_id(&lash_core::SessionId::from("explicit-root-child"))
            .await
            .expect("read root catalog")
            .is_some(),
        "a `.parent(..)` open is admitted through the core store factory like \
         every other facade open"
    );
    assert!(
        creation_catalog.session_ids().is_empty(),
        "the session-creation catalog serves the internal creation boundary, \
         not facade opens"
    );
    Ok(())
}

/// FIG-1559: the handle reports the relation the store recorded, and a rebind
/// that renames the parent is a typed refusal rather than silent absorption.
#[tokio::test]
async fn parent_relation_is_read_back_and_a_conflicting_rebind_is_refused() -> Result<()> {
    let store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(lash_core::facade_support::InMemorySessionStore::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;

    let child = core
        .session("relation-child")
        .store(Arc::clone(&store))
        .parent("relation-parent")
        .open()
        .await?;
    assert_eq!(child.parent_session_id(), Some("relation-parent"));
    drop(child);

    // A reopen that names no parent still reports the recorded relation: the
    // handle reads the durable fact, not the request it was built from.
    let reopened = core
        .session("relation-child")
        .store(Arc::clone(&store))
        .open()
        .await?;
    assert_eq!(reopened.parent_session_id(), Some("relation-parent"));
    drop(reopened);

    let error = match core
        .session("relation-child")
        .store(Arc::clone(&store))
        .parent("other-parent")
        .open()
        .await
    {
        Ok(_) => panic!("a rebind naming a different parent must be refused"),
        Err(error) => error,
    };
    match &error {
        crate::EmbedError::Store(lash_core::store::StoreError::SessionRelationMismatch {
            session_id,
            recorded,
            requested,
        }) => {
            assert_eq!(session_id.as_str(), "relation-child");
            assert_eq!(
                **recorded,
                lash_core::SessionLineage::Child {
                    parent_session_id: "relation-parent".into()
                }
            );
            assert_eq!(
                **requested,
                lash_core::SessionLineage::Child {
                    parent_session_id: "other-parent".into()
                }
            );
        }
        other => panic!("expected a typed relation-mismatch refusal, got: {other:?}"),
    }

    // The refusal left the recorded relation intact.
    let after = core.session("relation-child").store(store).open().await?;
    assert_eq!(after.parent_session_id(), Some("relation-parent"));
    Ok(())
}

/// ADR 0088's resume clause reaches past the store: a parked session keeps the
/// *owner services* it was opened with, not the ones belonging to the core that
/// resumes it.
///
/// `BoundSession::apply_owner` (`crates/lash/src/session_binding.rs`) is where
/// that happens — it overwrites the receiving environment's effect host, trigger
/// store, process definitions, child-store factory, attachment store, process-env
/// store and work ports with the parked binding's. The sibling test above proves
/// the store and effect-host halves through cancellation. This one proves a
/// service the store cannot stand in for: the process registry reached through
/// the resumed session's own admin surface. A resume that took the receiving
/// core's registry would read an empty deployment while the source's rows stayed
/// live and unaddressable, and every assertion that goes through the store would
/// still pass.
#[tokio::test]
async fn resume_addresses_the_parked_owner_registry_not_the_receiving_core() -> Result<()> {
    let owner = crate::testing::runtime_lease_owner();
    let session_id = "owner-services-preserved";
    let process_id = lash_core::ProcessId::from("owner-services-process");

    let source_registry = Arc::new(crate::testing::TestLocalProcessRegistry::default());
    let receiving_registry = Arc::new(crate::testing::TestLocalProcessRegistry::default());

    let source =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(text_provider(
                "owner-services-provider",
                "owner-services-model",
                "source-provider",
            ))
            .model(model_spec("owner-services-model", None, 200_000))
            .store_factory(Arc::new(
                lash_core::facade_support::InMemorySessionStoreFactory::new(),
            ))
            .process_registry(source_registry.clone())
            .build(owner.clone())?;
    let receiving =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(text_provider(
                "owner-services-provider",
                "owner-services-model",
                "receiving-provider",
            ))
            .model(model_spec("owner-services-model", None, 200_000))
            .store_factory(Arc::new(
                lash_core::facade_support::InMemorySessionStoreFactory::new(),
            ))
            .process_registry(receiving_registry.clone())
            .build(owner)?;

    let session = source.session(session_id).open().await?;
    source_registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                &process_id,
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::session(session.observe().process_scope()),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            ),
            &[lash_core::SessionId::from(session_id)],
        )
        .await?;
    assert_eq!(
        session
            .admin()
            .processes()
            .list_all()
            .await?
            .into_iter()
            .map(|process| process.process_id)
            .collect::<Vec<_>>(),
        vec![process_id.clone()],
        "the opened session addresses the registry its own core supplied"
    );

    let parked = Box::pin(session.park()).await?;
    let resumed = receiving.resume(parked).await?;

    assert_eq!(
        resumed
            .admin()
            .processes()
            .list_all()
            .await?
            .into_iter()
            .map(|process| process.process_id)
            .collect::<Vec<_>>(),
        vec![process_id.clone()],
        "resume carries the parked binding's registry forward rather than \
         substituting the receiving core's"
    );
    let receiving_registry: Arc<dyn lash_core::ProcessRegistry> = receiving_registry;
    assert!(
        receiving_registry
            .list_processes(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            })
            .await?
            .is_empty(),
        "the row was read where it lives: resume copies nothing into the \
         receiving core's registry"
    );
    Ok(())
}
