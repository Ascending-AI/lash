//! The session shift's contract: what one shift of a session answers, and
//! the scopes its recorded steps run under (FIG-3600, ADR 0104 O1/O2/O6).
//!
//! A shift admits a run with a recorded `AdmitShift` step, seals the
//! admission with a recorded `SealShiftAdmission` step, and executes the run's
//! turns to their terminal commit. Nothing here names an engine: an engine
//! runs the shift either in process (one controller, rescoped per step) or
//! split across its own handlers (admission in a per-session handler, each
//! run in a per-run one), and both reach the same kernel bodies.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use std::collections::BTreeSet;

use super::admission::{Admitted, AdmittedWork, ParkRef, SealRefusal, ShiftRequestId};
use super::contracts::ShiftRequest;
use crate::store::TurnCommitId;
use crate::{AdmittedScope, RuntimeError, SessionId, TurnId};

/// The prefix of the session-operation id a shift's admission steps are
/// recorded under: the scope gives the admission journal a session-bearing
/// address.
const SHIFT_ADMISSION_SCOPE_PREFIX: &str = "shift:";

/// Maximum runs admitted by one engine shift invocation before it hands
/// remaining work to a new request.
pub const MAX_RUNS_PER_SHIFT: usize = 64;

/// What a `SessionShifts` keeps open for one attempt of one shift invocation
/// ([`SessionShifts::hold_shift`](crate::runtime::work::SessionShifts::hold_shift)):
/// dropping it releases what it holds.
#[must_use = "a shift hold releases what it holds when it is dropped"]
pub struct ShiftHold(Option<Box<dyn Send>>);

impl ShiftHold {
    /// A hold on nothing: a `SessionShifts` that opens nothing per session.
    pub fn empty() -> Self {
        Self(None)
    }

    /// A hold on `held`, released when the hold is dropped.
    pub fn new(held: impl Send + 'static) -> Self {
        Self(Some(Box::new(held)))
    }
}

impl std::fmt::Debug for ShiftHold {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("ShiftHold")
            .field(&self.0.is_some())
            .finish()
    }
}

/// The request-id prefix of a shift's continuation invocations: every leg a
/// yielded shift hands off to is named under it, so a walk of a session's
/// shift requests can tell chain legs from new chain runs.
pub const SHIFT_CONTINUATION_PREFIX: &str = "shift-next:";

/// The stable, fixed-size request id of a shift's next invocation.
#[must_use]
pub fn shift_continuation_request(request: &ShiftRequest) -> ShiftRequestId {
    let mut digest = Sha256::new();
    digest.update((request.session.as_str().len() as u64).to_be_bytes());
    digest.update(request.session.as_str().as_bytes());
    digest.update(request.request.as_str().as_bytes());
    ShiftRequestId::new(format!(
        "{SHIFT_CONTINUATION_PREFIX}{:x}",
        digest.finalize()
    ))
}

/// The scope a shift's `AdmitShift` steps are recorded under: one per shift
/// request, so a redrive of the same request replays its admissions and a new
/// request admits afresh.
#[must_use]
pub fn shift_admission_scope(session: &SessionId, request: &ShiftRequestId) -> AdmittedScope {
    AdmittedScope::session_operation(
        session.clone(),
        format!("{SHIFT_ADMISSION_SCOPE_PREFIX}{}", request.as_str()),
    )
}

/// The scope an admitted run executes under: the run's own turn. Every turn a
/// shift runs is opened by its run, never by a session operation (FIG-3607
/// contract 4).
#[must_use]
pub fn shift_run_scope(session: &SessionId, run: &TurnId) -> AdmittedScope {
    AdmittedScope::turn(session.clone(), run.clone())
}

/// The replay key of admission `ordinal` of shift `request`, inside
/// [`shift_admission_scope`]. It names the request as well as the scope does,
/// so a journal keyed by replay key alone never serves one request's
/// admission to another.
#[must_use]
pub fn shift_admission_replay_key(request: &ShiftRequestId, ordinal: u32) -> String {
    format!("shift-admission:{}#{ordinal}", request.as_str())
}

