//! The commit label catalog (I0, FIG-5194): every label the runtime lanes
//! commit under, so a harness can cut at each and a catalog audit can find
//! an unnamed one. A lane that needs a new label adds it here, in its own
//! lane, and lists it in [`CommitLabel::ALL`].

use crate::ids::CommitLabel;

impl CommitLabel {
    // Turns (V0, then L3).
    /// `turn.accept`: A session input accepted into the mailbox (C0).
    pub const TURN_ACCEPT: Self = Self::new("turn.accept");
    /// `turn.admit`: A turn admitted: inputs bound, deadline recorded (C2).
    pub const TURN_ADMIT: Self = Self::new("turn.admit");
    /// `turn.prepare`: The prepared context, checkpoint and model pin, before the first byte (C3).
    pub const TURN_PREPARE: Self = Self::new("turn.prepare");
    /// `model.start`: A model call started: its request pinned and its deadline recorded.
    pub const MODEL_START: Self = Self::new("model.start");
    /// `model.done`: A model response, the next checkpoint and the round's admission, fused (C4).
    pub const MODEL_DONE: Self = Self::new("model.done");
    /// `round.present+model.start`: A round's presentation and the next model start, fused (C6).
    pub const ROUND_PRESENT_MODEL_START: Self = Self::new("round.present+model.start");
    /// `turn.commit`: The turn's commit: head compare-and-set, terminal, phase-row pruning (C7).
    pub const TURN_COMMIT: Self = Self::new("turn.commit");
    /// `turn.cancel`: A turn's cancel terminal.
    pub const TURN_CANCEL: Self = Self::new("turn.cancel");
    /// `session.release`: The session activation gives its actor up with
    /// nothing left to run.
    pub const SESSION_RELEASE: Self = Self::new("session.release");

    // Tool rounds (V0, then L4).
    /// `round.outcome`: A batch of finished members' outcomes and their store-local effects (C5).
    pub const ROUND_OUTCOME: Self = Self::new("round.outcome");
    /// `round.retry`: A retry record with its due time.
    pub const ROUND_RETRY: Self = Self::new("round.retry");
    /// `round.start`: A retried member's next attempt started, before its body runs.
    pub const ROUND_START: Self = Self::new("round.start");

    // Waits (L5).
    /// `wait.mint`: A wait pinned.
    pub const WAIT_MINT: Self = Self::new("wait.mint");
    /// `wait.resolve`: A wait resolved from outside its owner.
    pub const WAIT_RESOLVE: Self = Self::new("wait.resolve");
    /// `wait.timeout`: A due wait timed out or a timer resolved.
    pub const WAIT_TIMEOUT: Self = Self::new("wait.timeout");
    /// `wait.revoke`: A scope's waits revoked.
    pub const WAIT_REVOKE: Self = Self::new("wait.revoke");

    // Processes (L6).
    /// `process.register`: A process registered and its actor created ready.
    pub const PROCESS_REGISTER: Self = Self::new("process.register");
    /// `process.advance`: An engine transition and its action's admission.
    pub const PROCESS_ADVANCE: Self = Self::new("process.advance");
    /// `step.start`: A process step's admission and started row.
    pub const STEP_START: Self = Self::new("step.start");
    /// `step.outcome`: A process step's outcome.
    pub const STEP_OUTCOME: Self = Self::new("step.outcome");
    /// `process.cancel`: A process cancel requested.
    pub const PROCESS_CANCEL: Self = Self::new("process.cancel");
    /// `process.terminal`: A process's terminal transaction.
    pub const PROCESS_TERMINAL: Self = Self::new("process.terminal");
    /// `cascade.batch`: One batch of a scope's `Until` children marked for cancel.
    pub const CASCADE_BATCH: Self = Self::new("cascade.batch");

    // VM (V0, then L7).
    /// `cell.snapshot+admit`: A snapshot, its broker ledger and the admission of the operations it issued.
    pub const CELL_SNAPSHOT_ADMIT: Self = Self::new("cell.snapshot+admit");
    /// `cell.inject`: An outcome injected into a cell.
    pub const CELL_INJECT: Self = Self::new("cell.inject");
    /// `cell.snapshot`: A quiet-point snapshot that admits nothing.
    pub const CELL_SNAPSHOT: Self = Self::new("cell.snapshot");

