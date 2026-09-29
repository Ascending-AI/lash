//! Edge authority for history anchors (ADR 0057 §"Edge authority").
//!
//! A history read, the active-ancestor predicate and the commit fence each
//! take a node the caller names and answer about it only when the requesting
//! session's head leaf reaches that node through parent edges. The
//! `fork_lineage` ceilings select candidate rows through an index; they are an
//! accelerator that may narrow what a session reads and never widen it. This
//! module is the one place that decides reachability, so SQLite and
//! PostgreSQL answer the same question the same way; each backend supplies
//! only the rows.
//!
//! # Why one hop per owning session is enough
//!
//! Three facts about `graph_nodes`, none of which lineage rows can change:
//!
//! 1. A node's parent, owner (`session_id`) and generation are written once,
//!    in the same insert, and never rewritten. A non-root node's generation is
//!    its parent's plus one.
//! 2. `UNIQUE (session_id, generation)`: one owner holds at most one node per
//!    generation.
//! 3. An owner's nodes are appended only from its own head leaf (the commit
//!    planner refuses any other first parent), and a head leaf moves only by
//!    such an append, or is set once when a fork creates the session. So the
//!    first node an owner appends has a foreign parent (or none), and every
//!    later one has the owner's previous node as its parent.
//!
//! From (1) to (3), an owner's nodes form one parent chain over contiguous
//! generations whose lowest node's parent, if any, belongs to another owner.
//! Walking parent edges down from any node `e` of owner `s` therefore visits
//! every node of `s` at or below `e`'s generation, then leaves `s` at the
//! parent of `s`'s lowest node and never returns to `s`.
//!
//! The probe walks exactly that: it starts at the head leaf, and while the
//! candidate is not decided it jumps from the current owner's entry node to
//! the parent of that owner's lowest node, one indexed lookup per owner. The
//! candidate `c` of owner `o` at generation `g` is reached iff the walk enters
//! `o` at a node whose generation is at least `g`: then `c` is `o`'s unique
//! node at `g` (fact 2), which the chain passes through. Generations strictly
//! decrease at every hop, so the walk ends. Nothing it reads comes from
//! `fork_lineage` or the head's frame pointer.
//!
//! A row that contradicts the chain shape (an owner's lowest node above the
//! node the walk entered it at, a parent in the same owner, a parent whose
//! generation is not one lower, or a missing or retired parent under a live
//! path) is stored-data corruption, not an unreadable candidate, per
//! ADR 0024's ruling that a live path cannot be partly reclaimed.

use crate::store::StoreError;
use crate::{NodeId, SessionId};

/// The facts of one graph node the probe reads: its id, owner and
/// generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathNode {
    pub node_id: NodeId,
    pub owner_session_id: SessionId,
    pub generation: u64,
}

/// Where the walk leaves one owner: that owner's lowest-generation node, and
/// the node its parent edge names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerExit {
    /// The owner's lowest-generation row. `None` when the owner holds no row
    /// at all, which contradicts the node the walk entered it at.
    pub lowest: Option<OwnerLowestNode>,
}

/// An owner's lowest-generation node and its parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerLowestNode {
    pub node_id: NodeId,
    pub generation: u64,
    pub parent: OwnerExitParent,
}

/// The parent edge of an owner's lowest node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnerExitParent {
    /// The lowest node is a root: the walk ends here.
    Root,
    /// The parent row, and whether it is retired.
    Node { node: PathNode, tombstoned: bool },
    /// The parent edge names a row that is not stored.
    Missing { node_id: NodeId },
}

/// One question: does the head leaf reach the candidate through parent
/// edges? Drive it with [`Self::verdict`] and [`Self::descend`]:
///
/// ```ignore
/// let mut probe = HeadPathProbe::new(candidate, head_leaf);
/// let reaches = loop {
///     if let Some(reaches) = probe.verdict() {
///         break reaches;
///     }
///     probe.descend(owner_exit(probe.owner())?)?;
/// };
/// ```
#[derive(Clone, Debug)]
pub struct HeadPathProbe {
    candidate: PathNode,
    /// The node the walk entered its current owner at; `None` once the walk
    /// has passed a root.
    entry: Option<PathNode>,
}

