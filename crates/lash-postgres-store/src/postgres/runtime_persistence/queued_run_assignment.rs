use super::*;
use lash_core::store::QueuedRunMember;

pub(super) async fn assign_checkpoint_members_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &lash_core::TurnId,
    input: Option<&lash_core::TurnInputClaim>,
    queued: Option<&QueuedWorkClaim>,
) -> Result<(), StoreError> {
    if input.is_none() && queued.is_none() {
        return Ok(());
    }
    let Some(mut admission) = super::queued_run::load_run_tx(tx, session_id, None).await? else {
        return Ok(());
    };
    if &admission.position.turn_id != turn_id {
        return Ok(());
    }
    let members = input
        .into_iter()
        .flat_map(|claim| claim.inputs.iter())
        .map(|input| QueuedRunMember::Input(input.input_id.clone()))
        .chain(
            queued
                .into_iter()
                .flat_map(|claim| claim.batches.iter())
                .map(|batch| QueuedRunMember::Batch(batch.batch_id.clone())),
        );
    let mut changed = false;
    for member in members {
        if !admission.assigned_members.contains(&member) {
            admission.assigned_members.push(member);
            changed = true;
        }
    }
    if changed {
        super::queued_run::write_run_tx(tx, &admission, false).await?;
    }
    Ok(())
}
