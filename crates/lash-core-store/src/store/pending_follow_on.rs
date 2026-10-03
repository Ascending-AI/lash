//! The pending follow-on: the next physical turn a committed turn owes its
//! logical run, recorded on the session head (ADR 0101 §3, FIG-3542).
//!
//! A turn that switches agent frame commits the switch and the obligation to
//! run the switched frame's task in one head write. A turn that ends at a
//! segment boundary of its run (FIG-4739) commits the same way: what it
//! owes is the run's continuation, in the frame it ran in, which a new
//! invocation runs. The obligation is this
//! fact, in its own head column (`pending_follow_on_json`), never an ingress
//! row: nothing can admit it, reorder it, cancel it or render it into another
//! frame. It is consumed exactly once, by the terminal commit of the turn it
//! names, which clears it or writes the next link of the chain.
//!
//! This module holds the backend-neutral decisions every store and the
//! runtime apply: which admissions the fact blocks, which head writes it refuses,
//! and whether a recovering shift may still run it.

use crate::{FrameNodeId, TurnId};

use super::{PhysicalTurn, StoreError};

/// How many times a shift may recover a pending follow-on before the
/// follow-on is committed as failed instead (ADR 0101 §3, recovery bound).
pub const DEFAULT_MAX_FOLLOW_ON_RECOVERIES: u32 = 3;

/// The follow-on turn a committed agent-frame switch owes the session.
///
/// Written by the switch commit, atomically with the frame change; cleared or
/// replaced only by the terminal commit of `follow_on_turn_id`.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct PendingFollowOn {
    /// The follow-on's turn id: the logical run's root turn plus the
    /// follow-on's physical-turn index, so the run is recoverable from it.
    pub follow_on_turn_id: TurnId,
    /// The frame the follow-on runs in. Every head write keeps it current.
    pub frame_id: FrameNodeId,
    /// The task the switching turn handed to the frame; the follow-on's
    /// input. Empty for a [`continuation`](Self::continuation), which has no
    /// input of its own.
    pub task: String,
    /// Set when the follow-on continues a run that crossed a segment
    /// boundary (FIG-4739) rather than running a switched frame's task: the
    /// owing turn ended at a quiet point of the run, and the follow-on goes
    /// on from the history that turn committed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<RunContinuation>,
    /// The shape the logical run's run resolved under, recorded at the
    /// switch so a recovered follow-on runs under it — its protocol turn
    /// options included — rather than resolving the session's current
    /// defaults fresh (FIG-3877). It also carries the logical run's
    /// follow-on recovery bound, which every recovery decides on.
    #[schemars(with = "serde_json::Value")]
    pub resolved_run: Box<crate::run_spec::ResolvedRun>,
    /// Frame switches in this chain so far, carried across a crash so the
    /// chain bound does not restart at zero.
    pub chain_depth: u32,
    /// Recoveries so far. Raised once per recovering shift, never reset.
    pub attempts: u32,
}

/// The continuation a segment boundary owes its run (FIG-4739).
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct RunContinuation {
    /// Why the run crossed the boundary.
    pub reason: lash_sansio::BoundaryReason,
    /// The protocol iterations the run has spent so far, over every physical
    /// turn up to the boundary: the continuation's turn budget counts on
    /// from them, so a run's budget is one budget however many segments it
    /// takes.
    pub protocol_iterations: u64,
    /// The code cell the boundary stopped inside, when it stopped inside one:
    /// the continuation issues the cell again and the cell resumes from the
    /// state the boundary's commit captured. `None` for a boundary between
    /// protocol steps, whose continuation asks the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<SuspendedCell>,
    /// The settled tool round the root still awaits, including its expansion plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<serde_json::Value>,
    /// The logical opener's groups and incorporation ledger. Every physical
    /// boundary carries them, including a boundary between code cells.
    pub opener: RunOpenerState,
}

/// The logical Run's opener state transferred by a physical boundary.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct RunOpenerState {
    /// The settlements already incorporated by any earlier cell or segment.
    pub incorporation: crate::effect_opener::IncorporationLedger,
    /// Groups whose consumers stopped before exhaustion, in formation order.
    pub groups: Vec<RunOpenerGroup>,
}