impl HeadPathProbe {
    /// Ask whether `head_leaf` reaches `candidate`. `head_leaf` is `None`
    /// for a session whose head has no leaf, which reaches nothing.
    pub fn new(candidate: PathNode, head_leaf: Option<PathNode>) -> Self {
        Self {
            candidate,
            entry: head_leaf,
        }
    }

    /// `Some(true)` once the candidate is on the head's path, `Some(false)`
    /// once it cannot be, and `None` while the backend must supply the
    /// [`OwnerExit`] of [`Self::owner`].
    pub fn verdict(&self) -> Option<bool> {
        let Some(entry) = self.entry.as_ref() else {
            return Some(false);
        };
        if entry.owner_session_id == self.candidate.owner_session_id {
            // The walk visits every node of this owner at or below the
            // entry, and never this owner again.
            return Some(self.candidate.generation <= entry.generation);
        }
        if self.candidate.generation >= entry.generation {
            // Everything below the entry is lower, and the entry itself
            // belongs to another owner.
            return Some(false);
        }
        None
    }

    /// The owner whose [`OwnerExit`] the next [`Self::descend`] needs.
    /// Meaningful only while [`Self::verdict`] is `None`.
    pub fn owner(&self) -> &SessionId {
        match self.entry.as_ref() {
            Some(entry) => &entry.owner_session_id,
            None => &self.candidate.owner_session_id,
        }
    }

