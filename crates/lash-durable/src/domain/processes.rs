//! Process actor rows (L6): the process actor and registry row, its state
//! revision, cancel, terminal and cascade cursor (ADR 0132 §10, §11).
//!
//! The registry row is the process's one row: its outcome, its lifetime
//! scope and its first cancel request are the registry's, and the actor
//! columns beside them (state revision, the driver's pending action, the
//! cascade cursor) are the process actor's. The engine's state is the
//! process's snapshot, `p/<pid>` in the snapshots domain, and moves in the
//! same commit as the state revision.

use crate::ids::{DurableInstant, Epoch};
use lash_protocol_types::CancelOrigin;
use lash_vocabulary::identity::ProcessId;

use super::keys::ScopeKey;

/// One process actor's row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessActorRow {
    /// The process.
    pub process: ProcessId,
    /// Its engine state's revision; advances with every committed
    /// transition. Zero before the first.
    pub state_rev: u64,
    /// The process driver's state, encoded by its owner: what the last
    /// committed transition left in flight. `None` before the first.
    pub driver_json: Option<String>,
    /// When its cancel was first requested.
    pub cancel_requested_at: Option<DurableInstant>,
    /// Whether it is terminal.
    pub terminal: bool,
    /// The terminal's cascade cursor while `Until` children remain to be
    /// marked: the last child marked, or empty before the first batch.
    pub cascade_cursor: Option<String>,
    /// The epoch of the commit that last wrote it.
    pub written_epoch: Option<Epoch>,
    /// The last event of its log its owners handed to their hosts' sinks:
    /// where publication resumes after a takeover. Zero before the first.
    pub published_event_sequence: u64,
}

/// What a process owner decides one pass from, read with its fenced open
/// in one snapshot ([`DurableStore::open_process`](crate::DurableStore::open_process)):
/// the process's actor row, its registry record as stored, and the
/// execution root its engine state is published under. Nothing is decoded
/// past the store's own columns, so the owner meets each refusal where it
/// decides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessView {
    /// The process's actor row; `None` once it is gone.
    pub row: Option<ProcessActorRow>,
    /// Its registry record.
    pub registry: ProcessRegistryRow,
    /// Its engine state's execution root row; `None` before its first
    /// transition.
    pub root: Option<super::ExecRootRow>,
}

/// Externally writable control observed independently of retained process state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessFrontier {
    /// Registry event metadata changes independently of actor revision.
    pub last_event_sequence: u64,
    pub updated_at_ms: u64,
    pub cancel: Option<lash_protocol_types::CancelRequest>,
    /// The registry encoding of the current external execution reference.
    pub external_ref: Option<String>,
}

/// A process's registry record as a [`ProcessView`] reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessRegistryRow {
    /// The retained record, as its registry encoded it.
    Retained(String),
    /// The record was pruned: its tombstone's terminal label and when.
    Pruned {
        /// The terminal it ended with.
        terminal_label: String,
        /// When it was pruned, in store milliseconds.
        pruned_at_ms: i64,
    },
    /// No record and no tombstone.
    Absent,
}

/// The rows a process start writes: its registry row, its observers and
/// its actor, ready, in the transaction that records the outcome of the
/// call that started it (ADR 0132 §5). A start whose key a process already
/// holds writes nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessStartRows {
    /// The process.
    pub process: ProcessId,
    /// The prepared registration (its observers, the minted process id and
    /// the instant it was prepared at), encoded by its owner.
    pub registration_json: String,
    /// Validated linked execution membership for this new process.
    pub execution: Option<Box<crate::resolutions::ExecutionRegistration>>,
    /// Validated inherited pins, committed before the process is claimable.
    pub initial_resolutions: Vec<crate::resolutions::ResolutionRecord>,
}