/// The replay key of the start marker an execution of an admitted run draws
/// before its seal, inside [`shift_run_scope`] (ADR 0105 L-S8).
#[must_use]
pub fn shift_run_start_replay_key(admitted: &Admitted) -> String {
    format!("shift-run-start:{}", admitted.admission().as_str())
}

/// The replay key of an admitted run's seal, inside [`shift_run_scope`].
#[must_use]
pub fn shift_seal_replay_key(admitted: &Admitted) -> String {
    format!("shift-seal:{}", admitted.admission().as_str())
}

/// The replay key of a run's scope close, inside the run's scope: one
/// close per run, whichever admission ran it, so a redrive of a run that
/// already closed replays the recorded close.
#[must_use]
pub fn shift_close_run_replay_key(run: &TurnId) -> String {
    format!("shift-close:{}", run.as_str())
}

/// How one admitted run ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "run_outcome", rename_all = "snake_case")]
pub enum RunOutcome {
    /// The run's turns ran and its terminal commit landed. The answer is
    /// read from the run's durable terminal record, never from this reply.
    Committed {
        run: TurnId,
        kind: crate::store::RunTerminalKind,
        /// The final commit's retained decision about remaining session work.
        work_remaining: bool,
    },
    /// The run applied the session's open command run (ADR 0101 §4); it ran
    /// no turn.
    Applied { run: TurnId },
    /// The seal refused the admission (another admission superseded it, or
    /// the run started under a history this execution cannot read), so
    /// nothing ran.
    Refused { run: TurnId, refusal: SealRefusal },
    /// The work admission named was answered by another `SessionShifts` or withdrawn
    /// before the run took it, so nothing ran.
    Ceded { run: TurnId },
    /// The engine released the run's execution for good (an operator's
    /// cancel or fork killed it, or it ended terminally without a lash
    /// outcome). The shift goes on to its next admission, which reads what
    /// the store decided about the run.
    Released { run: TurnId },
}

impl RunOutcome {
    pub fn run(&self) -> &TurnId {
        match self {
            Self::Committed { run, .. }
            | Self::Applied { run }
            | Self::Refused { run, .. }
            | Self::Ceded { run }
            | Self::Released { run } => run,
        }
    }
}

/// How an engine's per-run handler execution of one admitted run ended
/// ([`SessionShifts::execute_run`](crate::runtime::work::SessionShifts::execute_run)):
/// the run's outcome, and the scope close its terminal commit left owed
/// (FIG-4035).
///
/// The run never closes the run's scope itself: a close the handler awaited
/// would hold the session's shift, and so the next run's admission, behind
/// it. The engine runs the owed close through
/// [`SessionShifts::close_run`](crate::runtime::work::SessionShifts::close_run)
/// in a journal of its own once it has the outcome.
#[derive(Debug)]
pub struct RunEnd {
    /// The run's outcome, or how the attempt ended without one.
    pub result: Result<RunOutcome, ShiftAbort>,
    /// The logical run whose terminal evidence the execution made durable: its
    /// scope close is owed. A durable fact, so every execution of the run
    /// answers it alike, and set however `result` ended. `None` for a run
    /// that ended no run, or on a host that owns no scopes.
    pub owed_close: Option<TurnId>,
}

impl RunEnd {
    /// A run that owes no scope close.
    pub fn owing_nothing(result: Result<RunOutcome, ShiftAbort>) -> Self {
        Self {
            result,
            owed_close: None,
        }
    }
}

