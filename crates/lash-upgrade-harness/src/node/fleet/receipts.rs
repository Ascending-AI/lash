//! Project the operation identities stored in SQL's historical `turn_id`
//! column into the terminal physical turn's final settlement witness.
use super::FleetCommitReceipt;
use anyhow::{Result, ensure};
use lash_core::{SessionId, TurnId};

pub(super) fn terminal_commit_count(
    receipts: &[FleetCommitReceipt],
    session: &SessionId,
    turn: &TurnId,
) -> Result<usize> {
    let terminal = lash_core::OperationId::turn(session.clone(), turn.clone(), "final");
    receipts.iter().try_fold(0, |count, receipt| {
        let operation: lash_core::OperationId = serde_json::from_str(&receipt.turn)?;
        let result: lash_core::store::RuntimeCommitReceipt =
            serde_json::from_value(receipt.result.clone())?;
        lash_core::store::validate_turn_commit_outcome_code(&result, receipt.outcome.as_deref())?;
        if operation == terminal {
            ensure!(
                result.outcome.is_some(),
                "terminal final operation has no typed outcome"
            );
            Ok(count + 1)
        } else {
            Ok(count)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::store::{
        FleetFormat, RuntimeCommitReceipt, SessionCheckpoint, TurnCommitOutcome,
    };

    fn receipt(
        session: &SessionId,
        turn: &TurnId,
        key: &str,
        outcome: Option<TurnCommitOutcome>,
    ) -> FleetCommitReceipt {
        let result = RuntimeCommitReceipt {
            committed_at_ms: 0,
            schema_version: lash_core::store::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION,
            head_revision: 2,
            checkpoint_ref: "checkpoint".to_string().into(),
            manifest: SessionCheckpoint::for_fleet(FleetFormat::current()),
            committed_leaf_node_id: None,
            realized_node_timestamps: Vec::new(),
            failure_evidence: Vec::new(),
            outcome: outcome.clone(),
            trace: None,
            pending_follow_on: None,
            command_outcomes: Default::default(),
            turn_input_applications: Vec::new(),
            turn_cancel_input_outcome: Default::default(),
            work_remaining: false,
            receipt_replayed: false,
        };
        FleetCommitReceipt {
            turn: lash_core::OperationId::turn(session.clone(), turn.clone(), key)
                .storage_key()
                .expect("canonical SQL key"),
            hash: "commit-hash".into(),
            result: serde_json::to_value(result).expect("typed SQL receipt"),
            outcome: outcome.map(|outcome| outcome.as_str().to_owned()),
        }
    }

    /// FIG-4965: SQL `turn_id` is a canonical OperationId, and a checkpoint
    /// of the same physical turn is not its one terminal final settlement.
    #[test]
    fn sql_operation_receipts_witness_exactly_one_typed_terminal_settlement() {
        let session = SessionId::from("fleet");
        let turn = TurnId::from("terminal-turn");
        let checkpoint = receipt(&session, &turn, "checkpoint:1", None);
        let terminal = receipt(&session, &turn, "final", Some(TurnCommitOutcome::Cancelled));
        assert_eq!(
            terminal_commit_count(&[checkpoint, terminal.clone()], &session, &turn)
                .expect("valid receipts"),
            1
        );
        let mut corrupt = terminal;
        corrupt.outcome = Some("completed".into());
        assert!(
            terminal_commit_count(&[corrupt], &session, &turn).is_err(),
            "a corrupt SQL outcome never supplies a terminal witness"
        );
    }
}
