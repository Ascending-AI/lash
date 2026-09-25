//! The pending follow-on: a frame handoff recorded on the session head
//! (ADR 0101 §3, FIG-3542).
//!
//! A turn that switches agent frame commits the switch and the obligation to
//! run the switched frame's task in one head write. The obligation is this
//! fact, in its own head column (`pending_follow_on_json`), never an ingress
//! row: nothing can claim it, reorder it, cancel it or render it into another
//! frame. It is consumed exactly once, by the terminal commit of the turn it
//! names, which clears it or writes the next link of the chain.
//!
//! This module holds the backend-neutral decisions every store and the
//! runtime apply: which claims the fact blocks, which head writes it refuses,
//! and whether a recovering drive may still run it.

use crate::{FrameNodeId, TurnId};

use super::{QueuedRunPosition, StoreError};

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
    /// The task the switching turn handed to the frame; the follow-on's input.
    pub task: String,
    /// The protocol turn options the follow-on runs under. Boxed: the fact
    /// rides in every resident session state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Box<crate::ProtocolTurnOptions>>,
    /// Frame switches in this chain so far, carried across a crash so the
    /// chain bound does not restart at zero.
    pub chain_depth: u32,
    /// Recoveries so far. Raised once per recovering drive, never reset.
    pub attempts: u32,
}

impl PendingFollowOn {
    /// The follow-on of the physical turn `current_turn_id` switching to
    /// `frame_id` with `task`. `chain_depth` counts this switch.
    pub fn after_switch(
        current_turn_id: &TurnId,
        frame_id: FrameNodeId,
        task: impl Into<String>,
        options: Option<crate::ProtocolTurnOptions>,
        chain_depth: u32,
    ) -> Result<Self, StoreError> {
        let (root, index) = QueuedRunPosition::split_turn_id(current_turn_id);
        let next = StoreError::checked_monotonic_increment("follow_on_physical_index", index)?;
        Ok(Self {
            follow_on_turn_id: QueuedRunPosition::derive_turn_id(&root, next),
            frame_id,
            task: task.into(),
            options: options.map(Box::new),
            chain_depth,
            attempts: 0,
        })
    }

    /// The root turn of the logical run this follow-on continues.
    pub fn root_turn_id(&self) -> TurnId {
        QueuedRunPosition::split_turn_id(&self.follow_on_turn_id).0
    }

    /// The follow-on's physical-turn index within its logical run.
    pub fn physical_index(&self) -> u64 {
        QueuedRunPosition::split_turn_id(&self.follow_on_turn_id).1
    }

    /// Whether `turn_id` is this follow-on.
    pub fn is_turn(&self, turn_id: &TurnId) -> bool {
        self.follow_on_turn_id == *turn_id
    }

    /// The typed refusal a claim or commit meets while this fact is set.
    pub fn pending_error(&self, session_id: &crate::SessionId) -> StoreError {
        StoreError::FollowOnPending {
            session_id: session_id.clone(),
            follow_on_turn_id: self.follow_on_turn_id.clone(),
            attempts: self.attempts,
        }
    }

    /// What a drive that recovers this fact may do: raise `attempts` and run
    /// the follow-on, or, once the raised count would pass `max_recoveries`,
    /// commit it failed with [`FollowOnRecovery::Exhausted`].
    pub fn recovery(&self, max_recoveries: u32) -> Result<FollowOnRecovery, StoreError> {
        let raised = self.raised()?;
        Ok(if raised.attempts > max_recoveries {
            FollowOnRecovery::Exhausted(self.clone())
        } else {
            FollowOnRecovery::Run(raised)
        })
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

/// The claim a store is asked to grant while it reads the head's fact.
#[derive(Clone, Copy, Debug)]
pub enum FollowOnClaim<'a> {
    /// A claim outside any running turn: idle, a session command, a queued
    /// run's first selection, a direct turn's drive.
    Idle,
    /// A checkpoint claim of the running turn `turn_id`.
    Checkpoint { turn_id: &'a TurnId },
    /// A queued run's selection at the physical turn `turn_id`.
    QueuedRun { turn_id: &'a TurnId },
}

/// The typed non-error answer to a claim the pending follow-on blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FollowOnBlocked {
    pub follow_on_turn_id: TurnId,
    pub attempts: u32,
}

/// Whether the head's fact blocks `claim`.
///
/// While a follow-on is pending, every claim is refused except the
/// follow-on's own: its checkpoint claims, and the selection of the queued run
/// whose next physical turn it is.
pub fn follow_on_blocks_claim(
    pending: Option<&PendingFollowOn>,
    claim: FollowOnClaim<'_>,
) -> Option<FollowOnBlocked> {
    let pending = pending?;
    let own = match claim {
        FollowOnClaim::Idle => false,
        FollowOnClaim::Checkpoint { turn_id } | FollowOnClaim::QueuedRun { turn_id } => {
            pending.is_turn(turn_id)
        }
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

    fn fact(turn: &str, frame: &str) -> PendingFollowOn {
        PendingFollowOn {
            follow_on_turn_id: TurnId::from(turn),
            frame_id: FrameNodeId::new(frame).expect("frame"),
            task: "task".into(),
            options: None,
            chain_depth: 1,
            attempts: 0,
        }
    }

    fn terminal(turn: &str) -> super::super::OperationId {
        super::super::OperationId::turn("s", turn, TURN_TERMINAL_OPERATION_KEY)
    }

    #[test]
    fn follow_on_ids_count_physical_turns_from_the_root() {
        let first = PendingFollowOn::after_switch(
            &TurnId::from("root"),
            FrameNodeId::new("f").expect("frame"),
            "t",
            None,
            1,
        )
        .expect("first");
        assert_eq!(first.follow_on_turn_id, TurnId::from("root:agent-frame:1"));
        let second = PendingFollowOn::after_switch(
            &first.follow_on_turn_id,
            FrameNodeId::new("g").expect("frame"),
            "t",
            None,
            2,
        )
        .expect("second");
        assert_eq!(second.follow_on_turn_id, TurnId::from("root:agent-frame:2"));
        assert_eq!(second.root_turn_id(), TurnId::from("root"));
        assert_eq!(second.physical_index(), 2);
    }

    #[test]
    fn only_the_follow_on_claims_while_it_is_pending() {
        let pending = fact("root:agent-frame:1", "f");
        assert!(follow_on_blocks_claim(Some(&pending), FollowOnClaim::Idle).is_some());
        assert!(
            follow_on_blocks_claim(
                Some(&pending),
                FollowOnClaim::Checkpoint {
                    turn_id: &TurnId::from("other")
                }
            )
            .is_some()
        );
        assert!(
            follow_on_blocks_claim(
                Some(&pending),
                FollowOnClaim::Checkpoint {
                    turn_id: &pending.follow_on_turn_id
                }
            )
            .is_none()
        );
        assert!(follow_on_blocks_claim(None, FollowOnClaim::Idle).is_none());
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
            match pending
                .recovery(DEFAULT_MAX_FOLLOW_ON_RECOVERIES)
                .expect("recovery")
            {
                FollowOnRecovery::Run(raised) => {
                    assert_eq!(raised.attempts, expected);
                    pending = raised;
                }
                FollowOnRecovery::Exhausted(_) => panic!("exhausted early"),
            }
        }
        assert!(matches!(
            pending.recovery(DEFAULT_MAX_FOLLOW_ON_RECOVERIES),
            Ok(FollowOnRecovery::Exhausted(_))
        ));
    }
}
