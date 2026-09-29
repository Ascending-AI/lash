//! The history segment of the multi-session store (ADR 0112 §1.3, §5 to §8).
//!
//! These are the only store reads that decode graph bodies, usage rows or
//! turn receipts. A window read is bounded by the session's current frame;
//! every other read that returns history is paged under an explicit budget,
//! and the rest are predicates that return no content.
use std::num::{NonZeroU32, NonZeroU64};

use super::{BlobRef, HydratedSessionCheckpoint, PendingFollowOn, SessionHeadRef, StoreError};
use crate::usage::SessionUsageTotals;
use crate::{
    FrameNodeId, NodeId, PersistedSessionConfig, SessionGraph, SessionId, SessionNodeRecord,
    TokenLedgerEntry, TurnFailureSettlement, TurnId,
};

/// Which head a window read resolves its leaf from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowSelector {
    /// The live head.
    Current,
    /// The head a turn was admitted on (FIG-3682).
    Admitted(SessionHeadRef),
}

/// One session's current frame, read in one snapshot (ADR 0112 §5).
///
/// `window` holds exactly the rows from the frame's `FrameOpen` to the
/// selected leaf, anchored by [`SessionGraph::anchor`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SessionWindowRead {
    pub session_id: SessionId,
    pub head_revision: u64,
    pub config: PersistedSessionConfig,
    /// Equals `window.anchor().map(|a| &a.frame_node_id)`. `None` iff the
    /// window is empty.
    pub current_frame_node_id: Option<FrameNodeId>,
    pub pending_follow_on: Option<PendingFollowOn>,
    pub window: SessionGraph,
    pub checkpoint_ref: Option<BlobRef>,
    pub checkpoint: Option<HydratedSessionCheckpoint>,
    pub usage: SessionUsageTotals,
}

impl SessionWindowRead {
    /// Assemble a window read, checking that the frame pointer agrees with
    /// the window's anchor.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate
    /// implementors**. The struct is `#[non_exhaustive]`, so this is how a
    /// backend outside this crate builds one.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: SessionId,
        head_revision: u64,
        config: PersistedSessionConfig,
        pending_follow_on: Option<PendingFollowOn>,
        window: SessionGraph,
        checkpoint_ref: Option<BlobRef>,
        checkpoint: Option<HydratedSessionCheckpoint>,
        usage: SessionUsageTotals,
    ) -> Result<Self, StoreError> {
        if window.nodes.is_empty() != window.anchor().is_none() {
            return Err(StoreError::StoredDataCorrupt {
                record_kind: "SessionWindowRead",
                message: format!(
                    "window of session `{session_id}` has {} nodes but {} anchor",
                    window.nodes.len(),
                    if window.anchor().is_some() {
                        "an"
                    } else {
                        "no"
                    }
                ),
            });
        }
        let current_frame_node_id = window.anchor().map(|anchor| anchor.frame_node_id.clone());
        Ok(Self {
            session_id,
            head_revision,
            config,
            current_frame_node_id,
            pending_follow_on,
            window,
            checkpoint_ref,
            checkpoint,
            usage,
        })
    }
}

/// Where a history page starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryAnchor {
    /// The live head's leaf, inclusive.
    Head,
    /// The named node, inclusive.
    Node(NodeId),
    /// Continue a previous page.
    Cursor(HistoryCursor),
}

/// Both limits are required and non-zero. There is no default and no
/// unbounded value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryBudget {
    pub max_nodes: NonZeroU32,
    pub max_bytes: NonZeroU64,
}

/// One page of one ancestry, in descending generation (ADR 0112 §6).
#[derive(Clone, Debug)]
pub struct HistoryPage {
    /// The leaf this paging run is pinned to. `None` only for `Head` on a
    /// session whose head has no leaf.
    pub pinned_leaf: Option<NodeId>,
    /// Descending generation. The first entry is the anchor node.
    pub nodes: Vec<HistoryNode>,
    pub stop: HistoryStop,
    /// `Some` iff `stop` is `NodeBudget` or `ByteBudget`.
    pub next: Option<HistoryCursor>,
}

/// One history row with the stored facts a page is budgeted and checked by.
#[derive(Clone, Debug)]
pub struct HistoryNode {
    pub generation: u64,
    pub owner_session_id: SessionId,
    pub frame_node_id: FrameNodeId,
    pub body_bytes: u64,
    pub record: SessionNodeRecord,
}

/// Why a history page ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryStop {
    NodeBudget,
    ByteBudget,
    /// The page ends at generation 0. Nothing remains.
    Root,
}

/// Opaque and session-bound. Serializable so a host can hand it through
/// its UI. The store revalidates it on every use, so a forged cursor reads
/// nothing the session could not already read.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HistoryCursor {
    session_id: SessionId,
    pinned_leaf: NodeId,
    lineage: LineageStamp,
    next_node_id: NodeId,
    next_generation: u64,
}

