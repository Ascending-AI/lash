//! A session-graph append's request and outcome: what a plugin's graph
//! service call and a host's `SessionCommand::AppendSessionNodes` carry, and
//! what either settles as (FIG-4202). They live below the engine so the
//! session command that carries an append can persist it and its settlement.

use serde::{Deserialize, Serialize};

use crate::SessionAppendNode;

/// A plugin-authored append onto a session's history graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppendSessionNodesRequest {
    /// Caller-stable identity for this logical append. While its durable receipt
    /// is retained, a retry that reproduces the same ancestor, ordered nodes,
    /// and semantic field values under the same identity-encoding version
    /// returns the first append result even after the session head advances.
    /// Changing any of those request fields while reusing the operation id is a
    /// typed caller conflict. A retry must therefore preserve the original
    /// request, rather than rebuilding it from the session's new head.
    pub operation_id: String,
    pub nodes: Vec<SessionAppendNode>,
    /// Branch-liveness precondition: refuse the append unless this node is
    /// still somewhere on the session's active path. `None` skips the check.
    ///
    /// This serves the derive-then-append pattern — read history up to some
    /// node, spend seconds deriving something from it (a model summary, a
    /// memory observation, an embedding, an index entry), then append the
    /// result. Such a caller has to tell two kinds of staleness apart. *More
    /// content arrived* is harmless: the derivation is still true of the prefix
    /// it read, and discarding expensive work over it would be wrong. *The
    /// history it read was rewritten* is fatal: the base the derivation
    /// describes is no longer part of what this session executes. This field
    /// catches the second and deliberately tolerates the first.
    ///
    /// # Accepted
    ///
    /// The runtime reloads the durable head first, then accepts the append if
    /// the named node is anywhere on the resulting active path — the leaf or
    /// any ancestor of it. An append whose base was overtaken while it was
    /// being derived is therefore accepted.
    ///
    /// # Where the nodes land
    ///
    /// **Not at the named node.** The append is always built from the *current*
    /// leaf: the first new node's parent is the leaf as of that reload, and
    /// history stays linear. A caller that named an ancestor gets its nodes
    /// appended *after* content it never read. Nothing forks, and nothing
    /// already committed is lost or reordered — but the position of the
    /// appended nodes carries no claim about what preceded them. A reader that
    /// needs to know what a node was derived from must find that in the node's
    /// own payload (an observed message id, a revision) instead of inferring it
    /// from graph position.
    ///
    /// # Refused
    ///
    /// [`AppendSessionNodesOutcome::StaleBranch`], with nothing written, when the
    /// named node has left the active path: the session forked or was rewound
    /// onto another line of history, or the id was never durable here at all.
    /// This applies only to a fresh operation id. A retry whose durable receipt
    /// proves the append already committed returns that first result even when
    /// the ancestor has since left the active path.
    ///
    /// # What this does *not* guarantee
    ///
    /// This is **not** a compare-and-swap on the session head, and it does
    /// **not** detect concurrent appends. Any number of other writers may have
    /// committed between the caller's read and this append, and this field will
    /// still accept it. If a derivation is only valid when nothing at all was
    /// added since it read — for instance it *replaces* rather than accumulates
    /// some derived state — this field will not protect it: carry the observed
    /// base in the payload and let the reader adjudicate, or make the derived
    /// state idempotent under late arrival. The store underneath does enforce a
    /// strict head fence, but the runtime satisfies it by construction by
    /// re-parenting onto the leaf it just read, so it never surfaces here as a
    /// conflict a caller could use as a concurrency signal.
    #[serde(default)]
    pub requires_ancestor_node_id: Option<crate::NodeId>,
}

/// Outcome of `SessionGraphService::append_session_nodes`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AppendSessionNodesOutcome {
    /// The nodes are durable. `node_ids` are their store-assigned ids in
    /// request order. On a fresh append, `leaf_node_id` is the selected leaf
    /// after that commit, and `None` only while the graph holds no node. On
    /// receipt replay both fields are the stored
    /// first-attempt result; later commits may have moved the current session
    /// leaf elsewhere.
    ///
    /// The first appended node parents on whatever the leaf was when the
    /// runtime reloaded the head, which is not necessarily
    /// [`AppendSessionNodesRequest::requires_ancestor_node_id`]; see that field.
    Appended {
        node_ids: Vec<crate::NodeId>,
        leaf_node_id: Option<crate::NodeId>,
    },
    /// Nothing was written: the branch the caller read from has been abandoned.
    /// [`AppendSessionNodesRequest::requires_ancestor_node_id`] named a node
    /// that is no longer on this session's active path, so the base the
    /// derivation describes is gone from this session's line of execution.
    ///
    /// This is not a failed compare-and-swap. There is no expected-versus-actual
    /// head to reconcile and no value to retry *against*: resubmitting the same
    /// append cannot make the base come back, so the derivation has to be redone
    /// against a fresh read or dropped. A head that merely advanced never
    /// produces this outcome.
    StaleBranch {
        /// Echo of the request's `requires_ancestor_node_id`, so a caller with
        /// several derivations in flight can tell which one lost its base.
        required_node_id: crate::NodeId,
    },
}
