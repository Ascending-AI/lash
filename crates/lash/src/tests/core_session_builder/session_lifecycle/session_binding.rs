use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// The backend's effect host with its next journal retirement failing.
struct FailOnceRetirementHost {
    inner: Arc<dyn lash_core::EffectHost>,
    fail_next_retirement: AtomicBool,
}

impl FailOnceRetirementHost {
    fn over(inner: Arc<dyn lash_core::EffectHost>) -> Self {
        Self {
            inner,
            fail_next_retirement: AtomicBool::new(true),
        }
    }
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for FailOnceRetirementHost {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.await_event_authority_binding_id()
    }

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
        self.inner.turn_control_binding_id()
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

    fn scoped_static(
        &self,
        scope: lash_core::AdmittedScope,
    ) -> std::result::Result<
        Option<lash_core::ScopedEffectController<'static>>,
        lash_core::RuntimeError,
    > {
        self.inner.scoped_static(scope)
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
async fn resume_preserves_the_parked_lifecycle_owner_with_the_same_lease_identity() -> Result<()> {
    let owner = crate::testing::runtime_lease_owner();
    let backend = memory_backend().await;
    let source_host = backend.effect_host();
    let source_catalog = backend.session_store_factory();
    let source = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone().into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(text_provider(
        "resume-provider",
        "resume-model",
        "source-provider",
    ))
    .model(model_spec("resume-model", None, 200_000))
    .build(owner.clone())?;
    let receiving = explicit_ephemeral_facets(LashCore::standard_builder(
        memory_backend().await.into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(text_provider(
        "resume-provider",
        "resume-model",
        "receiving-provider",
    ))
    .model(model_spec("resume-model", None, 200_000))
    .build(owner)?;

    let parked = Box::pin(source.session("owner-preserved").open().await?.park()).await?;
    let resumed = receiving.resume(parked).await?;
    let result = resumed
        .send(TurnInput::text("use receiving core live configuration"))
        .output()
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

    let store = lash_core::SessionStoreFactory::open_existing_store_by_id(
        source_catalog.as_ref(),
        &lash_core::SessionId::from("owner-preserved"),
    )
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
    let backend = DecoratedBackend::over(memory_backend().await.into())
        .effect_host(|inner| Arc::new(FailOnceRetirementHost::over(inner)));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    drop(core.session("delete-retry").open().await?);
    let administration = core.session_administration().await;

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

/// FIG-1559: the handle reports the relation the store recorded, and a rebind
/// that renames the parent is a typed refusal rather than silent absorption.
#[tokio::test]
async fn parent_relation_is_read_back_and_a_conflicting_rebind_is_refused() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        memory_backend().await.into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    let child = core
        .session("relation-child")
        .parent("relation-parent")
        .open()
        .await?;
    assert_eq!(child.parent_session_id(), Some("relation-parent"));
    drop(child);

    // A reopen that names no parent still reports the recorded relation: the
    // handle reads the durable fact, not the request it was built from.
    let reopened = core.session("relation-child").open().await?;
    assert_eq!(reopened.parent_session_id(), Some("relation-parent"));
    drop(reopened);

    let error = match core
        .session("relation-child")
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
    let after = core.session("relation-child").open().await?;
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

    let backend = memory_backend().await;

    let source_registry = backend.process_registry();
    let receiving_backend = memory_backend().await;
    let receiving_registry = receiving_backend.process_registry();

    let source = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone().into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(text_provider(
        "owner-services-provider",
        "owner-services-model",
        "source-provider",
    ))
    .model(model_spec("owner-services-model", None, 200_000))
    .build(owner.clone())?;
    let receiving = explicit_ephemeral_facets(LashCore::standard_builder(
        receiving_backend.clone().into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(text_provider(
        "owner-services-provider",
        "owner-services-model",
        "receiving-provider",
    ))
    .model(model_spec("owner-services-model", None, 200_000))
    .build(owner)?;

    let session = source.session(session_id).open().await?;
    let process_id = source_registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::session(session.observe().process_scope()),
                lash_core::Lifetime::Detached,
            ),
            &[lash_core::SessionId::from(session_id)],
        )
        .await?
        .id;
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
