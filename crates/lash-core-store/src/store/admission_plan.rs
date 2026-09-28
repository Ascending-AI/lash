//! Admission and settlement rules every store backend decides alike
//! (FIG-3927).
//!
//! A row of either admission table, `pending_turn_inputs` or
//! `queued_work_batches`, is *admitted* when a write fenced by the session's
//! current drive fence records the root that took it (`admitted_root`) and the
//! recorded step that took it (`admitted_by`). Only that root's fenced commit
//! or its terminal write settles or releases the row again, so a binding is
//! never a token anyone can outrun: re-executing an admission reads the
//! binding back instead of taking rows twice.
//!
//! This module holds what the backends must not decide on their own: what an
//! admission takes ([`plan_turn_input_admission`]), which rows a settlement
//! may touch ([`require_admitted_to_root`]), where one composition of the
//! turn lane stops ([`TurnLaneStop`]), and what a wake leaves behind when it
//! leaves the queue ([`TerminalProcessWake`]).

use serde::{Deserialize, Serialize};

use super::StoreError;
use super::queued_work::{QueuedWorkClass, TurnLaneCandidate};
use crate::{BatchId, InputId, SessionId, TurnId};

/// The `admitted_by` value of the rows a root's own admission step binds
/// ([`RootStore::admit_root`](super::RootStore::admit_root)). A checkpoint's
/// admission records its step's replay key instead.
pub const ROOT_ADMISSION_STEP: &str = "admit";

/// One row of either admission table.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum IngressRowId {
    /// A `pending_turn_inputs` row.
    Input(InputId),
    /// A `queued_work_batches` row.
    Batch(BatchId),
}

impl std::fmt::Display for IngressRowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input(input) => write!(f, "input `{input}`"),
            Self::Batch(batch) => write!(f, "batch `{batch}`"),
        }
    }
}

/// What one commit does with the rows its root admitted (FIG-3927, design
/// §2.4). Keyed by the root: every row write is predicated on
/// `admitted_root = root`, and a row the root does not hold refuses the whole
/// commit [`StoreError::IngressRowNotAdmitted`].
///
/// A commit carrying a settlement must present its drive fence
/// ([`RuntimeCommit::drive_fence`](super::RuntimeCommit::drive_fence)); the
/// store checks it in the commit's own transaction before anything is
/// written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngressSettlement {
    /// The root whose admitted rows this commit settles.
    pub root: TurnId,
    /// Inputs the committing turn delivered, with their application evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_inputs: Vec<crate::TurnInputCompletion>,
    /// Queued work the committing turn delivered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_batches: Vec<crate::QueuedWorkCompletion>,
    /// Rows handed back open at their own position: the `Defer` disposition.
    /// A released active-turn input names a turn that is over, so it is
    /// re-deferred to the next turn (FIG-1573). A released wake keeps its
    /// queue position and its redelivery floor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub released: Vec<IngressRowId>,
    /// Addressed host input a cancellation's `Drop` disposition cancels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<IngressRowId>,
}

impl IngressSettlement {
    /// An empty settlement of `root`.
    #[must_use]
    pub fn new(root: TurnId) -> Self {
        Self {
            root,
            completed_inputs: Vec::new(),
            completed_batches: Vec::new(),
            released: Vec::new(),
            dropped: Vec::new(),
        }
    }

