use super::*;

/// The next event sequence of a process is one past its last; a replayed
/// lifecycle intent allocates none.
pub(super) struct EventSequenceStep;

impl EventSequenceStep {
    pub(super) fn capture(_model: &ReferenceModel) -> Self {
        Self
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    pub(super) fn advance(&self, record: &mut ProcessRecord) {
        record.last_event_sequence = record
            .last_event_sequence
            .checked_add(1)
            .expect("generated process sequence remains in range");
    }

    pub(super) fn advance_lifecycle(
        &self,
        process: &mut ModelProcess,
        request: ProcessEventAppendRequest,
    ) {
        // Repeating an observer/retarget operation can change its index again,
        // while replaying the same audit event instead of allocating a new one.
        if let Some(replay) = request.replay
            && !process.lifecycle_replay_keys.insert(replay.key)
        {
            return;
        }
        if let Some(record) = process.expected_mut() {
            self.advance(record);
        }
    }
}

#[cfg(test)]
mod floor_tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// The store-contract handles over a fresh SQLite memory store set, which
    /// the caller keeps alive for the case.
    async fn memory_handles() -> (lash_sqlite_store::SqliteStoreSet, StoreContractHandles) {
        let backend = lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("memory backend");
        lash_core::testing::process_execution_env_fixture(backend.process_env_store().as_ref())
            .await;
        let handles = StoreContractHandles {
            registry: backend.process_registry() as Arc<dyn crate::ProcessRegistry>,
            runtime: backend.open_store().await.expect("durable-core store")
                as Arc<dyn crate::RuntimeStore>,
        };
        (backend, handles)
    }

    /// A slot re-registered after its run was pruned starts a new process:
    /// a new id whose first event is its own first (ADR 0107).
    #[tokio::test]
    async fn a_restart_after_prune_starts_a_new_process_at_its_own_first_event() {
        let (_backend, handles) = memory_handles().await;
        let registry = Arc::clone(&handles.registry);
        let mut scenario = StoreContractScenario::new(handles);
        let register = StoreContractOp::Register { process: 0 };
        for operation in [
            register.clone(),
            StoreContractOp::FirstStart {
                process: 0,
                owner: 0,
                attempt: 0,
            },
            StoreContractOp::Terminal {
                process: 0,
                disposition: 0,
            },
        ] {
            scenario.apply(&operation).await.expect("initial lifecycle");
        }
        let pruned = scenario.slot_process_id(0);
        assert!(
            registry
                .get_process(&pruned)
                .await
                .unwrap()
                .unwrap()
                .last_event_sequence
                > 0
        );
        scenario
            .apply(&StoreContractOp::Prune { watermark: false })
            .await
            .unwrap();
        assert!(matches!(
            registry.get_process(&pruned).await,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ));
        scenario.apply(&register).await.unwrap();
        let restarted = scenario.slot_process_id(0);
        assert_ne!(restarted, pruned, "the restart is minted a new id");
        assert_eq!(
            registry
                .get_process(&restarted)
                .await
                .unwrap()
                .unwrap()
                .last_event_sequence,
            0
        );
        scenario
            .apply(&StoreContractOp::SetExternalRef {
                process: 0,
                value: 0,
            })
            .await
            .unwrap();
        let actual = registry
            .get_process(&restarted)
            .await
            .unwrap()
            .unwrap()
            .last_event_sequence;
        assert_eq!(actual, 1, "the new process's first event is its own first");
        assert_eq!(
            scenario.model.processes[&restarted]
                .expected()
                .unwrap()
                .last_event_sequence,
            actual
        );
        assert!(matches!(
            registry.get_process(&pruned).await,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ));
    }
    #[tokio::test]
    async fn repeated_observer_operations_replay_their_audit_events() {
        let (_backend, handles) = memory_handles().await;
        replay_case(
            handles,
            &[
                StoreContractOp::Register { process: 2 },
                StoreContractOp::AddObserver {
                    process: 2,
                    session: 1,
                },
                StoreContractOp::RemoveObserver {
                    process: 2,
                    session: 1,
                },
                StoreContractOp::AddObserver {
                    process: 2,
                    session: 1,
                },
                StoreContractOp::RemoveObserver {
                    process: 2,
                    session: 1,
                },
            ],
        )
        .await
        .expect("repeated lifecycle intents do not allocate fresh audit events");
    }
}