/// A retained group's consumer cursor and tool-call reservation.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct RunOpenerGroup {
    /// The durable group's identity, reused by the successor.
    pub group_key: String,
    /// The group's fixed number of children.
    pub children: usize,
    /// The settled prefix the consumer has already consumed.
    pub consumed: usize,
    /// The tool-call reservation this group continues to hold.
    pub held_tool_calls: usize,
}

/// A code cell a segment boundary stopped inside (FIG-4739): what the turn's
/// machine was waiting on, so the continuation's machine waits on it again.
///
/// The cell's own state — the VM continuation and its cell-local ledgers —
/// is the protocol plugin's, committed with the session's execution state by
/// the boundary's commit. The Run's opener state lives beside this record
/// in its continuation. This record carries only what the turn
/// machine held: the cell and the protocol driver's state for it.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SuspendedCell {
    pub language: String,
    pub code: String,
    /// The protocol plugin whose driver issued the cell.
    pub driver_plugin_id: String,
    /// That driver's state for the cell, opaque to the store.
    pub driver_state: serde_json::Value,
}

impl PendingFollowOn {
    /// The continuation the turn of `physical_ordinal` of `run` owes its execution
    /// after ending at a segment boundary in `frame_id`, the frame it ran in
    /// (FIG-4739). A boundary is no frame switch: `chain_depth` is the depth
    /// the owing turn itself ran at. `resolved` is the shape the execution's run
    /// resolved under, as for a switch.
    pub fn after_boundary(
        run: &TurnId,
        physical_ordinal: u64,
        frame_id: FrameNodeId,
        continuation: RunContinuation,
        chain_depth: u32,
        resolved: crate::run_spec::ResolvedRun,
    ) -> Result<Self, StoreError> {
        let next =
            StoreError::checked_monotonic_increment("follow_on_physical_index", physical_ordinal)?;
        Ok(Self {
            follow_on_turn_id: PhysicalTurn::derive_turn_id(run, next),
            frame_id,
            task: String::new(),
            continuation: Some(continuation),
            resolved_run: Box::new(resolved),
            chain_depth,
            attempts: 0,
        })
    }

    /// The follow-on of physical turn `physical_ordinal` of `run` switching to
    /// `frame_id` with `task`. `chain_depth` counts this switch; `resolved`
    /// is the shape the logical run's run resolved under, recorded so a
    /// recovered follow-on inherits it (FIG-3877) and is recovered under
    /// its bound.
    pub fn after_switch(
        run: &TurnId,
        physical_ordinal: u64,
        frame_id: FrameNodeId,
        task: impl Into<String>,
        chain_depth: u32,
        resolved: crate::run_spec::ResolvedRun,
    ) -> Result<Self, StoreError> {
        let next =
            StoreError::checked_monotonic_increment("follow_on_physical_index", physical_ordinal)?;
        Ok(Self {
            follow_on_turn_id: PhysicalTurn::derive_turn_id(run, next),
            frame_id,
            task: task.into(),
            continuation: None,
            resolved_run: Box::new(resolved),
            chain_depth,
            attempts: 0,
        })
    }

    /// The root turn of the logical run this follow-on continues.
    pub fn run_turn_id(&self) -> TurnId {
        PhysicalTurn::split_turn_id(&self.follow_on_turn_id).0
    }

    /// The run a shift admits this follow-on's recovery under: named by
    /// its recovery count, so every recovery runs as an execution of its
    /// own. Its evidence and its park name [`Self::run_turn_id`], the
    /// logical run the follow-on continues.
    pub fn recovery_run(&self) -> TurnId {
        TurnId::prefixed(
            "follow-on:",
            format_args!("{}#{}", self.follow_on_turn_id, self.attempts),
        )
    }

    /// Whether `run` is the admitted run of one of this follow-on's
    /// recoveries ([`Self::recovery_run`] at any recovery count).
    pub fn names_recovery(&self, run: &TurnId) -> bool {
        run.as_str()
            .strip_prefix("follow-on:")
            .and_then(|rest| rest.strip_prefix(self.follow_on_turn_id.as_str()))
            .and_then(|rest| rest.strip_prefix('#'))
            .is_some_and(|count| count.parse::<u32>().is_ok_and(|n| n.to_string() == count))
    }

