async fn inspect_session(
    core: &lash::LashCore,
    session_id: &str,
) -> lash::Result<Option<lash::persistence::SessionReadView>> {
    core.read_session(session_id).await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lash::persistence::SessionStoreFactory as _;

    use super::inspect_session;

    const SESSION_ID: &str = "docs-read-session";

    fn core(
        factory: Arc<lash::persistence::InMemorySessionStoreFactory>,
    ) -> lash::Result<lash::LashCore> {
        lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
            .provider(lash::provider::ProviderHandle::unconfigured())
            .model(
                lash::ModelSpec::builder("docs-read-session-model")
                    .context_window_tokens(4_096)
                    .build()
                    .expect("valid read-session model"),
            )
            .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
            .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
            .process_env_store(Arc::new(
                lash::persistence::InMemoryProcessExecutionEnvStore::new(),
            ))
            .store_factory(factory)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .disable_queued_work_driver()
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "docs-read-session-worker",
                "docs-read-session-worker:incarnation",
            ))
    }

    #[tokio::test]
    async fn inspection_reads_the_settled_view_without_opening_a_session() -> anyhow::Result<()> {
        let factory = Arc::new(lash::persistence::InMemorySessionStoreFactory::new());
        let policy = lash::runtime::SessionPolicy::new(lash::TurnBudget::Unbounded);
        let store = factory
            .create_store(&lash::persistence::SessionStoreCreateRequest {
                session_id: SESSION_ID.to_string(),
                relation: lash::persistence::SessionRelation::Root,
                policy: policy.clone(),
            })
            .await?;
        let mut state = lash::persistence::RuntimeSessionState {
            session_id: SESSION_ID.to_string(),
            ..lash::persistence::RuntimeSessionState::new(policy)
        };
        state.append_active_conversation_messages(&[lash::messages::Message {
            id: "docs-read-session-message".to_string(),
            role: lash::messages::MessageRole::User,
            parts: vec![lash::messages::Part::text(
                "docs-read-session-message.p0".to_string(),
                "inspect me".to_string(),
                None,
            )]
            .into(),
            origin: None,
        }]);
        store
            .commit_runtime_state(lash::persistence::RuntimeCommit::persisted_state_for_test(
                &state,
                &[],
            ))
            .await?;

        let view = inspect_session(&core(factory)?, SESSION_ID)
            .await?
            .expect("committed session has a read view");
        assert_eq!(view.session_id(), SESSION_ID);
        assert_eq!(view.messages().len(), 1);
        assert_eq!(view.message_tree().len(), 1);
        Ok(())
    }
}
