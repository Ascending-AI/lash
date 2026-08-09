//! Backend-neutral lineage derivation for zero-copy forks.

use std::collections::BTreeMap;

use super::StoreError;

/// Immutable edge and ownership facts for one node on a retained fork path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkNodeFacts {
    pub node_id: String,
    pub parent_node_id: Option<String>,
    pub owning_session_id: String,
    pub generation: u64,
}

/// One per-ancestor read ceiling inherited by a zero-copy fork.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkLineageAncestor {
    pub ancestor_session_id: String,
    pub fork_node_id: String,
    pub fork_generation: u64,
}

/// Core-owned durable lineage prescription for a zero-copy fork.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkPlan {
    session_id: String,
    ancestors: Vec<ForkLineageAncestor>,
}

impl ForkPlan {
    /// Derive the complete inherited ceiling set from the retained parent path.
    ///
    /// `edge_path` is root-to-fork-node order. The edge chain is the authority:
    /// every owning session receives the greatest generation it owns on that
    /// path, regardless of whether the owner's head or any descendant lineage
    /// carrier still exists.
    pub fn derive(
        session_id: &str,
        edge_path: impl IntoIterator<Item = ForkNodeFacts>,
    ) -> Result<Self, StoreError> {
        let mut ancestors = BTreeMap::new();
        let mut prior_node_id = None;
        let mut expected_generation = 0_u64;
        let mut saw_node = false;

        for node in edge_path {
            saw_node = true;
            if node.generation != expected_generation || node.parent_node_id != prior_node_id {
                return Err(StoreError::StoredDataCorrupt {
                    record_kind: "SessionGraph",
                    message: format!(
                        "generation/parent gap at `{}`: generation {}, expected {}",
                        node.node_id, node.generation, expected_generation
                    ),
                });
            }
            prior_node_id = Some(node.node_id.clone());
            expected_generation = StoreError::checked_monotonic_increment(
                "fork_path_generation",
                expected_generation,
            )?;
            ancestors.insert(
                node.owning_session_id.clone(),
                ForkLineageAncestor {
                    ancestor_session_id: node.owning_session_id,
                    fork_node_id: node.node_id,
                    fork_generation: node.generation,
                },
            );
        }

        if !saw_node {
            return Err(StoreError::StoredDataCorrupt {
                record_kind: "SessionGraph",
                message: "retained fork path is empty".to_string(),
            });
        }

        Ok(Self {
            session_id: session_id.to_string(),
            ancestors: ancestors.into_values().collect(),
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn ancestors(&self) -> &[ForkLineageAncestor] {
        &self.ancestors
    }

    pub fn includes(&self, owning_session_id: &str, generation: u64) -> bool {
        self.ancestors.iter().any(|ancestor| {
            ancestor.ancestor_session_id == owning_session_id
                && generation <= ancestor.fork_generation
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(owner: &str, generation: u64, parent: Option<&str>) -> ForkNodeFacts {
        ForkNodeFacts {
            node_id: format!("{owner}-{generation}"),
            parent_node_id: parent.map(str::to_string),
            owning_session_id: owner.to_string(),
            generation,
        }
    }

    #[test]
    fn fork_plan_derives_maximum_generation_per_owner_from_edges() {
        let plan = ForkPlan::derive(
            "child",
            [
                node("a", 0, None),
                node("a", 1, Some("a-0")),
                node("b", 2, Some("a-1")),
                node("b", 3, Some("b-2")),
                node("c", 4, Some("b-3")),
            ],
        )
        .expect("valid retained edge path");

        assert_eq!(plan.session_id(), "child");
        assert_eq!(
            plan.ancestors(),
            &[
                ForkLineageAncestor {
                    ancestor_session_id: "a".to_string(),
                    fork_node_id: "a-1".to_string(),
                    fork_generation: 1,
                },
                ForkLineageAncestor {
                    ancestor_session_id: "b".to_string(),
                    fork_node_id: "b-3".to_string(),
                    fork_generation: 3,
                },
                ForkLineageAncestor {
                    ancestor_session_id: "c".to_string(),
                    fork_node_id: "c-4".to_string(),
                    fork_generation: 4,
                },
            ]
        );
        assert!(plan.includes("a", 0));
        assert!(!plan.includes("a", 2));
    }

    #[test]
    fn fork_plan_rejects_a_non_edge_path() {
        let error = ForkPlan::derive("child", [node("a", 0, None), node("b", 2, Some("a-0"))])
            .expect_err("generation gap must be corruption");
        assert!(matches!(error, StoreError::StoredDataCorrupt { .. }));
    }
}
