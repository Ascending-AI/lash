use super::*;
use lash_sansio::SessionId;

/// Agreement alone misses a missing-root defect shared by every backend.
/// Apply the same independent byte-survival and rollback oracle to all three.
#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn cross_owner_attachment_adoption(
    sqlite_root: &Path,
    postgres: &PostgresStorage,
) {
    let memory = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("SQLite memory backend");
    let factories: [Arc<dyn SessionStoreFactory>; 3] = [
        memory.session_store_factory(),
        Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            sqlite_root.join("cross-owner"),
        )),
        Arc::new(postgres.session_store_factory()),
    ];
    for (index, factory) in factories.into_iter().enumerate() {
        // Each backend's laws put bytes through fresh filesystem stores.
        let bytes_root = sqlite_root.join(format!("cross-owner-bytes-{index}"));
        let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let make_bytes: lash_conformance::AttachmentBytesFactory = Arc::new(move || {
            let ordinal = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                bytes_root.join(ordinal.to_string()),
            )) as Arc<dyn lash_core::AttachmentStore>
        });
        Box::pin(
            lash_conformance::cross_owner_attachment_adoption_conformance(factory, make_bytes),
        )
        .await;
    }
}

pub(super) fn fence_precedence_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::ForkFencePrecedence,
        // The target session already exists while the node is deliberately
        // absent. The exists fence must win over retained/live/frame fences
        // on every backend.
        operations: vec![StoreOperation::ForkAtExistingTarget],
    }
}

pub(super) fn foreign_lineage_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::ForeignLineageFork,
        operations: vec![
            StoreOperation::Commit {
                label: "commit_foreign_lineage_forkable_leaf",
                expected_head_revision: 0,
                graph: append(
                    vec![NodeSpec::new("active-frame", None, "foreign-lineage")],
                    Some("active-frame"),
                ),
                turn_commit: Some(TurnCommitSpec {
                    turn_id: "foreign-lineage-fork",
                }),
                checkpoint: CheckpointSpec::Empty,
                usage: false,
                adopt_attachment: false,
            },
            StoreOperation::PinLeaf,
            StoreOperation::ForkAtForeignLineage,
            StoreOperation::UnpinLeaf,
        ],
    }
}

pub(super) fn rewind_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::Rewind,
        operations: vec![
            StoreOperation::Commit {
                label: "commit_rewind_forkable_leaf",
                expected_head_revision: 0,
                graph: append(
                    vec![NodeSpec::new("active-frame", None, "rewind")],
                    Some("active-frame"),
                ),
                turn_commit: Some(TurnCommitSpec {
                    turn_id: "rewind-fork",
                }),
                checkpoint: CheckpointSpec::Empty,
                usage: false,
                adopt_attachment: false,
            },
            StoreOperation::PinLeaf,
            StoreOperation::Rewind,
        ],
    }
}

