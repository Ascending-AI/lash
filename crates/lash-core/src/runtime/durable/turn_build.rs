//! What a turn's machine is built from, journaled once at the turn's start
//! (FIG-7585): the turn's new messages, the attachment-omission and
//! context-pressure decisions its start made, the frame it runs in and its
//! index. With the run configuration its PhaseRoot records and the window at
//! the turn's pin, these rows are everything the one machine builder reads:
//! a resume and the open-turn inspection build the turn's machine from them
//! with no claim, no store write and no plugin callback, so the machine is
//! the one the turn's start built even when the plugin that decided is gone.

use crate::plugin::{AttachmentOmissions, HistoryPartId};

pub(in crate::runtime) type HostIntent =
    <crate::HostTurnProtocol as lash_sansio::TurnProtocol>::IntentOutcome;

/// The turn's build inputs, the first row of its journal.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnBuild {
    /// The turn's index in its session.
    pub turn_index: usize,
    /// The agent frame the turn runs in, once its context pressure ran.
    pub frame: String,
    /// The turn's new messages after the window: one opening message per
    /// accepted input.
    pub messages: Vec<crate::Message>,
    /// The parts of the request's history whose attachments the turn's
    /// attachment-omission policies omitted, with the replacement text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omissions: Vec<RecordedOmission>,
    /// The records the turn's context-pressure hooks decided without a
    /// frame, which the turn's commit appends.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pressure: Vec<crate::AppendSessionNodesRequest>,
}

/// One omitted attachment part: its message, its index in it and the text
/// that replaces it in the request's view.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedOmission {
    pub message: String,
    pub part: usize,
    pub text: String,
}

impl TurnBuild {
    /// `omissions` as the turn records them.
    pub(in crate::runtime) fn record_omissions(
        omissions: &AttachmentOmissions,
    ) -> Vec<RecordedOmission> {
        omissions
            .iter()
            .map(|(id, text)| RecordedOmission {
                message: id.message.clone(),
                part: id.part,
                text: text.clone(),
            })
            .collect()
    }

    /// The recorded omissions as the policy applies them.
    pub(in crate::runtime) fn omissions(&self) -> AttachmentOmissions {
        self.omissions
            .iter()
            .map(|omission| {
                (
                    HistoryPartId {
                        message: omission.message.clone(),
                        part: omission.part,
                    },
                    omission.text.clone(),
                )
            })
            .collect()
    }
}

/// One row of a turn's journal: its build inputs, written once with the
/// turn's first phase commit, or the machine inputs a phase commit made
/// durable.
#[derive(serde::Deserialize)]
#[serde(untagged)]
pub(in crate::runtime) enum JournalEntry {
    Build { build: Box<TurnBuild> },
    Inputs(lash_sansio::JournalRow<HostIntent>),
}

/// The build row as the journal stores it.
pub(in crate::runtime) fn build_row(build: &TurnBuild) -> Result<String, String> {
    #[derive(serde::Serialize)]
    struct Row<'a> {
        build: &'a TurnBuild,
    }
    serde_json::to_string(&Row { build })
        .map_err(|error| format!("the turn's build inputs do not encode: {error}"))
}

/// A turn's journal read back: its build inputs, absent only for a host that
/// builds its machine itself, and its machine inputs in the order they were
/// appended.
pub(in crate::runtime) struct ReadJournal {
    pub(in crate::runtime) build: Option<TurnBuild>,
    pub(in crate::runtime) inputs: Vec<lash_sansio::MachineInput<HostIntent>>,
}

/// Decode the turn's journal `rows`, in the order the store returns them.
///
/// # Errors
///
/// The account of a row that does not decode, a build row past the
/// journal's first row, or machine inputs that lost an input.
pub(in crate::runtime) fn read_journal(rows: Vec<String>) -> Result<ReadJournal, String> {
    let mut build = None;
    let mut inputs = Vec::new();
    for (index, row) in rows.into_iter().enumerate() {
        match serde_json::from_str::<JournalEntry>(&row) {
            Ok(JournalEntry::Inputs(row)) => inputs.push(row),
            Ok(JournalEntry::Build { build: recorded }) if index == 0 => build = Some(*recorded),
            Ok(JournalEntry::Build { .. }) => {
                return Err("the turn's build inputs are not its journal's first row".to_owned());
            }
            Err(error) => return Err(format!("a journal row does not decode: {error}")),
        }
    }
    let inputs =
        lash_sansio::JournalRow::assemble(inputs).map_err(|divergence| divergence.to_string())?;
    Ok(ReadJournal { build, inputs })
}

/// Replay exactly the input prefix committed with a phase. The journal can
/// already contain a later phase when a reader races the serving owner.
pub(in crate::runtime) fn replay(
    machine: &mut crate::TurnMachine,
    mut inputs: Vec<lash_sansio::MachineInput<HostIntent>>,
    committed: usize,
    run: &lash_vocabulary::identity::TurnId,
    syncs: lash_sansio::RecordedSyncs<'_>,
) -> Result<(), super::session::TurnRestoreError> {
    if inputs.len() < committed {
        return Err(super::session::TurnRestoreError::Diverged {
            run: run.clone(),
            divergence: lash_sansio::ReplayDivergence {
                index: inputs.len(),
                reason: "the phase names inputs absent from its journal".to_owned(),
            },
        });
    }
    inputs.truncate(committed);
    machine.replay_inputs(inputs, syncs).map_err(|divergence| {
        super::session::TurnRestoreError::Diverged {
            run: run.clone(),
            divergence,
        }
    })?;
    machine.check_parked_state().map_err(Into::into)
}