    // Session close (L6b).
    /// `session.close.cancel`: Close step: the open turn cancelled.
    pub const SESSION_CLOSE_CANCEL: Self = Self::new("session.close.cancel");
    /// `session.close.revoke`: Close step: the session's waits revoked.
    pub const SESSION_CLOSE_REVOKE: Self = Self::new("session.close.revoke");
    /// `session.close.end_scope`: Close step: one batch of the session's `Until` processes ended.
    pub const SESSION_CLOSE_END_SCOPE: Self = Self::new("session.close.end_scope");
    /// `session.close.triggers`: Close step: the session's triggers deleted.
    pub const SESSION_CLOSE_TRIGGERS: Self = Self::new("session.close.triggers");
    /// `session.close.artifacts`: Close step: the session's artifact cleanup armed.
    pub const SESSION_CLOSE_ARTIFACTS: Self = Self::new("session.close.artifacts");
    /// `session.close.tombstone`: Close step: the tombstone written and state deleted.
    pub const SESSION_CLOSE_TOMBSTONE: Self = Self::new("session.close.tombstone");

    // Mail (L3s, L6).
    /// `mail.session`: A producer's mail to a session actor.
    pub const MAIL_SESSION: Self = Self::new("mail.session");
    /// `mail.process`: A producer's mail to a process actor.
    pub const MAIL_PROCESS: Self = Self::new("mail.process");

    // Drain (L11).
    /// `drain.release`: An actor released by a draining node.
    pub const DRAIN_RELEASE: Self = Self::new("drain.release");

    /// The commits a store serves from reserved connection capacity (L8,
    /// FIG-5178): the node lease's and every terminal and cancel write, so
    /// a burst of ordinary commits (model results, round outcomes) can
    /// neither starve a heartbeat into a self-stop nor hold back an ending.
    pub const RESERVED: [Self; 7] = [
        Self::HEARTBEAT,
        Self::REAP,
        Self::NODE_REGISTER,
        Self::NODE_RELEASE,
        Self::TURN_CANCEL,
        Self::PROCESS_CANCEL,
        Self::PROCESS_TERMINAL,
    ];

    /// Whether a store serves this commit from its reserved capacity.
    #[must_use]
    pub fn is_reserved(self) -> bool {
        Self::RESERVED.contains(&self)
    }

    /// Every label in the catalog, L1's lease labels first.
    pub const ALL: [Self; 39] = [
        Self::CLAIM,
        Self::HEARTBEAT,
        Self::REAP,
        Self::NODE_REGISTER,
        Self::NODE_RELEASE,
        Self::TURN_ACCEPT,
        Self::TURN_ADMIT,
        Self::TURN_PREPARE,
        Self::MODEL_START,
        Self::MODEL_DONE,
        Self::ROUND_PRESENT_MODEL_START,
        Self::TURN_COMMIT,
        Self::TURN_CANCEL,
        Self::SESSION_RELEASE,
        Self::ROUND_OUTCOME,
        Self::ROUND_RETRY,
        Self::ROUND_START,
        Self::WAIT_MINT,
        Self::WAIT_RESOLVE,
        Self::WAIT_TIMEOUT,
        Self::WAIT_REVOKE,
        Self::PROCESS_REGISTER,
        Self::PROCESS_ADVANCE,
        Self::STEP_START,
        Self::STEP_OUTCOME,
        Self::PROCESS_CANCEL,
        Self::PROCESS_TERMINAL,
        Self::CASCADE_BATCH,
        Self::CELL_SNAPSHOT_ADMIT,
        Self::CELL_INJECT,
        Self::CELL_SNAPSHOT,
        Self::SESSION_CLOSE_CANCEL,
        Self::SESSION_CLOSE_REVOKE,
        Self::SESSION_CLOSE_END_SCOPE,
        Self::SESSION_CLOSE_TRIGGERS,
        Self::SESSION_CLOSE_ARTIFACTS,
        Self::SESSION_CLOSE_TOMBSTONE,
        Self::MAIL_SESSION,
        Self::MAIL_PROCESS,
        Self::DRAIN_RELEASE,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_label_in_the_catalog_is_distinct() {
        let mut seen = std::collections::BTreeSet::new();
        for label in CommitLabel::ALL {
            assert!(seen.insert(label.as_str()), "{label} is listed twice");
        }
    }
}
