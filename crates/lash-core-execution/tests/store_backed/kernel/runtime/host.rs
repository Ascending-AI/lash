mod tests {
    use crate::runtime::host::RuntimeHostConfig;

    #[tokio::test]
    async fn attachment_limit_defaults_unbounded_and_accepts_host_override() {
        let backend = crate::support::memory_backend().await;
        let unbounded = RuntimeHostConfig::new(
            backend.effect_host(),
            crate::Backend::attachment_store(&backend),
            backend.process_env_store(),
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
        assert_eq!(
            unbounded.durability.attachment_store.max_attachment_bytes(),
            None
        );

        let bounded = unbounded.with_max_attachment_bytes(Some(4096));
        assert_eq!(
            bounded.durability.attachment_store.max_attachment_bytes(),
            Some(4096)
        );
    }
}
