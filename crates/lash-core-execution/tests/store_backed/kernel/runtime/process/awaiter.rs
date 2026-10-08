mod tests {
    use std::sync::Arc;

    use crate::facade_support::{ProcessEventSink, watch_process_registry};

    use crate::support::sqlite_memory_process_store_set;

    struct TestSink;

    #[async_trait::async_trait]
    impl ProcessEventSink for TestSink {
        async fn emit(&self, _: &crate::ProcessEvent) {}
    }

    #[tokio::test]
    async fn registration_detaches_its_sink_on_drop() {
        let backend = sqlite_memory_process_store_set().await;
        let watched = watch_process_registry(backend.process_registry());
        let registration = watched.add_event_sink(Arc::new(TestSink));
        assert_eq!(watched.event_sink_count_for_testing(), 1);
        drop(registration);
        assert_eq!(watched.event_sink_count_for_testing(), 0);
    }

    /// FIG-5531: best-effort process publication distinguishes a store fault
    /// from absence, and bounds repeated diagnostics until the read recovers.
    #[tokio::test]
    async fn process_feed_cursor_fault_reports_one_degradation_and_absence_reports_none() {
        use crate::testing::{ProcessRegistryFaults, trace_capture::capturing};
        use crate::{ProcessExecutionWriteAuthority, ProcessProvenance, ProcessRegistrar};
        let backend = sqlite_memory_process_store_set().await;
        let faults = Arc::new(ProcessRegistryFaults::new(backend.process_registry()));
        let record = faults
            .register_process(crate::testing::held_engine_registration(
                serde_json::json!({}),
                ProcessProvenance::host(),
                crate::Lifetime::Detached,
            ))
            .await
            .expect("registered process");
        let watched = watch_process_registry(faults.clone());
        let _registration = watched.add_event_sink(Arc::new(TestSink));
        let authority = ProcessExecutionWriteAuthority::invocation(record.id.clone(), "feed-law");
        let (_, absent) = capturing(|| async {
            faults.set_process_read_absent(true);
            watched
                .registry()
                .append_events(&record.id, Vec::new(), &authority)
                .await
                .expect("empty append");
        })
        .await;
        assert!(absent.named("process_feed.degraded").is_empty());
        faults.set_process_read_absent(false);
        let (_, failed) = capturing(|| async {
            faults.set_process_read_error(Some(crate::PluginError::from(
                crate::store::StoreFault::Contended,
            )));
            for _ in 0..3 {
                watched
                    .registry()
                    .append_events(&record.id, Vec::new(), &authority)
                    .await
                    .expect("cursor failure does not alter append");
            }
        })
        .await;
        let diagnostic = failed.exactly_one("process_feed.degraded");
        assert_eq!(diagnostic.level, "WARN");
        assert_eq!(diagnostic.field("process_id"), record.id.as_str());
        assert_eq!(diagnostic.field("error_type"), "Unavailable");
        assert_eq!(diagnostic.field("operation"), "process_cursor");
        let (_, recovered) = capturing(|| async {
            faults.set_process_read_error(None);
            watched
                .registry()
                .append_events(&record.id, Vec::new(), &authority)
                .await
                .expect("recovered append");
        })
        .await;
        assert_eq!(
            recovered
                .exactly_one("process_feed.recovered")
                .field("failure_count"),
            "3"
        );
    }
}