    /// The follow-on's physical-turn index within its logical run.
    pub fn physical_index(&self) -> u64 {
        PhysicalTurn::split_turn_id(&self.follow_on_turn_id).1
    }

    /// Whether `turn_id` is this follow-on.
    pub fn is_turn(&self, turn_id: &TurnId) -> bool {
        self.follow_on_turn_id == *turn_id
    }

    /// The typed refusal an admission or commit meets while this fact is set.
    pub fn pending_error(&self, session_id: &crate::SessionId) -> StoreError {
        StoreError::FollowOnPending {
            session_id: session_id.clone(),
            follow_on_turn_id: self.follow_on_turn_id.clone(),
            attempts: self.attempts,
        }
    }

    /// What a shift that recovers this fact may do: raise `attempts` and run
    /// the follow-on, or, once the raised count would pass the bound its
    /// run recorded, commit it failed with [`FollowOnRecovery::Exhausted`].
    pub fn recovery(&self) -> Result<FollowOnRecovery, StoreError> {
        let raised = self.raised()?;
        Ok(
            if raised.attempts > self.resolved_run.follow_on_recoveries {
                FollowOnRecovery::Exhausted(self.clone())
            } else {
                FollowOnRecovery::Run(raised)
            },
        )
    }

    /// This fact with `attempts` raised by one: what a recovering shift
    /// writes before the follow-on's first effect.
    pub fn raised(&self) -> Result<Self, StoreError> {
        let attempts = u32::try_from(StoreError::checked_monotonic_increment(
            "follow_on_attempts",
            u64::from(self.attempts),
        )?)
        .map_err(|_| StoreError::MonotonicCounterOverflow {
            counter: "follow_on_attempts",
            current: u64::from(self.attempts),
        })?;
        Ok(Self {
            attempts,
            ..self.clone()
        })
    }
}

/// A recovering shift's decision over a pending follow-on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FollowOnRecovery {
    /// Run the follow-on; the value carries the raised attempt count.
    Run(PendingFollowOn),
    /// The recovery bound is spent: commit the follow-on as a failed turn
    /// carrying `FollowOnRecoveryExhausted`, which clears the fact.
    Exhausted(PendingFollowOn),
}

/// What a follow-on recovery run records before its turn, as its
/// `shift-follow-on` step (FIG-4361): the recovery its decision took on the
/// fact the head owed, or that the head owed the follow-on no longer. The
/// run executes the recorded answer, so a replay never decides from the head
/// it finds.
///
/// A run or an exhaustion also records the head its follow-on's turn runs
/// on, `base`, and that turn's index, `turn_index` (FIG-4380). The step's
/// body retains the base as the session's latest admission's, as a run's
/// admission retains its own (FIG-3682), and the run adopts it and pins the
/// index before its turn, so a replay after the follow-on's own commit moved
/// the head runs the turn it recorded.
///
/// The decision is also the admission of the follow-on's turn by the build
/// that recovers it, so it records `plugins` as a run's admission does
/// (FIG-4747, FIG-4739): that build's plugin composition and the writer
/// format chosen for each plugin. A run that crossed a segment boundary
/// adopts the plugins of the build its continuation is admitted on, and
/// every replay of that continuation writes in the recorded formats.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum FollowOnRecoveryAnswer {
    /// Run the follow-on: `follow_on` carries the raised attempt count.
    Run {
        follow_on: PendingFollowOn,
        base: super::SessionHeadRef,
        turn_index: u64,
        plugins: super::plugin_writers::PluginAdmission,
    },
    /// The recovery bound is spent: the follow-on commits as its failed
    /// terminal, carrying `FollowOnRecoveryExhausted`.
    Exhausted {
        follow_on: PendingFollowOn,
        base: super::SessionHeadRef,
        turn_index: u64,
        plugins: super::plugin_writers::PluginAdmission,
    },
    /// The head owed the follow-on no longer: another shift answered it,
    /// and the run executes nothing.
    Ceded,
}