/// A process write inside an owner commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessWrite {
    /// Register a process: its registry row, its observers and its actor,
    /// ready. Refused with
    /// [`DomainRefusal::ProcessStartRefused`](super::DomainRefusal::ProcessStartRefused)
    /// when the registrar refuses the start.
    Register(ProcessStartRows),
    /// Commit one engine transition: the state revision moves from
    /// `expected_rev` to `expected_rev + 1` and the driver state is
    /// replaced. Refused with
    /// [`DomainRefusal::ProcessRevConflict`](super::DomainRefusal::ProcessRevConflict)
    /// when the stored revision is not `expected_rev`.
    Advance {
        /// The process.
        process: ProcessId,
        /// The revision replaced.
        expected_rev: u64,
        /// The driver state after the transition.
        driver_json: String,
    },
    /// Record a whole component format postimage without advancing the engine.
    ComponentFormats {
        process: ProcessId,
        expected_rev: u64,
        driver_json: String,
    },
    /// Record that the owner's host sinks were handed `process`'s log
    /// through sequence `through`. The mark only moves forward: an older
    /// one leaves it as it is.
    Published {
        /// The process.
        process: ProcessId,
        /// The last event handed to the sinks.
        through: u64,
    },
    /// Append one lifecycle event, encoded by its owner, exactly once: the
    /// replay key makes a repeat of the same commit a no-op.
    AppendEvent {
        /// The process.
        process: ProcessId,
        /// The event's type.
        event_type: String,
        /// Its payload.
        payload_json: String,
        /// Its replay key.
        replay_key: String,
    },
    /// End the process with its terminal, encoded by its owner. A process
    /// already terminal keeps its first terminal. The cascade over its
    /// `Until` children starts: its cursor is set, empty.
    Terminal {
        /// The process.
        process: ProcessId,
        /// The terminal outcome.
        outcome_json: String,
    },
    /// Mark `children` (live `Until(scope)` children whose cancel was not
    /// yet requested) for cancel with `origin`: each records its first
    /// cancel request, gets a cancel mail and a control wake. When `scope`
    /// is the committing process, its cursor moves to `cursor`; `None`
    /// once the cascade is done.
    CascadeBatch {
        /// The ending scope.
        scope: ScopeKey,
        /// The children this batch marks.
        children: Vec<ProcessId>,
        /// Why they are cancelled.
        origin: CancelOrigin,
        /// Who asked, for the children's cancel requests.
        requester: String,
        /// The cursor after this batch.
        cursor: Option<String>,
    },
    /// Close `scope`: its closure fact, the registry's `parent_end_plans`
    /// row, commits with the transaction that ended it, and from then on
    /// every registration under the scope, or inside it, is refused. On
    /// PostgreSQL the write takes the scope's advisory lock first, which a
    /// registration deciding the same scope also holds, so the start either
    /// commits before the closure or reads it. The scope's `Until` children
    /// are read after that, in the transaction: when a turn or operation
    /// scope still has one unmarked once this transaction's batch is
    /// applied, the scope is recorded as ending, so the session marks the
    /// rest. Closing a
    /// closed scope keeps its first fact.
    ScopeClosed {
        /// The closed scope.
        scope: ScopeKey,
    },
}

/// A request to cancel a process, from outside its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelRequest {
    /// The process.
    pub process: ProcessId,
    /// Why.
    pub origin: CancelOrigin,
    /// Who asked.
    pub requester: String,
}

/// The answer to a [`CancelRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelAnswer {
    /// The first request: recorded at `at`, a cancel mail appended and the
    /// process control-woken.
    Requested {
        /// When.
        at: DurableInstant,
    },
    /// An earlier request stands, with its own timestamp; the process was
    /// control-woken again.
    AlreadyRequested {
        /// The earlier request's time.
        at: DurableInstant,
    },
    /// The process is already terminal.
    AlreadyEnded,
}

/// A request to redrive a parked actor, from an operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedriveRequest {
    /// The actor.
    pub actor: crate::ids::ActorKey,
    /// Who asked.
    pub requester: String,
}

/// A node's boot redriving the session parks a change of build may clear
/// (FIG-6310): every session actor parked with one of [`Self::reasons`] is
/// redriven as an operator's [`RedriveRequest`] would redrive it, its park
/// and failed activations cleared and the actor control-woken, each only
/// while the park its candidate read found still stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionParksRedrive {
    /// The park reasons it redrives: the `reason` tag of the park its owner
    /// encoded.
    pub reasons: Vec<String>,
}

