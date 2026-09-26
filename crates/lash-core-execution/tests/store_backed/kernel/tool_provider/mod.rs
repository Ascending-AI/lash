mod process;

mod tests {
    use std::sync::Arc;

    #[tokio::test]
    async fn process_events_target_supplies_enclosing_process_when_host_did_not() {
        let registry: Arc<dyn crate::ProcessRegistry> =
            crate::support::memory_store_set().await.process_registry();
        let context = crate::testing::mock_tool_context().with_process_events_for_testing(
            "process-2",
            registry,
            crate::ProcessExecutionWriteAuthority::invocation("process-2", "exec-1"),
        );
        assert_eq!(context.enclosing_process(), Some("process-2"));
    }

    #[tokio::test]
    #[should_panic(expected = "process_events target must equal the context's enclosing process")]
    async fn process_events_target_must_match_enclosing_process() {
        let registry: Arc<dyn crate::ProcessRegistry> =
            crate::support::memory_store_set().await.process_registry();
        let _ = crate::testing::mock_tool_context()
            .with_enclosing_process("process-a", tokio_util::sync::CancellationToken::new())
            .with_process_events_for_testing(
                "process-b",
                registry,
                crate::ProcessExecutionWriteAuthority::invocation("process-b", "exec-1"),
            );
    }
}