/// Why a shift stopped admitting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stop", rename_all = "snake_case")]
pub enum ShiftStop {
    /// Admission found nothing to work. The shift re-checked before it
    /// stopped, so work admitted before this answer was executed.
    Idle,
    /// A parked run blocks the session; nothing is admitted until the park
    /// is resolved.
    Parked(ParkRef),
    /// The run admission would resume started under a history this
    /// execution cannot read. It is parked or abandoned, never re-run.
    SubstrateLost { run: TurnId },
    /// The run admission named already has its terminal (ADR 0105 L-S6).
    RunTerminal {
        run: TurnId,
        kind: crate::store::RunTerminalKind,
        commit: Option<TurnCommitId>,
    },
    /// A pending follow-on of `run` holds the session and admission could
    /// not resume it (ADR 0101 §3, FIG-3542).
    Blocked { run: TurnId },
    /// Admission named `run` again after the engine released its execution
    /// in this shift: the store still owes it work nothing will run. The
    /// shift stops rather than spin on it.
    RunAborted { run: TurnId },
    /// The shift stopped admitting with work possibly left: `run` ran
    /// nothing (its seal was superseded, or its queued run ceded), admission
    /// named it again after this shift already ran it, or the caller had what
    /// it waited for. Unlike [`Idle`](Self::Idle), admission did not answer
    /// that nothing is pending; the session's next ask to work re-checks.
    Yielded { run: TurnId },
    /// The engine's shift invocation ended at a run boundary and sent the
    /// rest of the shift to its continuation request
    /// ([`shift_continuation_request`]), after `run`: it reached its run
    /// bound, or it reached the boundary on an attempt that replayed. A
    /// waiter follows the continuation; this is one leg's stop, never a
    /// whole shift's.
    HandedOff { run: TurnId },
}

/// The stop rules every shift loop keeps, in process or split across an
/// engine's handlers: one copy, so no engine's loop can lose one.
///
/// - A run this shift already ran is not run again; what admission found
///   was left behind by that run, and the next ask answers it
///   ([`ShiftStop::Yielded`]).
/// - A run whose execution the engine released is consumed, and the shift
///   goes on; if admission names it again the shift stops
///   ([`ShiftStop::RunAborted`]) instead of calling it a second time.
/// - A run that ran nothing (a superseded seal, a queued-headed run that
///   ceded, or a command run admitted on the same leading command as the
///   command run before it, which applied nothing) stops the shift: another
///   `SessionShifts` holds the session, or the lane has nothing this shift can admit
///   now ([`ShiftStop::Yielded`]). Both are read from recorded admissions,
///   so a redrive stops where its first execution stopped.
/// - A command run whose execution the engine released after a refusal no
///   retry changes stops the shift too ([`ShiftStop::Yielded`]): the command
///   it met is still the lane's leading command, and every admission after
///   it would name that command under a new run and meet the same refusal
///   (FIG-4393). The refusal is the run's typed end; the session's next ask
///   to work tries the command again.
///
/// A shift an engine runs over several invocations keeps the rules across
/// each handoff: the leg that hands off sends what it remembers of its own
/// runs ([`handed_off`](Self::handed_off)), and the next leg starts from it.
/// A run the next admission names again is then met by the same rule,
/// whichever leg ran it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShiftLoop {
    ran: BTreeSet<TurnId>,
    released: BTreeSet<TurnId>,
    /// The leading command the last command run was admitted on.
    commands_head: Option<u64>,
}

impl ShiftLoop {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// What the leg a shift hands off to starts from, after this leg ran
    /// `leg`: the rules' memory of those runs alone, so what a shift carries
    /// stays bounded by one leg however long its chain grows.
    #[must_use]
    pub fn handed_off(&self, leg: &[RunOutcome]) -> Self {
        let leg: BTreeSet<&TurnId> = leg.iter().map(RunOutcome::run).collect();
        let kept = |runs: &BTreeSet<TurnId>| {
            runs.iter()
                .filter(|run| leg.contains(run))
                .cloned()
                .collect()
        };
        Self {
            ran: kept(&self.ran),
            released: kept(&self.released),
            commands_head: self.commands_head,
        }
    }

    /// Whether the shift runs `admitted`'s run: `Err(stop)` when it stops
    /// instead.
    pub fn before(&mut self, admitted: &Admitted) -> Result<(), ShiftStop> {
        let run = admitted.run();
        if self.released.contains(run) {
            return Err(ShiftStop::RunAborted { run: run.clone() });
        }
        if !self.ran.insert(run.clone()) {
            return Err(ShiftStop::Yielded { run: run.clone() });
        }
        Ok(())
    }

