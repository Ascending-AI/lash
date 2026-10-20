//! What a follower of an input or a run decides from: one keyed snapshot
//! (FIG-7045).
//!
//! A follower resolves its subject on every wake. Each resolution reads
//! [`RunStore::follow_facts`](super::RunStore::follow_facts) once: the input's
//! binding, lifecycle, forsake and session fault, the bound run's terminal
//! evidence (or its session's deletion), a plugin operation's queued batch,
//! and the session actor's park, all from one statement and so one
//! snapshot. Nothing is cached between resolutions: a fork or release can
//! rebind an input, so every resolution reads its binding afresh.
//!
//! Backends read the row; [`StoredFollowRow::decode`] decides what it says.
//! It validates the columns in the order a follower's resolution consults
//! them (the binding and the fault, then the run's terminal) and leaves the
//! park undecoded, so stored corruption is refused exactly where a
//! resolution reaches it.

use super::control_intent::ControlIntent;
use super::{RunTerminal, SessionFault, StoreError};
use crate::tool_run::operation::OperationRun;
use crate::turn_input_vocabulary::TurnInputStateKind;
use crate::{InputId, SessionId, TurnId};

/// What a follower follows, as the store keys it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FollowSubject {
    /// An accepted input; its bound run's facts come with it.
    Input(InputId),
    /// A logical run, by id.
    Run(TurnId),
}

impl FollowSubject {
    /// The input this subject names, if it names one.
    #[must_use]
    pub fn input(&self) -> Option<&InputId> {
        match self {
            Self::Input(input) => Some(input),
            Self::Run(_) => None,
        }
    }

    /// The run this subject names, if it names one.
    #[must_use]
    pub fn run(&self) -> Option<&TurnId> {
        match self {
            Self::Input(_) => None,
            Self::Run(run) => Some(run),
        }
    }

    /// The batch of the plugin operation a run subject of `session_id`
    /// names: the key of its queued-batch fact.
    #[must_use]
    pub fn operation_batch(&self, session_id: &SessionId) -> Option<String> {
        OperationRun::for_run_id(session_id.clone(), self.run()?)
            .map(|operation| operation.operation_id)
    }
}

/// Everything a follower's resolution consults, from one snapshot
/// ([`RunStore::follow_facts`](super::RunStore::follow_facts)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FollowFacts {
    Input(InputFollowFacts),
    Run(RunFollowFacts),
}

/// An accepted input's facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputFollowFacts {
    /// The run the input is bound to, with that run's facts: the run its
    /// admission bound it to, the run whose commit applied it, or the run a
    /// fork rebound it to. `None` while no run took it.
    pub run: Option<RunFollowFacts>,
    /// Whether the input's row is still open: neither completed nor
    /// cancelled. An input with no row is not open.
    pub open: bool,
    /// Whether the host forsook the input's id before any input was filed
    /// under it. A forsaken input has no row, so it is never open and never
    /// bound.
    pub forsaken: bool,
    /// The session's standing fault (ADR 0109 §9).
    pub fault: Option<SessionFault>,
    /// Whether the input left the queue without any run taking it, with its
    /// withdrawal on record: no run is bound to it, its row is settled, and
    /// its session is live. An input with no row, or in a session the catalog
    /// does not hold (deleted, or never created), is not withdrawn: nothing
    /// of it is on record.
    pub withdrawn: bool,
}

/// A logical run's facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunFollowFacts {
    pub run: TurnId,
    /// The run's terminal evidence. A run of a deleted session with no row
    /// answers the session's deletion
    /// ([`RunTerminalCause::SessionDeleted`](super::RunTerminalCause::SessionDeleted)).
    pub terminal: Option<RunTerminal>,
    /// For a plugin operation's run followed by its run id: whether the
    /// operation's batch is still queued. `None` for any other run, and for
    /// a run an input subject is bound to: a follower of an input bound to an
    /// operation's run follows that run by its id.
    pub operation_pending: Option<bool>,
    /// The session actor's stored park reason while the actor is parked, as
    /// stored: the facade owns its type and decodes it only when its
    /// resolution reaches the park.
    pub park: Option<String>,
}

/// A run's terminal-evidence columns, as a backend stored them: every
/// column `NULL` while the run has none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoredTerminalColumns {
    pub cause_json: Option<String>,
    pub head_revision: Option<i64>,
    pub at_ms: Option<i64>,
    pub kind: Option<String>,
}

