//! Admission and settlement rules every store backend decides alike
//! (FIG-3927).
//!
//! A row of either admission table, `pending_turn_inputs` or
//! `queued_work_batches`, is *admitted* when a write fenced by the session's
//! current shift fence records the run that took it (`admitted_run`) and the
//! recorded step that took it (`admitted_by`). Only that run's fenced commit
//! or its terminal write settles or releases the row again, so a binding is
//! never a token anyone can outrun: re-executing an admission reads the
//! binding back instead of taking rows twice.
//!
//! This module holds what the backends must not decide on their own: what an
//! admission takes ([`plan_next_turn_input_admission`],
//! [`plan_checkpoint_input_admission`]) and which rows a settlement may
//! touch ([`require_admitted_to_run`]).

use serde::{Deserialize, Serialize};

use super::StoreError;
use crate::{BatchId, InputId, SessionId, TurnId};

/// The `admitted_by` value of the rows a run's own admission step binds
/// ([`RunStore::admit_run`](super::RunStore::admit_run)). A checkpoint's
/// admission records its step's replay key instead.
pub const RUN_ADMISSION_STEP: &str = "admit";

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

/// What one commit does with the rows its run admitted (FIG-3927, design
/// §2.4). Keyed by the run: every row write is predicated on
/// `admitted_run = run`, and a row the run does not hold refuses the whole
/// commit [`StoreError::IngressRowNotAdmitted`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngressSettlement {
    /// The run whose admitted rows this commit settles.
    pub run: TurnId,
    /// Inputs the committing turn delivered, with their application evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_inputs: Vec<crate::TurnInputCompletion>,
    /// Queued work the committing turn delivered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_batches: Vec<crate::QueuedWorkCompletion>,
    /// Rows handed back open at their own position: the `Defer` disposition.
    /// A released active-turn input names a turn that is over, so it is
    /// re-deferred to the next turn (FIG-1573).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub released: Vec<IngressRowId>,
    /// Addressed host input a cancellation's `Drop` disposition cancels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<IngressRowId>,
}