    /// How the shift goes on after a run of `work` ended `outcome`:
    /// `Some(stop)` when it stops.
    pub fn after(&mut self, work: &AdmittedWork, outcome: &RunOutcome) -> Option<ShiftStop> {
        match outcome {
            RunOutcome::Committed {
                work_remaining: false,
                ..
            } => Some(ShiftStop::Idle),
            RunOutcome::Committed {
                work_remaining: true,
                ..
            } => None,
            RunOutcome::Applied { run } => match work {
                AdmittedWork::Commands { head } if self.commands_head == Some(*head) => {
                    Some(ShiftStop::Yielded { run: run.clone() })
                }
                AdmittedWork::Commands { head } => {
                    self.commands_head = Some(*head);
                    None
                }
                _ => None,
            },
            RunOutcome::Ceded { run } => (!matches!(work, AdmittedWork::Input { .. }))
                .then(|| ShiftStop::Yielded { run: run.clone() }),
            RunOutcome::Refused { run, .. } => Some(ShiftStop::Yielded { run: run.clone() }),
            RunOutcome::Released { run } => {
                self.released.insert(run.clone());
                matches!(
                    work,
                    AdmittedWork::Commands { .. } | AdmittedWork::Operation { .. }
                )
                .then(|| ShiftStop::Yielded { run: run.clone() })
            }
        }
    }
}

/// What one shift of a session did: the runs it ran, in order, and why it
/// stopped.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShiftOutcome {
    pub ran: Vec<RunOutcome>,
    pub stop: ShiftStop,
}

/// Why a shift ended without an outcome. The engine decides what to do with
/// the attempt; nothing here is a recorded result.
#[derive(Debug, thiserror::Error)]
pub enum ShiftAbort {
    /// A live fault. The engine retries the attempt under its own policy,
    /// and the retry replays what was recorded.
    #[error("shift attempt failed and will be retried: {0}")]
    Retry(RuntimeError),
    /// The run parked (ADR 0104 O3): its park is durable, and the engine
    /// keeps its history for a restored build instead of retrying it.
    #[error("run `{run}` parked: {error}")]
    Parked {
        run: TurnId,
        error: Box<RuntimeError>,
    },
    /// A refusal no retry changes. The engine ends the attempt terminally.
    #[error("shift refused: {0}")]
    Refused(RuntimeError),
}

impl ShiftAbort {
    /// The error the abort carries, whatever its disposition.
    pub fn error(&self) -> &RuntimeError {
        match self {
            Self::Retry(error) | Self::Refused(error) => error,
            Self::Parked { error, .. } => error,
        }
    }

    pub fn into_error(self) -> RuntimeError {
        match self {
            Self::Retry(error) | Self::Refused(error) => error,
            Self::Parked { error, .. } => *error,
        }
    }
}

/// Constructors for the admission executor alone.
///
/// An [`Admitted`] is minted only by the body of a recorded `AdmitShift`
/// step, whose recorded verdict every replay decodes. No other caller may
/// build one.
#[doc(hidden)]
pub mod admission_body {
    use super::super::admission::{AdmissionId, Admitted, ShiftRequestId};
    use crate::SessionId;