impl BackendRunner {
    #[expect(
        clippy::unwrap_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    pub(super) async fn reclaim_terminal_evidence(
        &self,
    ) -> Result<Option<ComparableRuntimeCommitResult>, StoreError> {
        let bound = lash_core::RetentionBound {
            committed_before_epoch_ms: u64::MAX,
        };
        let report = self
            .factory()
            .reclaim_retained_evidence(bound)
            .await
            .map_err(|failure| StoreError::Backend(failure.to_string()))?;
        assert_eq!(report.removed_receipt_count, 1);
        assert_eq!(report.removed_usage_delta_count, 1);
        assert_eq!(report.removed_attachment_root_count, 0);
        assert_eq!(
            self.factory()
                .reclaim_retained_evidence(bound)
                .await
                .unwrap(),
            lash_core::RetentionReport::default()
        );
        assert!(
            self.factory()
                .live_attachment_refs(0)
                .await?
                .contains(&differential_attachment_id())
        );
        Ok(None)
    }

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    pub(super) async fn apply_fork_operation(
        &mut self,
        operation: &StoreOperation,
    ) -> Result<Option<ComparableRuntimeCommitResult>, StoreError> {
        match operation {
            StoreOperation::ForkAtExistingTarget => {
                let error = self
                    .factory()
                    .fork_at(&ForkSessionRequest {
                        pending_observer_intents: Vec::new(),
                        session_id: self.session_id.clone(),
                        node_id: format!("{}:missing-fork-node", self.session_id).into(),
                        relation: SessionRelation::Root,
                        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
                    })
                    .await
                    .expect_err("existing fork target must be rejected");
                assert!(
                    matches!(&error, StoreError::ForkSessionAlreadyExists { .. }),
                    "{} must run the exists fence before retained/live/frame fences; got {error}",
                    self.name
                );
                Err(error)
            }
            StoreOperation::ForkAtForeignLineage => {
                let node_id = self
                    .current_leaf_node_id
                    .clone()
                    .expect("generated sequence committed a leaf before foreign-lineage fork");
                let result = self
                    .factory()
                    .fork_at(&ForkSessionRequest {
                        pending_observer_intents: Vec::new(),
                        session_id: SessionId::from(format!("{}:foreign-lineage", self.session_id)),
                        node_id: node_id.clone().into(),
                        relation: SessionRelation::Fork {
                            source_session_id: SessionId::from(format!(
                                "{}:foreign-source",
                                self.session_id
                            )),
                            source_node_id: format!("{}:foreign-node", self.session_id).into(),
                        },
                        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
                    })
                    .await
                    .expect("foreign lineage must not gate a retained fork point");
                assert_eq!(
                    result.source_session_id, self.session_id,
                    "fork result must report retained-anchor provenance"
                );
                Ok(None)
            }
            StoreOperation::Rewind => {
                let attachment_rooted = self
                    .factory()
                    .live_attachment_refs(0)
                    .await?
                    .contains(&differential_attachment_id());
                let node_id = self
                    .current_leaf_node_id
                    .clone()
                    .expect("generated sequence committed a leaf before rewind");
                let branch_session_id =
                    SessionId::from(format!("{}:rewind-branch", self.session_id));
                let branch = self
                    .factory()
                    .fork_at(&ForkSessionRequest {
                        pending_observer_intents: Vec::new(),
                        session_id: branch_session_id.clone(),
                        node_id: node_id.clone().into(),
                        relation: SessionRelation::Fork {
                            source_session_id: self.session_id.clone(),
                            source_node_id: node_id.clone().into(),
                        },
                        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
                    })
                    .await
                    .expect("rewind must create its first branch");
                assert_eq!(
                    branch.source_session_id, self.session_id,
                    "first rewind fork must report retained-anchor provenance"
                );
                self.factory()
                    .delete_session(&self.session_id)
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                if attachment_rooted {
                    assert!(
                        self.factory()
                            .live_attachment_refs(0)
                            .await?
                            .contains(&differential_attachment_id()),
                        "FIG-2501: surviving fork retains the deleted parent's attachment root"
                    );
                }
                let rewound = self
                    .factory()
                    .fork_at(&ForkSessionRequest {
                        pending_observer_intents: Vec::new(),
                        session_id: SessionId::from(format!("{}:rewind", self.session_id)),
                        node_id: node_id.into(),
                        relation: SessionRelation::Fork {
                            source_session_id: branch_session_id,
                            source_node_id: format!("{}:rewind-source-node", self.session_id)
                                .into(),
                        },
                        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
                    })
                    .await
                    .expect("rewind must re-fork after deleting the superseded source");
                assert_eq!(
                    rewound.source_session_id, self.session_id,
                    "re-fork must report retained-anchor provenance, not relation lineage"
                );
                Ok(None)
            }
            _ => unreachable!("fork helper received non-fork operation"),
        }
    }
}

/// PG reuses its catalog across cases and runs; remove earlier terminal evidence
/// before this case commits so the literal count oracle covers this case alone.
#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn prepare_retention_case(case: CaseName, runners: &[BackendRunner]) {
    if case == CaseName::AttachmentAdoption {
        for runner in runners {
            runner
                .factory()
                .reclaim_retained_evidence(lash_core::RetentionBound {
                    committed_before_epoch_ms: u64::MAX,
                })
                .await
                .expect("clear prior terminal evidence before the retention fixture");
        }
    }
}