impl IngressSettlement {
    /// An empty settlement of `run`.
    #[must_use]
    pub fn new(run: TurnId) -> Self {
        Self {
            run,
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
            // session's run could hold.
            return Err(StoreError::IngressRowNotAdmitted {
                session_id: session_id.clone(),
                run: self.run.clone(),
                row: Box::new(row),
                admitted_run: None,
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for row in self.rows() {
            if !seen.insert(row.clone()) {
                return Err(StoreError::IngressSettlementDuplicate {
                    session_id: session_id.clone(),
                    run: self.run.clone(),
                    row: Box::new(row),
                });
            }
        }
        Ok(())
    }
}

/// The one verdict for "may this run's commit settle this row?"
/// (FIG-3927).
///
/// `admitted_run` is the row's binding as the backend read it under its
/// commit authority, `None` when the row is open, and `observed` is `false`
/// when no row holds the identity at all. Only a row bound to `run` may be
/// completed, released or dropped; anything else refuses the whole commit, so
/// a row is never answered by a run that did not admit it.
pub fn require_admitted_to_run(
    session_id: &SessionId,
    run: &TurnId,
    row: &IngressRowId,
    observed: Option<Option<&str>>,
) -> Result<(), StoreError> {
    match observed {
        Some(Some(admitted_run)) if admitted_run == run.as_str() => Ok(()),
        _ => Err(StoreError::IngressRowNotAdmitted {
            session_id: session_id.clone(),
            run: run.clone(),
            row: Box::new(row.clone()),
            admitted_run: observed.flatten().map(TurnId::parse).transpose()?,
        }),
    }
}

/// The one verdict for "may the command lane settle this command row?"
/// (design §2.7): the row still exists and no run admitted it. A concurrent
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

/// The durable state a row takes when `mode` admits it (FIG-3927), `None`
/// when it keeps its own: a next-turn row stays in the state its submitted
/// delivery names, bound where it stands, an addressed row included (ADR 0101
/// §5.1); active-turn input a checkpoint takes is `accepted` into the running
/// turn.
#[must_use]
pub fn turn_input_state_after_admission(
    mode: &crate::TurnInputAdmissionMode,
) -> Option<crate::TurnInputStateKind> {
    match mode {
        crate::TurnInputAdmissionMode::ActiveTurn { .. } => {
            Some(crate::TurnInputStateKind::Accepted)
        }
        crate::TurnInputAdmissionMode::NextTurn => None,
    }
}

/// Compose one idle run admission of next-turn host input over its
/// candidate rows, in `enqueue_seq` order and at most `max_inputs` of them
/// (FIG-3927): `None` when there are none.
///
/// The composition never mixes run specs (FIG-3838): the prefix stops, never
/// skips, at the first row whose spec differs from its head's. How much of
/// that eligible prefix one run takes is the host's drain policy's decision,
/// (ADR 0101 §5.2): the default takes the head alone,
/// so each next-turn input is its own run and a cancel of one never reaches
/// another (FIG-4457). A next-turn row keeps its own state when admitted.
#[must_use]
pub fn plan_next_turn_input_admission(
    session_id: &SessionId,
    mut rows: Vec<crate::PendingTurnInput>,
    max_inputs: usize,
    policy: &crate::TurnLaneAdmissionPolicy,
    now_epoch_ms: u64,
) -> Option<crate::AdmittedTurnInputs> {
    let spec = rows.first()?.run_spec.clone();
    let same_spec = rows.iter().take_while(|row| row.run_spec == spec).count();
    rows.truncate(same_spec);
    if rows.len() > 1 {
        let candidates = rows
            .iter()
            .map(|row| crate::QueuedDrainCandidate {
                enqueue_seq: row.enqueue_seq,
                family: crate::QueuedDrainFamily::HostInput,
                merge_key: None,
                authority: crate::QueuedWorkAuthority::default(),
                // One serialized UTF-8 byte of the input charged as one
                // token.
                projected_tokens: serde_json::to_vec(&row.input).map_or(0, |bytes| bytes.len()),
                pending_age_ms: now_epoch_ms.saturating_sub(row.enqueued_at_ms),
            })
            .collect::<Vec<_>>();
        let request = crate::QueuedDrainRequest::new(
            &candidates,
            policy
                .max_context_tokens
                .saturating_sub(policy.action_token_reserve),
            policy.max_context_tokens,
            max_inputs,
            crate::AdmissionBoundary::Idle,
        );
        let selected = policy
            .drain_policy
            .select_drain(&request)
            .drain_count()
            .clamp(1, rows.len());
        tracing::debug!(
            target: "lash::queued_work_batching",
            drain_policy = policy.drain_policy.name(),
            offered = rows.len(),
            selected,
            "next-turn input drain policy selection"
        );
        rows.truncate(selected);
    }
    Some(crate::AdmittedTurnInputs {
        session_id: session_id.clone(),
        mode: crate::TurnInputAdmissionMode::NextTurn,
        inputs: rows,
        applications: Vec::new(),
    })
}

/// Compose one checkpoint admission of the input addressed to the running
/// physical turn `turn_id`, over its candidate rows in `enqueue_seq` order
/// (FIG-3927): `None` when there are none.
///
/// The input joins a running run whose shape is already recorded, and
/// enqueue refused every differing explicit spec there. Each returned input
/// is `accepted` into the running turn, the state the admission writes.
#[must_use]
pub fn plan_checkpoint_input_admission(
    session_id: &SessionId,
    turn_id: &TurnId,
    checkpoint: crate::CheckpointKind,
    mut rows: Vec<crate::PendingTurnInput>,
) -> Option<crate::AdmittedTurnInputs> {
    if rows.is_empty() {
        return None;
    }
    for input in &mut rows {
        if let Some(accepted) = input.state.accepted() {
            input.state = accepted;
        }
    }
    Some(crate::AdmittedTurnInputs {
        session_id: session_id.clone(),
        mode: crate::TurnInputAdmissionMode::ActiveTurn {
            turn_id: turn_id.clone(),
            checkpoint,
        },
        inputs: rows,
        applications: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> IngressRowId {
        IngressRowId::Input(InputId::fixture(id))
    }

    #[test]
    fn only_the_admitting_root_settles_a_row() {
        let session = SessionId::from("s");
        let run = TurnId::from("r");
        assert!(require_admitted_to_run(&session, &run, &row("a"), Some(Some("r"))).is_ok());
        for observed in [None, Some(None), Some(Some("other"))] {
            assert!(matches!(
                require_admitted_to_run(&session, &run, &row("a"), observed),
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
}