impl StoredTerminalColumns {
    /// Decode the terminal of `run` of `session_id` these columns hold.
    ///
    /// # Errors
    ///
    /// [`StoreError::StoredDataCorrupt`] for partial evidence, a negative
    /// counter, an undecodable cause, or a kind that disagrees with the
    /// cause.
    pub fn decode(
        self,
        session_id: &SessionId,
        run: &TurnId,
    ) -> Result<Option<RunTerminal>, StoreError> {
        let (cause_json, at_ms, kind) = match (self.cause_json, self.at_ms, self.kind) {
            (None, None, None) if self.head_revision.is_none() => return Ok(None),
            (Some(cause), Some(at), Some(kind)) => (cause, at, kind),
            _ => return Err(corrupt("RunTerminal", "partial terminal evidence")),
        };
        let terminal = RunTerminal::from_stored(
            session_id.clone(),
            run.clone(),
            &cause_json,
            self.head_revision
                .map(|revision| stored_u64("RunTerminal", revision))
                .transpose()?,
            stored_u64("RunTerminal", at_ms)?,
        )?;
        if terminal.kind().as_str() != kind {
            return Err(corrupt("RunTerminal", "terminal kind disagrees with cause"));
        }
        Ok(Some(terminal))
    }
}

/// A control intent's columns, as a backend stored them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredControlIntent {
    pub id: i64,
    pub session_id: String,
    pub format: i64,
    pub kind_json: String,
    pub state_json: String,
    pub created_at_ms: i64,
}

impl StoredControlIntent {
    /// # Errors
    ///
    /// [`StoreError::StoredDataCorrupt`] for a column out of range or a
    /// format this build does not read.
    pub fn decode(self) -> Result<ControlIntent, StoreError> {
        ControlIntent::from_stored(
            stored_u64("ControlIntent", self.id)?,
            SessionId::parse(self.session_id)?,
            u32::try_from(self.format)
                .map_err(|_| corrupt("ControlIntent", "format out of range"))?,
            &self.kind_json,
            &self.state_json,
            stored_u64("ControlIntent", self.created_at_ms)?,
        )
    }
}

/// The follow snapshot's columns, as a backend read them in one statement.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoredFollowRow {
    /// The input's bound run.
    pub binding: Option<String>,
    /// The input row's stored lifecycle state; `None` with no row.
    pub input_state: Option<String>,
    pub open: bool,
    pub fault_json: Option<String>,
    pub fault_at_ms: Option<i64>,
    pub forsaken: bool,
    /// The session's deletion tombstone stands.
    pub session_deleted: bool,
    /// The session has a meta or head row.
    pub session_materialized: bool,
    /// The followed run has a `session_runs` row.
    pub run_row: bool,
    pub terminal: StoredTerminalColumns,
    /// The session's first `close_session` intent.
    pub close_intent: Option<StoredControlIntent>,
    /// An operation subject's batch is open in the queue.
    pub operation_pending: bool,
    /// The session actor's stored park while its state is parked.
    pub park_json: Option<String>,
}

impl StoredFollowRow {
    /// What the row says about `subject` of `session_id`.
    ///
    /// # Errors
    ///
    /// [`StoreError::StoredDataCorrupt`] for a column the resolution
    /// consults that holds no value any build writes, in the resolution's
    /// order: an input's binding and its session's fault, then an unbound
    /// input's lifecycle state, then the run's terminal evidence and the
    /// deletion intent that stands in for it.
    pub fn decode(
        self,
        session_id: &SessionId,
        subject: &FollowSubject,
    ) -> Result<FollowFacts, StoreError> {
        match subject {
            FollowSubject::Run(run) => {
                let operation = subject.operation_batch(session_id).is_some();
                self.run_facts(session_id, run.clone(), operation)
                    .map(FollowFacts::Run)
            }
            FollowSubject::Input(_) => {
                let binding = self
                    .binding
                    .clone()
                    .map(TurnId::parse)
                    .transpose()
                    .map_err(StoreError::from)?;
                let fault = SessionFault::from_stored_columns(
                    session_id,
                    self.fault_json.clone(),
                    self.fault_at_ms,
                )?;
                let (open, forsaken) = (self.open, self.forsaken);
                let session_live = self.session_materialized && !self.session_deleted;
                let withdrawn = binding.is_none() && !open && session_live && {
                    match self.input_state.as_deref() {
                        None => false,
                        Some(state) => TurnInputStateKind::from_wire_str(state)
                            .ok_or_else(|| StoreError::StoredDataCorrupt {
                                record_kind: "PendingTurnInput",
                                message: format!("unknown turn input state `{state}`"),
                            })?
                            .is_terminal(),
                    }
                };
                let run = binding
                    .map(|run| self.run_facts(session_id, run, false))
                    .transpose()?;
                Ok(FollowFacts::Input(InputFollowFacts {
                    run,
                    open,
                    forsaken,
                    fault,
                    withdrawn,
                }))
            }
        }
    }