    #[must_use]
    pub fn admitted(
        session: SessionId,
        request: ShiftRequestId,
        admission: AdmissionId,
        receipt: lash_core_store::store::ShiftAdmissionReceipt,
    ) -> Admitted {
        Admitted::minted(session, request, admission, receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command run the engine released after a refusal stops the shift:
    /// the admission after it would name the same leading command under a
    /// new run and meet the same refusal, without end (FIG-4393).
    #[test]
    fn a_released_command_run_stops_the_shift() {
        let mut rules = ShiftLoop::new();
        let run = TurnId::from("shift-commands:admission-0");
        assert_eq!(
            rules.after(
                &AdmittedWork::Commands { head: 7 },
                &RunOutcome::Released { run: run.clone() },
            ),
            Some(ShiftStop::Yielded { run }),
        );
    }

    /// A released turn run is consumed and the shift goes on: admission
    /// answers what the store decided about it.
    #[test]
    fn a_released_turn_run_lets_the_shift_go_on() {
        let mut rules = ShiftLoop::new();
        assert_eq!(
            rules.after(
                &AdmittedWork::Queued {
                    head: crate::BatchId::from("qwb:head"),
                },
                &RunOutcome::Released {
                    run: TurnId::from("shift-run:admission-0"),
                },
            ),
            None,
        );
    }

    fn admitted(run: &TurnId) -> Admitted {
        let receipt_session: SessionId = SessionId::from("s");
        let receipt_admission = super::super::admission::AdmissionId::new("r:0");
        admission_body::admitted(
            receipt_session.clone(),
            ShiftRequestId::new("r"),
            receipt_admission.clone(),
            lash_core_store::store::ShiftAdmissionReceipt {
                selection: lash_core_store::store::ShiftAdmissionSelection {
                    run: run.clone(),
                    work: AdmittedWork::Queued {
                        head: crate::BatchId::from("qwb:head"),
                    },
                    observed_epoch: 0,
                },
                run_start: lash_core_store::store::RunStartNonce::new(receipt_admission.as_str()),
                seal: lash_core_store::store::ShiftEpochSeal::Sealed(
                    lash_core_store::store_backend_support::sealed_shift_fence(
                        receipt_session.clone(),
                        1,
                        receipt_admission.clone(),
                    ),
                ),
                run_admission: None,
            },
        )
    }

    /// S04 F2: an executable admission cannot exist without its recorded receipt.
    #[test]
    fn an_admission_without_its_receipt_is_refused() {
        let run = TurnId::from("receipt-run");
        let mut body = serde_json::to_value(admitted(&run)).expect("encode admission");
        body.as_object_mut()
            .expect("admission object")
            .remove("receipt");
        assert!(serde_json::from_value::<Admitted>(body.clone()).is_err());
        body["receipt"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<Admitted>(body).is_err());

        let selected = admitted(&run);
        let body = serde_json::to_value(&selected).expect("encode admission");
        for copy in ["run", "work", "observed_epoch", "root"] {
            assert!(
                body.get(copy).is_none(),
                "selection is stored only in the receipt"
            );
        }
        let decoded: Admitted = serde_json::from_value(body).expect("receipt admission");
        assert_eq!(decoded.run(), &decoded.root().selection.run);
        assert_eq!(decoded.work(), &decoded.root().selection.work);
        assert_eq!(
            decoded.observed_epoch(),
            decoded.root().selection.observed_epoch
        );
    }

    /// The rules outlive a handoff (FIG-4523): the leg a shift continues on
    /// starts from what the leg before it remembers of its own runs, so a
    /// released run admission names again stops the shift there, and a run
    /// of an earlier leg is no longer carried.
    #[test]
    fn the_leg_a_shift_hands_off_to_keeps_the_rules_of_the_leg_before_it() {
        let earlier = TurnId::from("earlier");
        let released = TurnId::from("released");
        let mut rules = ShiftLoop::new();
        rules
            .before(&admitted(&earlier))
            .expect("a new run executes");
        let first = rules.handed_off(&[RunOutcome::Ceded {
            run: earlier.clone(),
        }]);

        let mut rules = first;
        assert_eq!(
            rules.before(&admitted(&earlier)),
            Err(ShiftStop::Yielded {
                run: earlier.clone()
            }),
            "the leg before ran it"
        );
        rules
            .before(&admitted(&released))
            .expect("a new run executes");
        let outcome = RunOutcome::Released {
            run: released.clone(),
        };
        assert_eq!(rules.after(admitted(&released).work(), &outcome), None);
        let mut next = rules.handed_off(std::slice::from_ref(&outcome));
        assert_eq!(
            next.before(&admitted(&released)),
            Err(ShiftStop::RunAborted { run: released }),
        );
        next.before(&admitted(&earlier))
            .expect("only the handing-off leg's runs are carried");
        assert_eq!(
            serde_json::from_value::<ShiftLoop>(serde_json::to_value(&next).expect("encode"))
                .expect("decode"),
            next
        );
    }
}
