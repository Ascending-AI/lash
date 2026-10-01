//! Deleting a session retires its effect journal through the backend's
//! effect host.

use super::*;

/// The backend's effect host, recording every journal retirement it is
/// asked for before carrying it out.
struct RetirementRecordingHost {
    inner: Arc<dyn lash_core::EffectHost>,
    retirements: Arc<std::sync::Mutex<Vec<lash_core::EffectJournalRetirement>>>,
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for RetirementRecordingHost {
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
impl lash_core::EffectHost for RetirementRecordingHost {
    async fn drain_usage_accounting(
        &self,
        owner: &lash_core::RuntimeOwner,
    ) -> std::result::Result<lash_core::UsageOwnerRetired, lash_core::RuntimeError> {
        self.inner.drain_usage_accounting(owner).await
    }

    async fn retire_usage_execution(
        &self,
        owner: &lash_core::RuntimeOwner,
        scope: &lash_core::ExecutionScope,
    ) -> std::result::Result<u64, lash_core::RuntimeError> {
        self.inner.retire_usage_execution(owner, scope).await
    }

    async fn journal_replay(
        &self,
        journal: &lash_sansio::EffectJournalIdentity,
    ) -> std::result::Result<lash_core::JournalReplay, lash_core::RuntimeError> {
        self.inner.journal_replay(journal).await
    }

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
        self.retirements.lock_recover().push(retirement.clone());
        self.inner.retire_effect_journal(retirement).await
    }
}

#[tokio::test]
async fn core_delete_session_retires_the_deleted_session_effect_journal() -> Result<()> {
    let retirements = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&retirements);
    let backend = DecoratedBackend::over(double_backend_explicit_reconcile().await).effect_host(
        move |inner| {
            Arc::new(RetirementRecordingHost {
                inner,
                retirements: recorded,
            })
        },
    );
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.into()))
        .serve_test_model(mock_provider(), mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    drop(
        core.session("retire-delete-session")
            .created()
            .await
            .open()
            .await?,
    );

    delete_bound_session(&core, "retire-delete-session").await?;

    assert_eq!(
        *retirements.lock_recover(),
        vec![lash_core::EffectJournalRetirement::session(
            "retire-delete-session"
        )]
    );
    Ok(())
}