    fn run_facts(
        self,
        session_id: &SessionId,
        run: TurnId,
        operation: bool,
    ) -> Result<RunFollowFacts, StoreError> {
        let terminal = if self.run_row {
            self.terminal.decode(session_id, &run)?
        } else if self.session_deleted {
            self.close_intent
                .map(StoredControlIntent::decode)
                .transpose()?
                .and_then(|intent| intent.session_deleted_terminal(&run))
        } else {
            None
        };
        Ok(RunFollowFacts {
            run,
            terminal,
            operation_pending: operation.then_some(self.operation_pending),
            park: self.park_json,
        })
    }
}

fn corrupt(record_kind: &'static str, message: &str) -> StoreError {
    StoreError::StoredDataCorrupt {
        record_kind,
        message: message.to_owned(),
    }
}

fn stored_u64(record_kind: &'static str, value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| corrupt(record_kind, "a negative counter"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{
        ControlIntentKind, ControlIntentState, RunTerminalCause, stored_intent_kind,
        stored_intent_state,
    };

    fn session() -> SessionId {
        SessionId::from("follow")
    }

    fn committed() -> StoredTerminalColumns {
        StoredTerminalColumns {
            cause_json: Some(r#"{"cause":"commands_applied"}"#.to_owned()),
            head_revision: Some(1),
            at_ms: Some(7),
            kind: Some("answered".to_owned()),
        }
    }

    fn partial() -> StoredTerminalColumns {
        StoredTerminalColumns {
            kind: Some("answered".to_owned()),
            ..StoredTerminalColumns::default()
        }
    }

    fn close_intent(kind_json: &str) -> StoredControlIntent {
        StoredControlIntent {
            id: 3,
            session_id: "follow".to_owned(),
            format: 1,
            kind_json: kind_json.to_owned(),
            state_json: stored_intent_state(&ControlIntentState::Pending)
                .expect("state")
                .1,
            created_at_ms: 11,
        }
    }

    fn closing() -> StoredControlIntent {
        close_intent(
            &stored_intent_kind(&ControlIntentKind::CloseSession { runs: Vec::new() })
                .expect("kind"),
        )
    }

    fn corrupt_kind(result: Result<FollowFacts, StoreError>) -> &'static str {
        match result {
            Err(StoreError::StoredDataCorrupt { record_kind, .. }) => record_kind,
            other => panic!("expected a corruption refusal, got {other:?}"),
        }
    }

    fn input() -> FollowSubject {
        FollowSubject::Input(InputId::from("input"))
    }

    fn run() -> FollowSubject {
        FollowSubject::Run(TurnId::from("run"))
    }

    /// FOLLOW-FACTS-PRECEDENCE: the snapshot refuses stored corruption only
    /// where a resolution consults it, in the order it consults it: an
    /// input's fault before its run's terminal, an unbound input's lifecycle
    /// only in a live session, a deletion intent only for a run of a deleted
    /// session with no row, and a session fault never for a run subject.
    #[test]
    fn follow_facts_refuse_corruption_where_a_resolution_reaches_it() {
        let bound = StoredFollowRow {
            binding: Some("run".to_owned()),
            run_row: true,
            terminal: partial(),
            fault_json: Some("not a fault".to_owned()),
            ..StoredFollowRow::default()
        };
        assert_eq!(
            corrupt_kind(bound.clone().decode(&session(), &input())),
            "SessionFault",
            "an input's fault is decoded before its run's terminal"
        );
        let bound_run_corrupt = StoredFollowRow {
            fault_json: None,
            ..bound.clone()
        };
        assert_eq!(
            corrupt_kind(bound_run_corrupt.clone().decode(&session(), &input())),
            "RunTerminal"
        );
        assert_eq!(
            corrupt_kind(bound.clone().decode(&session(), &run())),
            "RunTerminal",
            "a run subject never consults the session fault"
        );

        let unbound = StoredFollowRow {
            input_state: Some("no such state".to_owned()),
            session_materialized: true,
            ..StoredFollowRow::default()
        };
        assert_eq!(
            corrupt_kind(unbound.clone().decode(&session(), &input())),
            "PendingTurnInput"
        );
        let FollowFacts::Input(deleted) = StoredFollowRow {
            session_deleted: true,
            ..unbound
        }
        .decode(&session(), &input())
        .expect("a deleted session's input lifecycle is never consulted") else {
            panic!("an input subject answers input facts");
        };
        assert!(!deleted.withdrawn && deleted.run.is_none());

        let FollowFacts::Run(ended) = StoredFollowRow {
            run_row: true,
            terminal: committed(),
            session_deleted: true,
            close_intent: Some(close_intent("corrupt")),
            ..StoredFollowRow::default()
        }
        .decode(&session(), &run())
        .expect("a run with a row answers from it, not from the deletion intent") else {
            panic!("a run subject answers run facts");
        };
        assert_eq!(
            ended.terminal.map(|terminal| terminal.cause),
            Some(RunTerminalCause::CommandsApplied)
        );
        assert_eq!(
            corrupt_kind(
                StoredFollowRow {
                    session_deleted: true,
                    close_intent: Some(close_intent("corrupt")),
                    ..StoredFollowRow::default()
                }
                .decode(&session(), &run())
            ),
            "ControlIntent"
        );
    }

    /// FOLLOW-FACTS-CLASSIFY: absence, withdrawal, deletion and park are
    /// told apart from one row.
    #[test]
    fn follow_facts_tell_absence_withdrawal_deletion_and_park_apart() {
        let live = StoredFollowRow {
            session_materialized: true,
            ..StoredFollowRow::default()
        };
        let facts = |row: StoredFollowRow, subject: &FollowSubject| {
            row.decode(&session(), subject)
                .expect("a sound row decodes")
        };
        let input_facts = |row: StoredFollowRow| match facts(row, &input()) {
            FollowFacts::Input(input) => input,
            FollowFacts::Run(_) => panic!("an input subject answers input facts"),
        };
        let run_facts = |row: StoredFollowRow, subject: &FollowSubject| match facts(row, subject) {
            FollowFacts::Run(run) => run,
            FollowFacts::Input(_) => panic!("a run subject answers run facts"),
        };
        assert!(
            !input_facts(live.clone()).withdrawn,
            "no row: nothing on record"
        );
        assert!(
            input_facts(StoredFollowRow {
                input_state: Some("cancelled".to_owned()),
                ..live.clone()
            })
            .withdrawn
        );
        assert!(
            !input_facts(StoredFollowRow {
                input_state: Some("cancelled".to_owned()),
                session_materialized: false,
                ..live.clone()
            })
            .withdrawn,
            "an uncreated session holds no withdrawal"
        );
        assert!(
            !input_facts(StoredFollowRow {
                binding: Some("run".to_owned()),
                input_state: Some("cancelled".to_owned()),
                ..live.clone()
            })
            .withdrawn,
            "a bound input is answered by its run"
        );

        let deleted = run_facts(
            StoredFollowRow {
                session_deleted: true,
                close_intent: Some(closing()),
                ..StoredFollowRow::default()
            },
            &run(),
        );
        assert!(matches!(
            deleted.terminal.map(|terminal| terminal.cause),
            Some(RunTerminalCause::SessionDeleted { .. })
        ));
        assert_eq!(
            run_facts(
                StoredFollowRow {
                    close_intent: Some(closing()),
                    ..StoredFollowRow::default()
                },
                &run()
            )
            .terminal,
            None,
            "a live session's run with no row has not ended"
        );

        let parked = run_facts(
            StoredFollowRow {
                run_row: true,
                park_json: Some("{}".to_owned()),
                ..live.clone()
            },
            &run(),
        );
        assert_eq!(
            (parked.terminal, parked.park),
            (None, Some("{}".to_owned()))
        );
        assert_eq!(parked.operation_pending, None);
        let operation = FollowSubject::Run(TurnId::from("shift-operation:op"));
        assert_eq!(operation.operation_batch(&session()).as_deref(), Some("op"));
        assert_eq!(
            run_facts(
                StoredFollowRow {
                    operation_pending: true,
                    ..live
                },
                &operation
            )
            .operation_pending,
            Some(true)
        );
    }
}
