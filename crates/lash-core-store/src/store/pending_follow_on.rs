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
//! and whether a recovering drive may still run it.

use crate::{FrameNodeId, TurnId};

use super::{PhysicalTurn, StoreError};

/// How many times a drive may recover a pending follow-on before the
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
    /// follow-on's physical-turn index, so the root is recoverable from it.
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
    /// The shape the logical run's root resolved under, recorded at the
    /// switch so a recovered follow-on runs under it — its protocol turn
    /// options included — rather than resolving the session's current
    /// defaults fresh (FIG-3877). It also carries the logical run's
    /// follow-on recovery bound, which every recovery decides on.
    #[schemars(with = "serde_json::Value")]
    pub resolved_run: Box<crate::run_spec::ResolvedRun>,
    /// Frame switches in this chain so far, carried across a crash so the
    /// chain bound does not restart at zero.
    pub chain_depth: u32,
    /// Recoveries so far. Raised once per recovering drive, never reset.
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
}

impl PendingFollowOn {
    /// The continuation the turn of `physical_ordinal` of `root` owes its run
    /// after ending at a segment boundary in `frame_id`, the frame it ran in
    /// (FIG-4739). A boundary is no frame switch: `chain_depth` is the depth
    /// the owing turn itself ran at. `resolved` is the shape the run's root
    /// resolved under, as for a switch.
    pub fn after_boundary(
        root: &TurnId,
        physical_ordinal: u64,
        frame_id: FrameNodeId,
        continuation: RunContinuation,
        chain_depth: u32,
        resolved: crate::run_spec::ResolvedRun,
    ) -> Result<Self, StoreError> {
        let next =
            StoreError::checked_monotonic_increment("follow_on_physical_index", physical_ordinal)?;
        Ok(Self {
            follow_on_turn_id: PhysicalTurn::derive_turn_id(root, next),
            frame_id,
            task: String::new(),
            continuation: Some(continuation),
            resolved_run: Box::new(resolved),
            chain_depth,
            attempts: 0,
        })
    }

    /// The follow-on of physical turn `physical_ordinal` of `root` switching to
    /// `frame_id` with `task`. `chain_depth` counts this switch; `resolved`
    /// is the shape the logical run's root resolved under, recorded so a
    /// recovered follow-on inherits it (FIG-3877) and is recovered under
    /// its bound.
    pub fn after_switch(
        root: &TurnId,
        physical_ordinal: u64,
        frame_id: FrameNodeId,
        task: impl Into<String>,
        chain_depth: u32,
        resolved: crate::run_spec::ResolvedRun,
    ) -> Result<Self, StoreError> {
        let next =
            StoreError::checked_monotonic_increment("follow_on_physical_index", physical_ordinal)?;
        Ok(Self {
            follow_on_turn_id: PhysicalTurn::derive_turn_id(root, next),
            frame_id,
            task: task.into(),
            continuation: None,
            resolved_run: Box::new(resolved),
            chain_depth,
            attempts: 0,
        })
    }

    /// The root turn of the logical run this follow-on continues.
    pub fn root_turn_id(&self) -> TurnId {
        PhysicalTurn::split_turn_id(&self.follow_on_turn_id).0
    }

    /// The root a drive admits this follow-on's recovery under: named by
    /// its recovery count, so every recovery runs as an execution of its
    /// own. Its evidence and its park name [`Self::root_turn_id`], the
    /// logical root the follow-on continues.
    pub fn recovery_root(&self) -> TurnId {
        TurnId::prefixed(
            "follow-on:",
            format_args!("{}#{}", self.follow_on_turn_id, self.attempts),
        )
    }