    /// Whether the settlement touches no row.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.completed_inputs
            .iter()
            .all(|completion| completion.input_ids.is_empty())
            && self
                .completed_batches
                .iter()
                .all(|completion| completion.batch_ids.is_empty())
            && self.released.is_empty()
            && self.dropped.is_empty()
    }

    /// Every row the settlement names, each once, in the order a backend
    /// writes them: completions, then releases, then drops.
    #[must_use]
    pub fn rows(&self) -> Vec<IngressRowId> {
        let mut rows = Vec::new();
        for completion in &self.completed_inputs {
            rows.extend(
                completion
                    .input_ids
                    .iter()
                    .cloned()
                    .map(IngressRowId::Input),
            );
        }
        for completion in &self.completed_batches {
            rows.extend(
                completion
                    .batch_ids
                    .iter()
                    .cloned()
                    .map(IngressRowId::Batch),
            );
        }
        rows.extend(self.released.iter().cloned());
        rows.extend(self.dropped.iter().cloned());
        rows
    }

    /// The application evidence of every completed input, in completion
    /// order.
    #[must_use]
    pub fn turn_input_applications(&self) -> Vec<crate::TurnInputApplication> {
        self.completed_inputs
            .iter()
            .flat_map(|completion| completion.applications.iter().cloned())
            .collect()
    }

    /// Refuse a settlement that names one row twice (a row is completed,
    /// released or dropped, never two of them) or a completion minted for
    /// another session.
    pub fn validate(&self, session_id: &SessionId) -> Result<(), StoreError> {
        let foreign_input = self
            .completed_inputs
            .iter()
            .filter(|completion| completion.session_id != *session_id)
            .find_map(|completion| {
                completion
                    .input_ids
                    .first()
                    .cloned()
                    .map(IngressRowId::Input)
            });
        let foreign_batch = self
            .completed_batches
            .iter()
            .filter(|completion| completion.session_id != *session_id)
            .find_map(|completion| {
                completion
                    .batch_ids
                    .first()
                    .cloned()
                    .map(IngressRowId::Batch)
            });
        if let Some(row) = foreign_input.or(foreign_batch) {
            // A completion minted for another session names no row this
            // session's root could hold.
            return Err(StoreError::IngressRowNotAdmitted {
                session_id: session_id.clone(),
                root: self.root.clone(),
                row: Box::new(row),
                admitted_root: None,
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for row in self.rows() {
            if !seen.insert(row.clone()) {
                return Err(StoreError::IngressSettlementDuplicate {
                    session_id: session_id.clone(),
                    root: self.root.clone(),
                    row: Box::new(row),
                });
            }
        }
        Ok(())
    }
}

/// The one verdict for "may this root's commit settle this row?"
/// (FIG-3927).
///
/// `admitted_root` is the row's binding as the backend read it under its
/// commit authority, `None` when the row is open, and `observed` is `false`
/// when no row holds the identity at all. Only a row bound to `root` may be
/// completed, released or dropped; anything else refuses the whole commit, so
/// a row is never answered by a root that did not admit it.
pub fn require_admitted_to_root(
    session_id: &SessionId,
    root: &TurnId,
    row: &IngressRowId,
    observed: Option<Option<&str>>,
) -> Result<(), StoreError> {
    match observed {
        Some(Some(admitted_root)) if admitted_root == root.as_str() => Ok(()),
        _ => Err(StoreError::IngressRowNotAdmitted {
            session_id: session_id.clone(),
            root: root.clone(),
            row: Box::new(row.clone()),
            admitted_root: observed
                .flatten()
                .map(|root| TurnId::from(root.to_string())),
        }),
    }
}

/// The one verdict for "may the command lane settle this command row?"
/// (design §2.7): the row still exists and no root admitted it. A concurrent
/// withdrawal refuses the applying commit whole, so no patch applies twice
/// or over a withdrawn command.
pub fn require_open_command(
    session_id: &SessionId,
    batch_id: &BatchId,
    observed: Option<Option<&str>>,
) -> Result<(), StoreError> {
    match observed {
        Some(None) => Ok(()),
        _ => Err(StoreError::SessionCommandWithdrawn {
            session_id: session_id.clone(),
            batch_id: batch_id.clone(),
        }),
    }
}

/// The durable state a row takes when `mode` admits it (FIG-3927): a
/// next-turn row keeps its state, bound where it stands; active-turn input a
/// checkpoint takes is `accepted` into the running turn.
#[must_use]
pub fn turn_input_state_after_admission(
    mode: &crate::TurnInputAdmissionMode,
) -> crate::TurnInputStateKind {
    match mode {
        crate::TurnInputAdmissionMode::ActiveTurn { .. } => crate::TurnInputStateKind::Accepted,
        crate::TurnInputAdmissionMode::NextTurn => crate::TurnInputStateKind::DeferredNextTurn,
    }
}

/// Compose one turn-input admission over its candidate rows, in
/// `enqueue_seq` order (FIG-3927): `None` when nothing is admitted.
///
/// A next-turn admission never mixes run specs (FIG-3838): the prefix stops,
/// never skips, at the first row whose spec differs from its head's. A
/// checkpoint admission delivers into a running root whose shape is already
/// recorded, and enqueue refused every differing explicit spec there. The
/// returned inputs carry the state the admission writes.
#[must_use]
pub fn plan_turn_input_admission(
    session_id: &SessionId,
    mode: crate::TurnInputAdmissionMode,
    mut rows: Vec<crate::PendingTurnInput>,
) -> Option<crate::AdmittedTurnInputs> {
    let spec = rows.first()?.run_spec.clone();
    if matches!(&mode, crate::TurnInputAdmissionMode::NextTurn) {
        let same_spec = rows.iter().take_while(|row| row.run_spec == spec).count();
        rows.truncate(same_spec);
    }
    if turn_input_state_after_admission(&mode) == crate::TurnInputStateKind::Accepted {
        for input in &mut rows {
            if let Some(accepted) = input.state.accepted() {
                input.state = accepted;
            }
        }
    }
    Some(crate::AdmittedTurnInputs {
        session_id: session_id.clone(),
        mode,
        inputs: rows,
        applications: Vec::new(),
    })
}

/// Where the turn lane stops a composition of one admission table (ADR 0101
/// §5, as the FIG-3540 close-out amends it): the `enqueue_seq` of the other
/// table's earliest open turn-lane row.
///
/// Host input and queued turn work take one per-session sequence and form one
/// FIFO, so a composition of either table takes only rows accepted before that
/// point. It stops there and never skips it: one turn never takes an item
/// past an earlier unconsumed item of the other kind. A session command is
/// the command lane (§4) and stops nothing.
///
/// A composition of queued work applies it here, over its candidate scan. A
/// next-turn input composition carries the same stop as a predicate of its
/// candidate statement, so the input composition keeps its round-trip budget.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TurnLaneStop(Option<u64>);

impl TurnLaneStop {
    /// The stop before `earliest_other_kind`, the other table's earliest open
    /// turn-lane row; `None` stops nothing.
    pub const fn before(earliest_other_kind: Option<u64>) -> Self {
        Self(earliest_other_kind)
    }

    /// Whether a row at `enqueue_seq` lies before the stop.
    pub fn admits(self, enqueue_seq: u64) -> bool {
        self.0.is_none_or(|stop| enqueue_seq < stop)
    }

    /// How many leading `candidates` a composition of queued work may take. A
    /// session command is never stopped.
    pub fn queued_prefix(self, candidates: &[TurnLaneCandidate]) -> usize {
        candidates
            .iter()
            .take_while(|candidate| {
                candidate.kind.work_class() == QueuedWorkClass::SessionCommand
                    || self.admits(candidate.enqueue_seq)
            })
            .count()
    }
}

/// The process wake a batch carried when it left the queue, which its
/// session's redelivery fence must record (FIG-1065, FIG-3545).
///
/// Every terminal transition of a wake row — settlement and host cancel
/// alike — raises the fence to `max(floor, sequence)` in the same
/// transaction that removes the row. Otherwise a redelivery of the same
/// `(process, sequence)` after a producer crash or a failed terminal mark
/// finds neither a row nor a floor and re-admits the wake.
///
/// A process-wake batch is validated at enqueue to carry exactly one wake
/// payload, so "the wake the batch carried" is one fact no matter how a
/// backend reads it.
#[derive(Clone, Debug)]
pub struct TerminalProcessWake {
    /// The batch's source key. PostgreSQL advisory-locks this identity before
    /// writing the fence; backends without advisory locks may leave it `None`.
    pub source_key: Option<String>,
    /// Structural producer identity the fence indexes on.
    pub process_id: crate::ProcessId,
    /// The terminal sequence the allocation floor rises to.
    pub sequence: u64,
}

impl TerminalProcessWake {
    /// The wake `payload` carries, if it is a process wake, under the batch's
    /// `source_key`.
    pub fn of_payload(
        source_key: Option<String>,
        payload: &crate::QueuedWorkPayload,
    ) -> Option<Self> {
        match payload {
            crate::QueuedWorkPayload::ProcessWake { wake } => Some(Self {
                source_key,
                process_id: wake.process_id.clone(),
                sequence: wake.sequence,
            }),
            crate::QueuedWorkPayload::SessionCommand { .. } => None,
        }
    }

    /// The wake a hydrated batch carries, if any.
    pub fn of_batch(batch: &crate::QueuedWorkBatch) -> Option<Self> {
        batch
            .items
            .iter()
            .find_map(|item| Self::of_payload(batch.source_key.clone(), &item.payload))
    }
}

/// The affected-item records of the process wakes `batches` hold, each
/// deferred (FIG-3543, ADR 0101 §10): one per wake item, in batch and item
/// order. A cancel commit writes them for the wakes it released; batches that
/// hold no wake yield none.
#[must_use]
pub fn deferred_wake_records(
    batches: &[crate::QueuedWorkBatch],
) -> Vec<crate::turn_control_vocabulary::TurnCancelAffectedWake> {
    batches
        .iter()
        .flat_map(|batch| {
            batch.items.iter().filter_map(|item| match &item.payload {
                crate::QueuedWorkPayload::ProcessWake { wake } => Some(
                    crate::turn_control_vocabulary::TurnCancelAffectedWake::deferred(
                        batch.batch_id.clone(),
                        item.item_id.clone(),
                        (**wake).clone(),
                    ),
                ),
                crate::QueuedWorkPayload::SessionCommand { .. } => None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> IngressRowId {
        IngressRowId::Input(InputId::from(id))
    }

    #[test]
    fn only_the_admitting_root_settles_a_row() {
        let session = SessionId::from("s");
        let root = TurnId::from("r");
        assert!(require_admitted_to_root(&session, &root, &row("a"), Some(Some("r"))).is_ok());
        for observed in [None, Some(None), Some(Some("other"))] {
            assert!(matches!(
                require_admitted_to_root(&session, &root, &row("a"), observed),
                Err(StoreError::IngressRowNotAdmitted { .. })
            ));
        }
    }

    #[test]
    fn a_command_settles_only_while_it_is_open() {
        let session = SessionId::from("s");
        let batch = BatchId::from("b");
        assert!(require_open_command(&session, &batch, Some(None)).is_ok());
        for observed in [None, Some(Some("r"))] {
            assert!(matches!(
                require_open_command(&session, &batch, observed),
                Err(StoreError::SessionCommandWithdrawn { .. })
            ));
        }
    }

    #[test]
    fn a_settlement_names_each_row_once() {
        let session = SessionId::from("s");
        let mut settlement = IngressSettlement::new(TurnId::from("r"));
        settlement.released.push(row("a"));
        assert!(settlement.validate(&session).is_ok());
        settlement.dropped.push(row("a"));
        assert!(matches!(
            settlement.validate(&session),
            Err(StoreError::IngressSettlementDuplicate { .. })
        ));
    }
}
