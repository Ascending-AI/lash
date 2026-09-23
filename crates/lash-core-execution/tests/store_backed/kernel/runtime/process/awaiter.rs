mod tests {
    use std::sync::Arc;

    use crate::facade_support::{ProcessEventSink, watch_process_registry};

    use crate::support::memory_backend;

    struct TestSink;

    #[async_trait::async_trait]
    impl ProcessEventSink for TestSink {
        async fn emit(&self, _: &crate::ProcessEvent) {}
    }

    #[tokio::test]
    async fn registration_detaches_its_sink_on_drop() {
        let backend = memory_backend().await;
        let watched = watch_process_registry(backend.process_registry());
        let registration = watched.add_event_sink(Arc::new(TestSink));
        assert_eq!(watched.event_sink_count_for_testing(), 1);
        drop(registration);
        assert_eq!(watched.event_sink_count_for_testing(), 0);
    }
}