/// Pin a leaf, fork at it, then unpin: the node anchor must move with the fork.
/// Declared here beside the other fork shapes so the parent file stays inside
/// the test file-size budget.
pub(super) fn pin_fork_unpin() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::PinForkUnpin,
        operations: vec![
            StoreOperation::Commit {
                label: "commit_forkable_leaf",
                expected_head_revision: 0,
                graph: append(
                    vec![NodeSpec::new("active-frame", None, "forkable")],
                    Some("active-frame"),
                ),
                turn_commit: Some(TurnCommitSpec {
                    turn_id: "forkable-leaf",
                }),
                checkpoint: CheckpointSpec::Empty,
                usage: false,
                adopt_attachment: false,
            },
            StoreOperation::PinLeaf,
            StoreOperation::ForkAtLeaf,
            StoreOperation::UnpinLeaf,
        ],
    }
}

/// A fork's durable intents keep the host-selected incarnation across a crash
/// and process-name reuse. Exercise real registry fencing on every backend.
#[expect(
    clippy::expect_used,
    reason = "test fixtures require successful setup and report explicit assertion failures"
)]
pub(super) async fn selected_observer_intents(
    sqlite_root: &Path,
    postgres: &PostgresStorage,
    nonce: &str,
) {
    use lash_core::ProcessEventLogTestSupport as _;
    let root = sqlite_root.join("selected-observer-sessions");
    let path = sqlite_root.join("selected-observer-processes.db");
    let sqlite = lash_sqlite_store::SqliteProcessRegistry::open(&path, &root)
        .await
        .expect("SQLite observer registry");
    let memory = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("SQLite memory observer backend");
    let backends: Vec<(
        Arc<dyn SessionStoreFactory>,
        Arc<dyn lash_core::ProcessRegistry>,
    )> = vec![
        (memory.session_store_factory(), memory.process_registry()),
        (
            Arc::new(
                lash_sqlite_store::SqliteSessionStoreFactory::new_with_process_registry(root, path),
            ),
            Arc::new(sqlite),
        ),
        (
            Arc::new(postgres.session_store_factory()),
            Arc::new(postgres.process_registry()),
        ),
    ];
    for (index, (factory, registry)) in backends.into_iter().enumerate() {
        let session_id = SessionId::from(format!("selected-observer-{nonce}-{index}"));
        let registration = lash_core::ProcessRegistration::new(
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::ExternallyOwned,
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        );
        let first = registry
            .register_process(registration.clone())
            .await
            .expect("selected run");
        let selected = first.id.clone();
        let intent =
            lash_core::facade_support::SessionObserverIntent::host_requested(selected.clone());
        let request = SessionStoreCreateRequest {
            session_id: session_id.clone(),
            relation: SessionRelation::Fork {
                source_session_id: "host-selected-lineage".into(),
                source_node_id: "foreign-history-provenance".into(),
            },
            pending_observer_intents: vec![intent.clone()],
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        };
        let source_id = SessionId::from(format!("selected-history-{nonce}-{index}"));
        let source_request = SessionStoreCreateRequest {
            session_id: source_id.clone(),
            relation: SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            policy: request.policy.clone(),
        };
        let source = factory
            .create_store(&source_request)
            .await
            .expect("history source");
        let mut state = RuntimeSessionState::new(request.policy.clone());
        state.session_id = source_id.clone();
        state.ensure_agent_frame_initialized();
        source
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
            .await
            .expect("commit fork point");
        let node_id = state
            .session_graph
            .leaf_node_id
            .clone()
            .expect("forkable node");
        factory.pin(&node_id).await.expect("retain writer history");
        factory
            .delete_session(&source_id)
            .await
            .expect("delete original writer");
        let receipt = factory
            .fork_at(&ForkSessionRequest {
                session_id: session_id.clone(),
                node_id,
                relation: request.relation.clone(),
                pending_observer_intents: request.pending_observer_intents.clone(),
                policy: request.policy.clone(),
            })
            .await
            .expect("fork deleted-writer history with exact intent");
        assert_eq!(receipt.source_session_id, source_id);
        let store = factory
            .open_existing_store(&request)
            .await
            .expect("reopen interrupted fork")
            .expect("fork retained");
        assert_eq!(
            store
                .load_session_meta()
                .await
                .expect("load exact intent")
                .expect("metadata")
                .pending_observer_intents,
            vec![intent]
        );
        // The first apply happened, but a crash prevented consuming its intent.
        registry
            .add_observer(
                &session_id,
                &selected,
                lash_core::ProcessObserverBy::host(format!("session-create:{session_id}")),
            )
            .await
            .expect("first observer publication");
        let receipts = lash_core::runtime::reconcile_session_process_observer_intents(
            Some(registry.as_ref()),
            &session_id,
            lash_core::runtime::SessionObserverIntentSource::Persisted(store.as_ref()),
        )
        .await
        .expect("reconcile interrupted publication");
        assert!(matches!(
            receipts[0].outcome,
            lash_core::plugin::SessionObservedProcessOutcome::Observed
        ));
        let events = registry
            .full_event_window(&first.id, 0)
            .await
            .expect("observer event window");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "process.observer_added")
                .count(),
            1
        );
        // Old observer authors remain opaque historical audit data, not a
        // selector that can attach or redirect a live edge.
        let mut historical = events
            .iter()
            .find(|event| event.event_type == "process.observer_added")
            .expect("observer audit event")
            .clone();
        historical.payload["by"] = serde_json::json!({"kind": "fork_inheritance"});
        let historical: lash_core::ProcessEvent = serde_json::from_slice(
            &serde_json::to_vec(&historical).expect("encode historical audit event"),
        )
        .expect("hydrate opaque historical author");
        let mut projected = first.clone();
        lash_core::runtime::apply_process_event_projection(&mut projected, &historical)
            .expect("historical observer author has no lifecycle projection");
        assert_eq!(projected.status, first.status);
        assert_eq!(historical.payload["by"]["kind"], "fork_inheritance");
        let mut meta = store
            .load_session_meta()
            .await
            .expect("load consumed intent")
            .expect("metadata");
        assert!(meta.pending_observer_intents.is_empty());
        // A second pending selection outlives the process it selected.
        meta.pending_observer_intents = vec![
            lash_core::facade_support::SessionObserverIntent::host_requested(selected.clone()),
        ];
        store
            .save_session_meta(meta)
            .await
            .expect("persist selection before reuse");
        let terminal = registry
            .complete_process(
                &first.id,
                lash_core::ProcessAwaitOutput::from_tool_output(
                    lash_core::ToolCallOutput::success(serde_json::Value::Null),
                ),
                lash_core::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("finish selected run");
        registry
            .prune_terminal_processes(
                terminal.updated_at_ms.saturating_add(1),
                None,
                lash_core::ProjectionWatermark::NoProjector,
            )
            .await
            .expect("prune selected run");
        let newer = registry
            .register_process(registration)
            .await
            .expect("start a new run from the same registration");
        assert_ne!(newer.id, selected, "a new run is minted a new id");
        let receipts = lash_core::runtime::reconcile_session_process_observer_intents(
            Some(registry.as_ref()),
            &session_id,
            lash_core::runtime::SessionObserverIntentSource::Persisted(store.as_ref()),
        )
        .await
        .expect("settle stale exact selection");
        assert!(
            matches!(
                receipts[0].outcome,
                lash_core::plugin::SessionObservedProcessOutcome::NoLongerRetained { .. }
            ),
            "a selection of a pruned run settles as no longer retained: {:?}",
            receipts[0].outcome
        );
        assert!(
            !registry
                .is_observer(&session_id, &newer.id)
                .await
                .expect("new run observer check")
        );
    }
}