/// The admission a store is asked to make while it reads the head's fact.
#[derive(Clone, Copy, Debug)]
pub enum FollowOnAdmission<'a> {
    /// An admission outside any running turn: a run's own admission.
    Idle,
    /// A checkpoint admission of the running physical turn `turn_id`.
    Checkpoint { turn_id: &'a TurnId },
}

/// The typed non-error answer to an admission the pending follow-on blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FollowOnBlocked {
    pub follow_on_turn_id: TurnId,
    pub attempts: u32,
}

/// Whether the head's fact blocks `admission`.
///
/// While a follow-on is pending, every admission is refused except the
/// follow-on's own checkpoint admissions.
pub fn follow_on_blocks_admission(
    pending: Option<&PendingFollowOn>,
    admission: FollowOnAdmission<'_>,
) -> Option<FollowOnBlocked> {
    let pending = pending?;
    let own = match admission {
        FollowOnAdmission::Idle => false,
        FollowOnAdmission::Checkpoint { turn_id } => pending.is_turn(turn_id),
    };
    (!own).then(|| FollowOnBlocked {
        follow_on_turn_id: pending.follow_on_turn_id.clone(),
        attempts: pending.attempts,
    })
}

/// Decide a head write against the fact the head holds.
///
/// * While a fact is set, only the terminal commit of its follow-on turn may
///   clear or replace it; any other turn's terminal commit is refused, and any
///   other head write must carry the fact unchanged.
/// * A fact is created only by a turn's terminal commit (the frame switch),
///   and never names the committing turn.
/// * Whatever is written, its frame is the head's current frame.
pub fn validate_follow_on_head_write(
    session_id: &crate::SessionId,
    existing: Option<&PendingFollowOn>,
    operation: &super::OperationId,
    written: Option<&PendingFollowOn>,
    current_frame_node_id: Option<&FrameNodeId>,
) -> Result<(), StoreError> {
    let terminal_turn = (operation.key == TURN_TERMINAL_OPERATION_KEY)
        .then(|| operation.turn_id())
        .flatten();
    match existing {
        Some(existing) => {
            let own_terminal = terminal_turn.is_some_and(|turn_id| existing.is_turn(turn_id));
            if !own_terminal && (terminal_turn.is_some() || written != Some(existing)) {
                return Err(existing.pending_error(session_id));
            }
            if own_terminal
                && let Some(written) = written
                && written.follow_on_turn_id == existing.follow_on_turn_id
            {
                return Err(StoreError::FollowOnHeadInvariant {
                    session_id: session_id.clone(),
                    reason: format!(
                        "the terminal commit of follow-on `{}` must clear or replace its fact",
                        existing.follow_on_turn_id
                    ),
                });
            }
        }
        None => {
            if let Some(written) = written {
                let Some(turn_id) = terminal_turn else {
                    return Err(StoreError::FollowOnHeadInvariant {
                        session_id: session_id.clone(),
                        reason: "a pending follow-on is written only by a turn's terminal commit"
                            .to_string(),
                    });
                };
                if written.is_turn(turn_id) {
                    return Err(StoreError::FollowOnHeadInvariant {
                        session_id: session_id.clone(),
                        reason: format!("turn `{turn_id}` cannot owe itself a follow-on"),
                    });
                }
            }
        }
    }
    if let Some(written) = written
        && current_frame_node_id != Some(&written.frame_id)
    {
        return Err(StoreError::FollowOnFrameNotCurrent {
            session_id: session_id.clone(),
            follow_on_frame_id: written.frame_id.to_string(),
            current_frame_node_id: current_frame_node_id.map(ToString::to_string),
        });
    }
    Ok(())
}

/// The operation key of a turn's terminal commit.
pub const TURN_TERMINAL_OPERATION_KEY: &str = "final";

/// Encode the head column. `None` is SQL `NULL`.
pub fn encode_pending_follow_on(
    pending: Option<&PendingFollowOn>,
) -> Result<Option<String>, StoreError> {
    pending
        .map(|pending| {
            serde_json::to_string(pending).map_err(|error| {
                StoreError::Backend(format!("failed to encode pending follow-on: {error}"))
            })
        })
        .transpose()
}