    /// Whether `root` is the admitted root of one of this follow-on's
    /// recoveries ([`Self::recovery_root`] at any recovery count).
    pub fn names_recovery(&self, root: &TurnId) -> bool {
        root.as_str()
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

    /// What a drive that recovers this fact may do: raise `attempts` and run
    /// the follow-on, or, once the raised count would pass the bound its
    /// root recorded, commit it failed with [`FollowOnRecovery::Exhausted`].
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

    /// This fact with `attempts` raised by one: what a recovering drive
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

/// A recovering drive's decision over a pending follow-on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FollowOnRecovery {
    /// Run the follow-on; the value carries the raised attempt count.
    Run(PendingFollowOn),
    /// The recovery bound is spent: commit the follow-on as a failed turn
    /// carrying `FollowOnRecoveryExhausted`, which clears the fact.
    Exhausted(PendingFollowOn),
}

/// What a follow-on recovery root records before its turn, as its
/// `drive-follow-on` step (FIG-4361): the recovery its decision took on the
/// fact the head owed, or that the head owed the follow-on no longer. The
/// root drives the recorded answer, so a replay never decides from the head
/// it finds.
///
/// A run or an exhaustion also records the head its follow-on's turn runs
/// on, `base`, and that turn's index, `turn_index` (FIG-4380). The step's
/// body retains the base as the session's latest admission's, as a root's
/// admission retains its own (FIG-3682), and the root adopts it and pins the
/// index before its turn, so a replay after the follow-on's own commit moved
/// the head runs the turn it recorded.
///
/// The decision is also the admission of the follow-on's turn by the build
/// that recovers it, so it records `plugins` as a root's admission does
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
    /// The head owed the follow-on no longer: another driver answered it,
    /// and the root runs nothing.
    Ceded,
}