impl HistoryCursor {
    /// Mint the cursor for the page after the one a backend just read.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate
    /// implementors**.
    pub fn new(
        session_id: SessionId,
        pinned_leaf: NodeId,
        lineage: LineageStamp,
        next_node_id: NodeId,
        next_generation: u64,
    ) -> Self {
        Self {
            session_id,
            pinned_leaf,
            lineage,
            next_node_id,
            next_generation,
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn pinned_leaf(&self) -> &NodeId {
        &self.pinned_leaf
    }

    /// The lineage the cursor was minted under. A backend recomputes the
    /// session's stamp on every use and refuses a difference with
    /// [`StoreError::HistoryCursorLineageChanged`].
    pub fn lineage(&self) -> &LineageStamp {
        &self.lineage
    }

    /// The first node of the next page.
    pub fn next_node_id(&self) -> &NodeId {
        &self.next_node_id
    }

    /// The stored generation of [`Self::next_node_id`].
    pub fn next_generation(&self) -> u64 {
        self.next_generation
    }

    /// Refuse a cursor minted for another session.
    pub fn check_session(&self, session_id: &SessionId) -> Result<(), StoreError> {
        check_cursor_session(&self.session_id, session_id)
    }
}

/// BLAKE3 (domain `lash-history-lineage/v1`) over the session's
/// `fork_lineage` rows, ordered by `ancestor_session_id`, each encoded as
/// `(ancestor_session_id, fork_generation)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LineageStamp([u8; 32]);

impl LineageStamp {
    const DOMAIN: &'static str = "lash-history-lineage/v1";

    /// Stamp the `(ancestor_session_id, fork_generation)` rows of one
    /// session's `fork_lineage`, in any order. Each row is encoded as the
    /// id's byte length (u64, big-endian), the id's bytes, then the
    /// generation (u64, big-endian), so no two row sets share an encoding.
    pub fn of_lineage<'a>(rows: impl IntoIterator<Item = (&'a SessionId, u64)>) -> Self {
        let mut rows = rows.into_iter().collect::<Vec<_>>();
        rows.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
        let mut preimage = Vec::new();
        for (ancestor_session_id, fork_generation) in rows {
            let id = ancestor_session_id.as_str().as_bytes();
            preimage.extend_from_slice(&(id.len() as u64).to_be_bytes());
            preimage.extend_from_slice(id);
            preimage.extend_from_slice(&fork_generation.to_be_bytes());
        }
        let hex = crate::stable_hash::blake3_hex(Self::DOMAIN, &preimage);
        let mut bytes = [0_u8; 32];
        let (pairs, _) = hex.as_bytes().as_chunks::<2>();
        for (byte, [high, low]) in bytes.iter_mut().zip(pairs) {
            *byte = (hex_nibble(*high) << 4) | hex_nibble(*low);
        }
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

fn hex_nibble(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        b'A'..=b'F' => digit - b'A' + 10,
        _ => 0,
    }
}

/// One page of a session's full usage rows, in `seq` order (ADR 0112 §8).
#[derive(Clone, Debug)]
pub struct UsageLedgerPage {
    pub rows: Vec<UsageLedgerRow>,
    /// `Some` only when more rows exist.
    pub next: Option<UsageLedgerCursor>,
}

/// One stored usage row.
#[derive(Clone, Debug)]
pub struct UsageLedgerRow {
    pub seq: u64,
    pub operation_storage_key: String,
    pub entry: TokenLedgerEntry,
}

/// Session-bound position after one usage row.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UsageLedgerCursor {
    session_id: SessionId,
    after_seq: u64,
}