/// Decode the head column, failing closed on anything but the current shape.
pub fn decode_pending_follow_on(
    session_id: &crate::SessionId,
    json: Option<&str>,
) -> Result<Option<PendingFollowOn>, StoreError> {
    json.map(|json| {
        serde_json::from_str(json).map_err(|error| StoreError::StoredDataCorrupt {
            record_kind: "PendingFollowOn",
            message: format!(
                "session `{session_id}` pending_follow_on_json does not match the supported \
                 shape: {error}"
            ),
        })
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A default-spec run's record under recovery bound `recoveries`.
    fn resolved(recoveries: u32) -> crate::run_spec::ResolvedRun {
        crate::run_spec::ResolvedRun::snapshot(
            crate::PersistedSessionConfig::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            ),
            crate::run_spec::TerminationPolicy::default(),
            recoveries,
        )
    }

    fn fact(turn: &str, frame: &str) -> PendingFollowOn {
        PendingFollowOn {
            continuation: None,
            follow_on_turn_id: TurnId::fixture(turn),
            frame_id: FrameNodeId::new(frame).expect("frame"),
            task: "task".into(),
            resolved_run: Box::new(resolved(DEFAULT_MAX_FOLLOW_ON_RECOVERIES)),
            chain_depth: 1,
            attempts: 0,
        }
    }

    /// A recovery's admitted run names its follow-on and count, and maps
    /// back to the logical run the follow-on continues.
    #[test]
    fn a_recovery_run_names_its_follow_on_at_any_count() {
        let mut owed = fact("run:agent-frame:1", "f");
        assert_eq!(
            owed.recovery_run(),
            TurnId::from("follow-on:run:agent-frame:1#0")
        );
        owed.attempts = 3;
        assert!(owed.names_recovery(&TurnId::from("follow-on:run:agent-frame:1#0")));
        assert!(owed.names_recovery(&owed.recovery_run()));
        for other in [
            "run",
            "follow-on:run:agent-frame:2#0",
            "follow-on:run:agent-frame:1#",
            "follow-on:run:agent-frame:1#01",
            "follow-on:run:agent-frame:1#x",
        ] {
            assert!(!owed.names_recovery(&TurnId::from(other)), "{other}");
        }
        assert_eq!(owed.run_turn_id(), TurnId::from("run"));
    }

    #[test]
    fn suffix_shaped_host_runs_keep_their_follow_on_identity() {
        for host in [
            "job",
            "job:agent-frame:1",
            "job:agent-frame:01",
            "job:agent-frame:+1",
        ] {
            let run = TurnId::from(host);
            let first = PendingFollowOn::after_switch(
                &run,
                0,
                FrameNodeId::new("f").expect("frame"),
                "t",
                1,
                resolved(DEFAULT_MAX_FOLLOW_ON_RECOVERIES),
            )
            .expect("switch");
            assert_eq!(
                first.follow_on_turn_id,
                PhysicalTurn::derive_turn_id(&run, 1),
                "{host}"
            );
            assert_eq!(first.run_turn_id(), run);
        }
    }

    /// The head column's fact always carries its run's record: a fact
    /// without one is not the supported shape.
    #[test]
    fn a_fact_without_its_runs_record_does_not_decode() {
        let session = crate::SessionId::from("s");
        let pending = fact("run:agent-frame:1", "f");
        let encoded = encode_pending_follow_on(Some(&pending))
            .expect("encode")
            .expect("a fact");
        assert_eq!(
            decode_pending_follow_on(&session, Some(&encoded)).expect("decode"),
            Some(pending)
        );
        let mut value: serde_json::Value = serde_json::from_str(&encoded).expect("json");
        value
            .as_object_mut()
            .expect("an object")
            .remove("resolved_run");
        assert!(matches!(
            decode_pending_follow_on(&session, Some(&value.to_string())),
            Err(StoreError::StoredDataCorrupt { .. })
        ));
    }
}