/// The admission a store is asked to make while it reads the head's fact.
#[derive(Clone, Copy, Debug)]
pub enum FollowOnAdmission<'a> {
    /// An admission outside any running turn: a root's own admission.
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

    /// A default-spec root's record under recovery bound `recoveries`.
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

    fn terminal(turn: &'static str) -> super::super::OperationId {
        super::super::OperationId::turn("s", turn, TURN_TERMINAL_OPERATION_KEY)
    }

    /// A recovery's admitted root names its follow-on and count, and maps
    /// back to the logical root the follow-on continues.
    #[test]
    fn a_recovery_root_names_its_follow_on_at_any_count() {
        let mut owed = fact("root:agent-frame:1", "f");
        assert_eq!(
            owed.recovery_root(),
            TurnId::from("follow-on:root:agent-frame:1#0")
        );
        owed.attempts = 3;
        assert!(owed.names_recovery(&TurnId::from("follow-on:root:agent-frame:1#0")));
        assert!(owed.names_recovery(&owed.recovery_root()));
        for other in [
            "root",
            "follow-on:root:agent-frame:2#0",
            "follow-on:root:agent-frame:1#",
            "follow-on:root:agent-frame:1#01",
            "follow-on:root:agent-frame:1#x",
        ] {
            assert!(!owed.names_recovery(&TurnId::from(other)), "{other}");
        }
        assert_eq!(owed.root_turn_id(), TurnId::from("root"));
    }

    #[test]
    fn follow_on_ids_count_physical_turns_from_the_root() {
        let first = PendingFollowOn::after_switch(
            &TurnId::from("root"),
            0,
            FrameNodeId::new("f").expect("frame"),
            "t",
            1,
            resolved(DEFAULT_MAX_FOLLOW_ON_RECOVERIES),
        )
        .expect("first");
        assert_eq!(first.follow_on_turn_id, TurnId::from("root:agent-frame:1"));
        let second = PendingFollowOn::after_switch(
            &TurnId::from("root"),
            1,
            FrameNodeId::new("g").expect("frame"),
            "t",
            2,
            resolved(DEFAULT_MAX_FOLLOW_ON_RECOVERIES),
        )
        .expect("second");
        assert_eq!(second.follow_on_turn_id, TurnId::from("root:agent-frame:2"));
        assert_eq!(second.root_turn_id(), TurnId::from("root"));
        assert_eq!(second.physical_index(), 2);
    }

    #[test]
    fn suffix_shaped_host_roots_keep_their_follow_on_identity() {
        for host in [
            "job",
            "job:agent-frame:1",
            "job:agent-frame:01",
            "job:agent-frame:+1",
        ] {
            let root = TurnId::from(host);
            let first = PendingFollowOn::after_switch(
                &root,
                0,
                FrameNodeId::new("f").expect("frame"),
                "t",
                1,
                resolved(DEFAULT_MAX_FOLLOW_ON_RECOVERIES),
            )
            .expect("switch");
            assert_eq!(
                first.follow_on_turn_id,
                PhysicalTurn::derive_turn_id(&root, 1),
                "{host}"
            );
            assert_eq!(first.root_turn_id(), root);
        }
    }

    #[test]
    fn only_the_follow_on_admits_while_it_is_pending() {
        let pending = fact("root:agent-frame:1", "f");
        assert!(follow_on_blocks_admission(Some(&pending), FollowOnAdmission::Idle).is_some());
        assert!(
            follow_on_blocks_admission(
                Some(&pending),
                FollowOnAdmission::Checkpoint {
                    turn_id: &TurnId::from("other")
                }
            )
            .is_some()
        );
        assert!(
            follow_on_blocks_admission(
                Some(&pending),
                FollowOnAdmission::Checkpoint {
                    turn_id: &pending.follow_on_turn_id
                }
            )
            .is_none()
        );
        assert!(follow_on_blocks_admission(None, FollowOnAdmission::Idle).is_none());
    }

    #[test]
    fn head_writes_keep_the_fact_until_its_own_terminal_commit() {
        let session = crate::SessionId::from("s");
        let pending = fact("root:agent-frame:1", "f");
        let frame = pending.frame_id.clone();
        // Another turn's terminal commit is refused, whatever it writes.
        assert!(matches!(
            validate_follow_on_head_write(
                &session,
                Some(&pending),
                &terminal("other"),
                Some(&pending),
                Some(&frame)
            ),
            Err(StoreError::FollowOnPending { .. })
        ));
        // A non-turn head write keeps the fact unchanged, or is refused.
        let config = super::super::OperationId::turn("s", "other", "record-config");
        assert!(
            validate_follow_on_head_write(
                &session,
                Some(&pending),
                &config,
                Some(&pending),
                Some(&frame)
            )
            .is_ok()
        );
        assert!(
            validate_follow_on_head_write(&session, Some(&pending), &config, None, Some(&frame))
                .is_err()
        );
        // The follow-on's own terminal commit clears it.
        assert!(
            validate_follow_on_head_write(
                &session,
                Some(&pending),
                &terminal("root:agent-frame:1"),
                None,
                Some(&frame)
            )
            .is_ok()
        );
        // The frame invariant holds on every write.
        let elsewhere = FrameNodeId::new("g").expect("frame");
        assert!(matches!(
            validate_follow_on_head_write(
                &session,
                Some(&pending),
                &config,
                Some(&pending),
                Some(&elsewhere)
            ),
            Err(StoreError::FollowOnFrameNotCurrent { .. })
        ));
        // Only a turn's terminal commit creates a fact.
        assert!(
            validate_follow_on_head_write(&session, None, &config, Some(&pending), Some(&frame))
                .is_err()
        );
        assert!(
            validate_follow_on_head_write(
                &session,
                None,
                &terminal("root"),
                Some(&pending),
                Some(&frame)
            )
            .is_ok()
        );
    }

    #[test]
    fn recovery_is_bounded_and_never_resets() {
        let mut pending = fact("root:agent-frame:1", "f");
        for expected in 1..=DEFAULT_MAX_FOLLOW_ON_RECOVERIES {
            match pending.recovery().expect("recovery") {
                FollowOnRecovery::Run(raised) => {
                    assert_eq!(raised.attempts, expected);
                    pending = raised;
                }
                FollowOnRecovery::Exhausted(_) => panic!("exhausted early"),
            }
        }
        assert!(matches!(
            pending.recovery(),
            Ok(FollowOnRecovery::Exhausted(_))
        ));
    }

    /// The bound a recovery decides on is the one the fact's root recorded,
    /// whatever bound the recovering host is configured with.
    #[test]
    fn recovery_decides_on_the_bound_its_root_recorded() {
        let mut pending = fact("root:agent-frame:1", "f");
        *pending.resolved_run = resolved(0);
        assert!(matches!(
            pending.recovery(),
            Ok(FollowOnRecovery::Exhausted(_))
        ));
        *pending.resolved_run = resolved(1);
        assert!(matches!(
            pending.recovery(),
            Ok(FollowOnRecovery::Run(raised))
                if raised.attempts == 1 && raised.resolved_run.follow_on_recoveries == 1
        ));
    }

    /// The head column's fact always carries its root's record: a fact
    /// without one is not the supported shape.
    #[test]
    fn a_fact_without_its_roots_record_does_not_decode() {
        let session = crate::SessionId::from("s");
        let pending = fact("root:agent-frame:1", "f");
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
