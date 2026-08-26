use super::*;

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
                        node_id: format!("{}:missing-fork-node", self.session_id),
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
                        session_id: format!("{}:foreign-lineage", self.session_id),
                        node_id: node_id.clone(),
                        relation: SessionRelation::Fork {
                            source_session_id: format!("{}:foreign-source", self.session_id),
                            source_node_id: format!("{}:foreign-node", self.session_id),
                            observer_inheritance: lash_core::ObserverInheritance::None,
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
                let node_id = self
                    .current_leaf_node_id
                    .clone()
                    .expect("generated sequence committed a leaf before rewind");
                let branch_session_id = format!("{}:rewind-branch", self.session_id);
                let branch = self
                    .factory()
                    .fork_at(&ForkSessionRequest {
                        pending_observer_intents: Vec::new(),
                        session_id: branch_session_id.clone(),
                        node_id: node_id.clone(),
                        relation: SessionRelation::Fork {
                            source_session_id: self.session_id.clone(),
                            source_node_id: node_id.clone(),
                            observer_inheritance: lash_core::ObserverInheritance::None,
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
                let rewound = self
                    .factory()
                    .fork_at(&ForkSessionRequest {
                        pending_observer_intents: Vec::new(),
                        session_id: format!("{}:rewind", self.session_id),
                        node_id,
                        relation: SessionRelation::Fork {
                            source_session_id: branch_session_id,
                            source_node_id: format!("{}:rewind-source-node", self.session_id),
                            observer_inheritance: lash_core::ObserverInheritance::None,
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
