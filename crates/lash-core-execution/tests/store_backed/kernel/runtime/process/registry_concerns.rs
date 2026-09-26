mod concern_isolation_tests {
    use std::num::NonZeroUsize;
    use std::sync::Arc;

    use crate::runtime::process::ProcessLiveReferenceView;
    use crate::runtime::process::{
        ProcessChange, ProcessChangeCursor, ProcessInput, ProcessListFilter, ProcessObserverBy,
        ProcessProvenance, ProcessRecord, ProcessRegistration, ProcessSessionDeleteReport,
        ProcessWorklistCursor, ProcessWorklistPage,
    };
    use crate::{
        PluginError, ProcessId, ProcessObserverRegistry, ProcessQuery, ProcessRegistry, SessionId,
    };

    use crate::support::memory_store_set;

    /// The positive twin of the module's `compile_fail` witness: a decorator
    /// that composes only the observer concern (plus its declared
    /// [`ProcessQuery`] read dependency) over an inner registry, implementing
    /// nothing else — no leases, no wake outbox, no lifecycle, no retention.
    struct ObserverOnly {
        inner: Arc<dyn ProcessRegistry>,
    }

    #[async_trait::async_trait]
    impl ProcessQuery for ObserverOnly {
        async fn get_process_by_start_key(
            &self,
            start_key: &crate::StartKey,
        ) -> Result<Option<ProcessRecord>, PluginError> {
            self.inner.get_process_by_start_key(start_key).await
        }

        async fn get_process(
            &self,
            process_id: &ProcessId,
        ) -> Result<Option<ProcessRecord>, PluginError> {
            self.inner.get_process(process_id).await
        }
        async fn list_processes(
            &self,
            filter: &ProcessListFilter,
        ) -> Result<Vec<ProcessRecord>, PluginError> {
            self.inner.list_processes(filter).await
        }
        async fn processes_changed_since(
            &self,
            cursor: ProcessChangeCursor,
            limit: usize,
        ) -> Result<(Vec<ProcessChange>, ProcessChangeCursor), PluginError> {
            self.inner.processes_changed_since(cursor, limit).await
        }
        async fn list_non_terminal_page(
            &self,
            limit: NonZeroUsize,
            continuation: Option<ProcessWorklistCursor>,
        ) -> Result<ProcessWorklistPage, PluginError> {
            self.inner.list_non_terminal_page(limit, continuation).await
        }
        async fn live_reference_summary(
            &self,
        ) -> Result<Vec<ProcessLiveReferenceView>, PluginError> {
            self.inner.live_reference_summary().await
        }
        async fn count_non_terminal_processes(&self) -> Result<usize, PluginError> {
            self.inner.count_non_terminal_processes().await
        }
        async fn list_parked_processes(
            &self,
            query: &lash_core_execution::store::ProcessParkQuery,
        ) -> Result<Vec<ProcessRecord>, PluginError> {
            self.inner.list_parked_processes(query).await
        }
        async fn process_park_feed(
            &self,
            after: lash_core_execution::store::ParkFeedCursor,
            limit: NonZeroUsize,
        ) -> Result<
            lash_core_execution::store::ParkFeedPage<lash_core_execution::store::ProcessParkKey>,
            PluginError,
        > {
            self.inner.process_park_feed(after, limit).await
        }
        async fn summarize_parked_processes(
            &self,
        ) -> Result<lash_core_execution::store::ParkSummary, PluginError> {
            self.inner.summarize_parked_processes().await
        }
    }

    #[async_trait::async_trait]
    impl ProcessObserverRegistry for ObserverOnly {
        async fn add_observer(
            &self,
            session_id: &SessionId,
            process_id: &ProcessId,
            by: ProcessObserverBy,
        ) -> Result<(), PluginError> {
            self.inner.add_observer(session_id, process_id, by).await
        }
        async fn remove_observer(
            &self,
            session_id: &SessionId,
            process_id: &ProcessId,
            by: ProcessObserverBy,
        ) -> Result<(), PluginError> {
            self.inner.remove_observer(session_id, process_id, by).await
        }
        async fn transfer_observers(
            &self,
            from_session_id: &SessionId,
            to_session_id: &SessionId,
            process_ids: &[ProcessId],
            by: ProcessObserverBy,
        ) -> Result<(), PluginError> {
            self.inner
                .transfer_observers(from_session_id, to_session_id, process_ids, by)
                .await
        }
        async fn list_observed_by(
            &self,
            session_id: &SessionId,
            filter: &ProcessListFilter,
        ) -> Result<Vec<ProcessRecord>, PluginError> {
            self.inner.list_observed_by(session_id, filter).await
        }
        async fn observers_for_process(
            &self,
            process_id: &ProcessId,
        ) -> Result<Vec<SessionId>, PluginError> {
            self.inner.observers_for_process(process_id).await
        }
        async fn retarget_subscription(
            &self,
            process_id: &ProcessId,
            target: Option<&str>,
        ) -> Result<(), PluginError> {
            self.inner.retarget_subscription(process_id, target).await
        }
        async fn delete_session_process_state(
            &self,
            session_id: &SessionId,
        ) -> Result<ProcessSessionDeleteReport, PluginError> {
            self.inner.delete_session_process_state(session_id).await
        }
    }

    #[tokio::test]
    async fn an_observer_only_wrapper_composes_without_any_other_concern() {
        let backend = memory_store_set().await;
        let inner = backend.process_registry() as Arc<dyn ProcessRegistry>;
        let proc_observer_isolation_record = inner
            .register_process(ProcessRegistration::new(
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                ProcessProvenance::host(),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
                ),
            ))
            .await
            .expect("register");
        let wrapper = ObserverOnly {
            inner: Arc::clone(&inner),
        };

        wrapper
            .add_observer(
                &SessionId::from("session-a"),
                &proc_observer_isolation_record.id,
                ProcessObserverBy::host("op-observer-isolation"),
            )
            .await
            .expect("add observer through the observer-only wrapper");
        assert!(
            wrapper
                .is_observer(
                    &SessionId::from("session-a"),
                    &proc_observer_isolation_record.id
                )
                .await
                .expect("is_observer provided method resolves through ProcessQuery"),
            "observer edge added through the wrapper must be visible through it"
        );
        let observed = wrapper
            .list_observed_by(
                &SessionId::from("session-a"),
                &crate::ProcessListFilter {
                    status: crate::ProcessStatusFilter::Any,
                    ..Default::default()
                },
            )
            .await
            .expect("list observed");
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].id, proc_observer_isolation_record.id.clone());
    }
}