impl UsageLedgerCursor {
    pub fn new(session_id: SessionId, after_seq: u64) -> Self {
        Self {
            session_id,
            after_seq,
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The last `seq` the previous page returned.
    pub fn after_seq(&self) -> u64 {
        self.after_seq
    }

    /// Refuse a cursor minted for another session.
    pub fn check_session(&self, session_id: &SessionId) -> Result<(), StoreError> {
        check_cursor_session(&self.session_id, session_id)
    }
}

/// One page of a session's turn failure evidence, ordered by
/// `(committed_at_ms, turn_id)` (ADR 0112 §8).
#[derive(Clone, Debug)]
pub struct FailureEvidencePage {
    pub settlements: Vec<TurnFailureSettlement>,
    /// `Some` only when more receipts carry failure evidence.
    pub next: Option<FailureEvidenceCursor>,
}

/// Session-bound position after one failure-bearing receipt.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailureEvidenceCursor {
    session_id: SessionId,
    committed_at_ms: u64,
    turn_id: TurnId,
}

impl FailureEvidenceCursor {
    pub fn new(session_id: SessionId, committed_at_ms: u64, turn_id: TurnId) -> Self {
        Self {
            session_id,
            committed_at_ms,
            turn_id,
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The commit time of the last receipt the previous page returned.
    pub fn committed_at_ms(&self) -> u64 {
        self.committed_at_ms
    }

    /// The turn of the last receipt the previous page returned.
    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    /// Refuse a cursor minted for another session.
    pub fn check_session(&self, session_id: &SessionId) -> Result<(), StoreError> {
        check_cursor_session(&self.session_id, session_id)
    }
}

fn check_cursor_session(
    cursor_session_id: &SessionId,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    if cursor_session_id == session_id {
        Ok(())
    } else {
        Err(StoreError::CursorForeignSession {
            cursor_session_id: cursor_session_id.clone(),
            session_id: session_id.clone(),
        })
    }
}

/// Bounded history reads (ADR 0112 §5 to §8).
///
/// Every operation is required and scoped to `session_id`. A deleted session
/// answers [`StoreError::SessionDeleted`] everywhere.
#[async_trait::async_trait]
pub trait SessionHistoryStore: Send + Sync {
    /// The session's frame at the selected leaf, from its `FrameOpen` to the
    /// leaf, with the head, checkpoint and usage totals of the same snapshot.
    ///
    /// - The frame is the `frame_node_id` column of the selected leaf row.
    ///   Under `Current` the head's `current_frame_node_id` must equal it, or
    ///   the read fails [`StoreError::CurrentFrameNodeMismatch`].
    /// - `Admitted(base)` reads `base.leaf` and `base.checkpoint`, reports
    ///   `head_revision = base.revision` and no pending follow-on, and never
    ///   answers `None`: a base the store no longer holds is
    ///   [`StoreError::TurnBaseNotRetained`].
    /// - `config` is the head's when the frame is the head's current frame,
    ///   and the frame's `FrameOpen` config otherwise. `usage` is the live
    ///   session's totals under both selectors.
    /// - Rows are validated as one anchored chain (§5), and a violation is
    ///   [`StoreError::InvalidWindowAnchor`] or
    ///   [`StoreError::StoredDataCorrupt`], never a smaller window.
    ///
    /// `Ok(None)` means the session has no head row, under `Current` only. A
    /// head with no leaf answers an empty, unanchored window.
    async fn load_session_window(
        &self,
        session_id: &SessionId,
        selector: WindowSelector,
    ) -> Result<Option<SessionWindowRead>, StoreError>;

    /// One page of the pinned leaf's ancestry, descending by generation,
    /// across frame and fork boundaries (ADR 0112 §6).
    ///
    /// Budgets are checked on the stored `body_bytes` before a body is
    /// fetched. A first row alone over `max_bytes` is
    /// [`StoreError::HistoryNodeTooLarge`], so every page with a `next` holds
    /// a node. A gone anchor is [`StoreError::HistoryAnchorUnavailable`];
    /// `Head` on a headless session is [`StoreError::SessionNotFound`], and
    /// on a head with no leaf an empty `Root` page. A cursor of another
    /// session is [`StoreError::CursorForeignSession`], and one whose
    /// lineage changed is [`StoreError::HistoryCursorLineageChanged`].
    async fn load_ancestors(
        &self,
        session_id: &SessionId,
        anchor: HistoryAnchor,
        budget: HistoryBudget,
    ) -> Result<HistoryPage, StoreError>;

    /// A predicate, not a history view: `true` iff `node_id` is a live row
    /// readable by the session at or below its head leaf's generation, which
    /// is exactly "on the active path" (ADR 0112 §7). A head with no leaf
    /// answers `false`. It decodes no body.
    ///
    /// It answers at a read snapshot and never replaces the commit fence.
    async fn contains_active_ancestor(
        &self,
        session_id: &SessionId,
        node_id: &NodeId,
    ) -> Result<bool, StoreError>;

    /// The session's usage totals: one aggregate row per `(source, model)`
    /// plus the outstanding holes. It decodes no reported usage row.
    async fn load_usage_totals(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionUsageTotals, StoreError>;

    /// One page of the session's full usage rows in `seq` order. `next` is
    /// `Some` only when more rows exist.
    async fn load_usage_ledger_page(
        &self,
        session_id: &SessionId,
        after: Option<&UsageLedgerCursor>,
        limit: NonZeroU32,
    ) -> Result<UsageLedgerPage, StoreError>;

    /// One page of the session's failure-bearing turn receipts, ordered by
    /// `(committed_at_ms, turn_id)`. It decodes exactly the receipts it
    /// returns.
    async fn load_failure_evidence_page(
        &self,
        session_id: &SessionId,
        after: Option<&FailureEvidenceCursor>,
        limit: NonZeroU32,
    ) -> Result<FailureEvidencePage, StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lineage_stamp_ignores_row_order_and_separates_row_sets() {
        let a = SessionId::from("a");
        let b = SessionId::from("b");
        let ab = LineageStamp::of_lineage([(&a, 3), (&b, 7)]);
        assert_eq!(ab, LineageStamp::of_lineage([(&b, 7), (&a, 3)]));
        assert_ne!(ab, LineageStamp::of_lineage([(&a, 3), (&b, 8)]));
        assert_ne!(ab, LineageStamp::of_lineage([(&a, 3)]));
        assert_ne!(
            LineageStamp::of_lineage(std::iter::empty()),
            LineageStamp::of_lineage([(&a, 0)])
        );
    }

    #[test]
    fn a_cursor_refuses_another_session() {
        let cursor = UsageLedgerCursor::new(SessionId::from("a"), 4);
        assert!(cursor.check_session(&SessionId::from("a")).is_ok());
        assert!(matches!(
            cursor.check_session(&SessionId::from("b")),
            Err(StoreError::CursorForeignSession { .. })
        ));
    }
}
