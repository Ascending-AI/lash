use super::*;

/// Capture the sender before an operation: retarget appends against its old
/// target, while pruning must preserve floors independently of process rows.
pub(super) struct EventSequenceStep {
    prior: Vec<(ProcessId, Option<SessionId>, u64)>,
    floors: BTreeMap<ProcessId, u64>,
}

impl EventSequenceStep {
    pub(super) fn capture(model: &ReferenceModel) -> Self {
        let prior = model
            .processes
            .iter()
            .filter_map(|(id, process)| {
                process.expected().map(|record| {
                    (
                        id.clone(),
                        process.wake_target.clone(),
                        record.last_event_sequence,
                    )
                })
            })
            .collect::<Vec<_>>();
        let floors = prior
            .iter()
            .map(|(id, target, _)| {
                let floor = target
                    .as_ref()
                    .and_then(|target| {
                        model
                            .event_sequence_floors
                            .get(&(target.clone(), id.clone()))
                    })
                    .copied()
                    .unwrap_or(0);
                (id.clone(), floor)
            })
            .collect();
        Self { prior, floors }
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    pub(super) fn advance(&self, record: &mut ProcessRecord) {
        record.last_event_sequence = record
            .last_event_sequence
            .max(self.floors.get(&record.id).copied().unwrap_or(0))
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

    pub(super) fn finish(self, model: &mut ReferenceModel) {
        for (id, target, previous_sequence) in self.prior {
            if let Some(target) = target
                && let Some(record) = model
                    .processes
                    .get(&id)
                    .and_then(|process| process.expected())
                && record.last_event_sequence > previous_sequence
            {
                let floor = model.event_sequence_floors.entry((target, id)).or_default();
                *floor = (*floor).max(record.last_event_sequence);
            }
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
        let handles = StoreContractHandles {
            registry: backend.process_registry() as Arc<dyn crate::ProcessRegistry>,
            runtime: Arc::new(backend.open_store().await.expect("durable-core store"))
                as Arc<dyn crate::RuntimePersistence>,
        };
        (backend, handles)
    }

    #[tokio::test]
    async fn first_event_after_process_reuse_jumps_past_retained_sender_floor() {
        let (_backend, handles) = memory_handles().await;
        let registry = Arc::clone(&handles.registry);
        let mut scenario = StoreContractScenario::new(handles);
        let register = StoreContractOp::Register {
            process: 0,
            disposition: 0,
            max_attempts: 1,
            wake_target: Some(0),
        };
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
        let id = process_id(0);
        let retained = registry
            .get_process(&id)
            .await
            .unwrap()
            .unwrap()
            .last_event_sequence;
        assert!(retained > 0);
        scenario
            .apply(&StoreContractOp::Prune { watermark: false })
            .await
            .unwrap();
        assert!(matches!(
            registry.get_process(&id).await,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ));
        scenario.apply(&register).await.unwrap();
        assert_eq!(
            registry
                .get_process(&id)
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
            .get_process(&id)
            .await
            .unwrap()
            .unwrap()
            .last_event_sequence;
        assert_eq!(actual, retained + 1);
        assert_eq!(
            scenario.model.processes[&id]
                .expected()
                .unwrap()
                .last_event_sequence,
            actual
        );
    }
    #[tokio::test]
    async fn prune_removes_settled_process_wakes_but_preserves_floor() {
        let (_backend, handles) = memory_handles().await;
        replay_case(
            handles,
            &[
                StoreContractOp::Register {
                    process: 1,
                    disposition: 0,
                    max_attempts: 1,
                    wake_target: Some(0),
                },
                StoreContractOp::Terminal {
                    process: 1,
                    disposition: 0,
                },
                StoreContractOp::Signal {
                    process: 1,
                    replay: 0,
                    value: 0,
                    wake: true,
                    stale: false,
                },
                StoreContractOp::ClaimWake,
                StoreContractOp::MarkWake { stale: false },
                StoreContractOp::Prune { watermark: false },
                StoreContractOp::Register {
                    process: 1,
                    disposition: 0,
                    max_attempts: 1,
                    wake_target: Some(0),
                },
                StoreContractOp::SetExternalRef {
                    process: 1,
                    value: 0,
                },
            ],
        )
        .await
        .expect("prune cascades deliveries while reuse retains its floor");
    }
    #[tokio::test]
    async fn repeated_observer_and_retarget_operations_replay_their_audit_events() {
        let (_backend, handles) = memory_handles().await;
        replay_case(
            handles,
            &[
                StoreContractOp::Register {
                    process: 2,
                    disposition: 0,
                    max_attempts: 1,
                    wake_target: None,
                },
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
                StoreContractOp::Retarget {
                    process: 2,
                    session: Some(0),
                },
                StoreContractOp::Retarget {
                    process: 2,
                    session: Some(1),
                },
                StoreContractOp::Retarget {
                    process: 2,
                    session: Some(0),
                },
            ],
        )
        .await
        .expect("repeated lifecycle intents do not allocate fresh audit events");
    }
}