impl SessionParksRedrive {
    /// Whether it redrives a session parked with `park`, as its owner
    /// encoded it: a JSON object whose `reason` is one of
    /// [`Self::reasons`]. A park that does not decode stays parked.
    #[must_use]
    pub fn redrives(&self, park: &str) -> bool {
        serde_json::from_str::<serde_json::Value>(park).is_ok_and(|park| {
            park.get("reason")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|reason| self.reasons.iter().any(|listed| listed == reason))
        })
    }
}

/// The answer to a [`RedriveRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedriveAnswer {
    /// It was parked: its park and its failed activations are cleared, the
    /// feed records the redrive, and it is ready.
    Redriven,
    /// It was not parked; nothing was written.
    NotParked,
}

/// One transaction's process acknowledgement. The owner fence authenticates the
/// preimage; successful guarded writes update fields and registry reducers supply
/// their realized record, including store-generated metadata.
#[derive(Default)]
pub struct ProcessPostimages(std::sync::Mutex<Option<ProcessView>>);

impl ProcessPostimages {
    pub fn of(tx: &crate::ActorTx) -> Self {
        Self(std::sync::Mutex::new(tx.process_preimage().cloned()))
    }

    pub fn from_view(view: Option<ProcessView>) -> Self {
        Self(std::sync::Mutex::new(view))
    }

    pub fn registry(&self, process: &ProcessId, record: String) {
        let mut held = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(view) = held.as_mut()
            && view.row.as_ref().is_some_and(|row| row.process == process)
        {
            view.registry = ProcessRegistryRow::Retained(record);
        }
    }

    pub fn terminal(&self, process: &ProcessId, ended: bool, epoch: Epoch) {
        if !ended {
            return;
        }
        let mut held = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(row) = held.as_mut().and_then(|view| view.row.as_mut())
            && row.process == process
        {
            row.terminal = true;
            row.cascade_cursor = Some(String::new());
            row.written_epoch = Some(epoch);
        }
    }

    /// Called only after the backend has applied the matching guarded write.
    pub fn applied(&self, write: &super::DomainWrite, epoch: Epoch) {
        use super::{DomainWrite, ExecKey, ExecRootRow, ExecRootWrite, SnapshotRev};
        let mut held = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(view) = held.as_mut() else { return };
        let Some(row) = view.row.as_mut() else { return };
        match write {
            DomainWrite::Process(ProcessWrite::Advance {
                process,
                expected_rev,
                driver_json,
            })
            | DomainWrite::Process(ProcessWrite::ComponentFormats {
                process,
                expected_rev,
                driver_json,
            }) if process == row.process => {
                row.state_rev = expected_rev
                    + u64::from(matches!(
                        write,
                        DomainWrite::Process(ProcessWrite::Advance { .. })
                    ));
                row.driver_json = Some(driver_json.clone());
                row.written_epoch = Some(epoch);
            }
            DomainWrite::Process(ProcessWrite::Published { process, through })
                if process == row.process =>
            {
                row.published_event_sequence = row.published_event_sequence.max(*through);
            }
            DomainWrite::Process(ProcessWrite::CascadeBatch {
                scope: ScopeKey::Process(process),
                cursor,
                ..
            }) if process == row.process => {
                row.cascade_cursor = cursor.clone();
                row.written_epoch = Some(epoch);
            }
            DomainWrite::ExecRoot(ExecRootWrite::Put {
                exec: ExecKey::Process(process),
                expected,
                root,
                commit_id,
                ..
            }) if process == row.process => {
                view.root = Some(ExecRootRow {
                    exec: ExecKey::Process(process.clone()),
                    rev: SnapshotRev(expected.map_or(1, |rev| rev.0 + 1)),
                    root: *root,
                    written_epoch: epoch,
                    commit_id: *commit_id,
                });
            }
            DomainWrite::ExecRoot(
                ExecRootWrite::Clear {
                    exec: ExecKey::Process(process),
                }
                | ExecRootWrite::Retire {
                    exec: ExecKey::Process(process),
                    ..
                },
            ) if process == row.process => view.root = None,
            _ => {}
        }
    }

    pub fn into_views(self) -> Vec<ProcessView> {
        self.0
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .into_iter()
            .collect()
    }
}