    /// Leave the current owner through its lowest node's parent edge.
    pub fn descend(&mut self, exit: OwnerExit) -> Result<(), StoreError> {
        let Some(entry) = self.entry.take() else {
            return Ok(());
        };
        let corrupt = |message: String| StoreError::StoredDataCorrupt {
            record_kind: "SessionGraph",
            message,
        };
        let lowest = exit.lowest.ok_or_else(|| {
            corrupt(format!(
                "owner `{}` holds no node, yet the path entered it at `{}`",
                entry.owner_session_id, entry.node_id
            ))
        })?;
        if lowest.generation > entry.generation {
            return Err(corrupt(format!(
                "owner `{}`'s lowest node `{}` is above `{}`, where the path entered it",
                entry.owner_session_id, lowest.node_id, entry.node_id
            )));
        }
        match lowest.parent {
            OwnerExitParent::Root => {
                if lowest.generation != 0 {
                    return Err(corrupt(format!(
                        "root `{}` has generation {}",
                        lowest.node_id, lowest.generation
                    )));
                }
                self.entry = None;
            }
            OwnerExitParent::Missing { node_id } => {
                return Err(corrupt(format!(
                    "parent `{node_id}` of `{}` is not stored",
                    lowest.node_id
                )));
            }
            OwnerExitParent::Node { node, tombstoned } => {
                if tombstoned {
                    return Err(corrupt(format!(
                        "parent `{}` of live path node `{}` is retired",
                        node.node_id, lowest.node_id
                    )));
                }
                if node.owner_session_id == entry.owner_session_id
                    || node.generation.checked_add(1) != Some(lowest.generation)
                {
                    return Err(corrupt(format!(
                        "parent `{}` of owner `{}`'s lowest node `{}` breaks the owner chain",
                        node.node_id, entry.owner_session_id, lowest.node_id
                    )));
                }
                self.entry = Some(node);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A raw graph: node id → (owner, generation, parent).
    struct Graph(BTreeMap<&'static str, (&'static str, u64, Option<&'static str>)>);

    impl Graph {
        fn node(&self, id: &str) -> PathNode {
            let (owner, generation, _) = self.0[id];
            PathNode {
                node_id: NodeId::from(id),
                owner_session_id: SessionId::from(owner),
                generation,
            }
        }

        fn exit(&self, owner: &SessionId) -> OwnerExit {
            let lowest = self
                .0
                .iter()
                .filter(|(_, (node_owner, _, _))| *node_owner == owner.as_str())
                .min_by_key(|(_, (_, generation, _))| *generation)
                .map(|(id, (_, generation, parent))| OwnerLowestNode {
                    node_id: NodeId::from(*id),
                    generation: *generation,
                    parent: match parent {
                        None => OwnerExitParent::Root,
                        Some(parent) if self.0.contains_key(parent) => OwnerExitParent::Node {
                            node: self.node(parent),
                            tombstoned: false,
                        },
                        Some(parent) => OwnerExitParent::Missing {
                            node_id: NodeId::from(*parent),
                        },
                    },
                });
            OwnerExit { lowest }
        }

        fn reaches(&self, head: Option<&str>, candidate: &str) -> Result<bool, StoreError> {
            let mut probe =
                HeadPathProbe::new(self.node(candidate), head.map(|head| self.node(head)));
            loop {
                if let Some(reaches) = probe.verdict() {
                    return Ok(reaches);
                }
                let exit = self.exit(probe.owner());
                probe.descend(exit)?;
            }
        }

        /// The ground truth: walk every parent edge from the head.
        fn walks_to(&self, head: &str, candidate: &str) -> bool {
            let mut cursor = Some(head);
            while let Some(id) = cursor {
                if id == candidate {
                    return true;
                }
                cursor = self.0[id].2;
            }
            false
        }
    }

    /// A0→A1→A2 in A; B forks at A1 and appends B2→B3; C forks at B2 and
    /// appends C3; D forks at A0 with no node of its own.
    fn fork_graph() -> Graph {
        Graph(BTreeMap::from([
            ("a0", ("a", 0, None)),
            ("a1", ("a", 1, Some("a0"))),
            ("a2", ("a", 2, Some("a1"))),
            ("b2", ("b", 2, Some("a1"))),
            ("b3", ("b", 3, Some("b2"))),
            ("c3", ("c", 3, Some("b2"))),
            ("u0", ("u", 0, None)),
        ]))
    }

    #[test]
    fn the_probe_agrees_with_a_full_parent_walk_for_every_head_and_candidate() {
        let graph = fork_graph();
        for head in graph.0.keys() {
            for candidate in graph.0.keys() {
                assert_eq!(
                    graph
                        .reaches(Some(head), candidate)
                        .expect("well-formed graph"),
                    graph.walks_to(head, candidate),
                    "head `{head}`, candidate `{candidate}`"
                );
            }
        }
    }

    #[test]
    fn a_fresh_fork_head_does_not_reach_its_source_past_the_fork_point() {
        // B forked at A1 with no node of its own: its head leaf is A1.
        let graph = fork_graph();
        assert!(graph.reaches(Some("a1"), "a0").expect("well-formed"));
        assert!(!graph.reaches(Some("a1"), "a2").expect("well-formed"));
        // Once B has nodes, the same holds one hop down.
        assert!(!graph.reaches(Some("b3"), "a2").expect("well-formed"));
        assert!(graph.reaches(Some("b3"), "a1").expect("well-formed"));
        assert!(!graph.reaches(Some("c3"), "b3").expect("well-formed"));
    }

    #[test]
    fn a_session_without_a_head_leaf_reaches_nothing() {
        let graph = fork_graph();
        assert!(!graph.reaches(None, "a0").expect("well-formed"));
    }

    #[test]
    fn a_broken_owner_chain_is_corruption() {
        let mut graph = fork_graph();
        // B's lowest node claims a parent inside B.
        graph.0.insert("b2", ("b", 2, Some("b3")));
        assert!(matches!(
            graph.reaches(Some("b3"), "a0"),
            Err(StoreError::StoredDataCorrupt { .. })
        ));

        let mut graph = fork_graph();
        // B's lowest node names a parent two generations down.
        graph.0.insert("b2", ("b", 2, Some("a0")));
        assert!(matches!(
            graph.reaches(Some("b3"), "a0"),
            Err(StoreError::StoredDataCorrupt { .. })
        ));

        let mut graph = fork_graph();
        // B's lowest node names a vacuumed parent.
        graph.0.insert("b2", ("b", 2, Some("gone")));
        assert!(matches!(
            graph.reaches(Some("b3"), "a0"),
            Err(StoreError::StoredDataCorrupt { .. })
        ));
    }

    #[test]
    fn a_retired_parent_under_a_live_path_is_corruption() {
        let graph = fork_graph();
        let mut probe = HeadPathProbe::new(graph.node("a0"), Some(graph.node("b3")));
        assert_eq!(probe.verdict(), None);
        let mut exit = graph.exit(probe.owner());
        if let Some(OwnerLowestNode {
            parent: OwnerExitParent::Node { tombstoned, .. },
            ..
        }) = exit.lowest.as_mut()
        {
            *tombstoned = true;
        }
        assert!(matches!(
            probe.descend(exit),
            Err(StoreError::StoredDataCorrupt { .. })
        ));
    }
}
