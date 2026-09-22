use super::*;
use crate::store::claim_plan::{
    ClaimPlanDecision, QueuedWorkClaimRow, TurnInputClaimRow, plan_queued_work_claim,
    plan_turn_input_claim,
};

impl InMemorySessionStore {
    pub(super) fn assign_checkpoint_members(
        &self,
        session_id: &SessionId,
        turn_id: &crate::TurnId,
        inputs: Option<&crate::TurnInputClaim>,
        queued: Option<&crate::QueuedWorkClaim>,
    ) {
        let mut runs = self.queued_runs.lock_recover();
        let Some(run) = runs.values_mut().find(|run| {
            run.scope.session_id() == Some(session_id)
                && run.terminal.is_none()
                && run.position.turn_id == *turn_id
        }) else {
            return;
        };
        let members = inputs
            .into_iter()
            .flat_map(|claim| &claim.inputs)
            .map(|input| crate::store::QueuedRunMember::Input(input.input_id.clone()))
            .chain(
                queued
                    .into_iter()
                    .flat_map(|claim| &claim.batches)
                    .map(|batch| crate::store::QueuedRunMember::Batch(batch.batch_id.clone())),
            );
        for member in members {
            if !run.assigned_members.contains(&member) {
                run.assigned_members.push(member);
            }
        }
    }

    pub(super) fn reclaim_run_inputs(
        &self,
        rows: &mut [InMemoryPendingTurnInput],
        ids: &[&crate::InputId],
        fence: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        now: u64,
    ) -> Result<Vec<crate::TurnInputClaim>, crate::StoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let indices = ids
            .iter()
            .map(|id| {
                rows.iter()
                    .position(|row| {
                        row.input.session_id == fence.session_id
                            && row.input.input_id == *id
                            && !row.input.state.is_terminal()
                    })
                    .ok_or_else(|| crate::StoreError::QueuedRunConflict {
                        session_id: fence.session_id.clone(),
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let groups = run_claim_groups(&indices, fence.fencing_token, |index| &rows[index].claim);
        let mut claims = Vec::with_capacity(groups.len());
        for indices in groups {
            let held = held_run_claim(&indices, fence, owner, |index| &rows[index].claim)?;
            let claim = if let Some(held) = held {
                crate::TurnInputClaim {
                    session_id: fence.session_id.clone(),
                    claim_id: held.claim_id,
                    owner: owner.clone(),
                    lease_token: held.lease_token,
                    fencing_token: held.fencing_token,
                    session_lease_generation: fence.fencing_token,
                    data: lash_core_store::turn_input_vocabulary::TurnInputClaimData {
                        mode: crate::TurnInputClaimMode::NextTurn,
                        inputs: indices
                            .iter()
                            .map(|&index| rows[index].input.clone())
                            .collect(),
                        applications: Vec::new(),
                    },
                }
            } else {
                let observations = indices
                    .iter()
                    .map(|&index| {
                        let row = &rows[index];
                        TurnInputClaimRow {
                            input: row.input.clone(),
                            enqueue_seq: row.input.enqueue_seq,
                            claim_fencing_token: row.claim.fencing_token,
                            claim_token: row.claim.token(),
                            claim_session_lease_generation: row
                                .claim
                                .diagnostic_generation()
                                .unwrap_or(0),
                        }
                    })
                    .collect();
                let plan = match plan_turn_input_claim(
                    crate::store::queued_work::ClaimIdDialect::RecordingTurnInput,
                    &fence.session_id,
                    owner,
                    fence.fencing_token,
                    now,
                    crate::TurnInputClaimMode::NextTurn,
                    observations,
                )? {
                    ClaimPlanDecision::Complete(plan) => plan,
                    ClaimPlanDecision::Empty | ClaimPlanDecision::Defer => {
                        return Err(crate::StoreError::QueuedRunConflict {
                            session_id: fence.session_id.clone(),
                        });
                    }
                };
                let writes = plan.writes().to_vec();
                let claim = plan.into_claim();
                for (&index, write) in indices.iter().zip(writes) {
                    rows[index].claim.acquire(
                        claim.claim_id.clone(),
                        claim.lease_token.clone(),
                        owner.clone(),
                        fence.fencing_token,
                        write.next_claim_fencing_token,
                    );
                }
                claim
            };
            claims.push(claim);
        }
        Ok(claims)
    }

    pub(super) fn reclaim_run_batches(
        &self,
        rows: &mut [InMemoryQueuedBatch],
        ids: &[&crate::BatchId],
        fence: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        now: u64,
    ) -> Result<Vec<crate::QueuedWorkClaim>, crate::StoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let indices = ids
            .iter()
            .map(|id| {
                rows.iter()
                    .position(|row| {
                        row.batch.session_id == fence.session_id && row.batch.batch_id == *id
                    })
                    .ok_or_else(|| crate::StoreError::QueuedRunConflict {
                        session_id: fence.session_id.clone(),
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let groups = run_claim_groups(&indices, fence.fencing_token, |index| &rows[index].claim);
        let mut claims = Vec::with_capacity(groups.len());
        for indices in groups {
            let held = held_run_claim(&indices, fence, owner, |index| &rows[index].claim)?;
            let claim = if let Some(held) = held {
                crate::QueuedWorkClaim {
                    session_id: fence.session_id.clone(),
                    claim_id: held.claim_id,
                    owner: owner.clone(),
                    lease_token: held.lease_token,
                    fencing_token: held.fencing_token,
                    session_lease_generation: fence.fencing_token,
                    data: crate::store_backend_support::queued_work_claim_data(
                        indices
                            .iter()
                            .map(|&index| rows[index].batch.clone())
                            .collect(),
                        None,
                        None,
                    )?,
                }
            } else {
                let candidates: Vec<_> = indices
                    .iter()
                    .map(|&index| {
                        let row = &rows[index];
                        crate::store::queued_work::ClaimCandidate::from_batch(
                            &row.batch,
                            row.claim.fencing_token,
                            row.claim.id(),
                            row.claim.token(),
                        )
                    })
                    .collect();
                let observations = indices
                    .iter()
                    .zip(&candidates)
                    .map(|(&index, candidate)| {
                        let row = &rows[index];
                        QueuedWorkClaimRow {
                            candidate: candidate.clone(),
                            batch: row.batch.clone(),
                            claim_token: row.claim.token(),
                            claim_session_lease_generation: row
                                .claim
                                .diagnostic_generation()
                                .unwrap_or(0),
                        }
                    })
                    .collect();
                let plan = match plan_queued_work_claim(
                    crate::store::queued_work::ClaimIdDialect::RecordingQueuedWork,
                    &fence.session_id,
                    owner,
                    fence.fencing_token,
                    now,
                    observations,
                    &candidates,
                )? {
                    ClaimPlanDecision::Complete(plan) => plan,
                    ClaimPlanDecision::Empty | ClaimPlanDecision::Defer => {
                        return Err(crate::StoreError::QueuedRunConflict {
                            session_id: fence.session_id.clone(),
                        });
                    }
                };
                let writes = plan.writes().to_vec();
                let claim = plan.into_claim()?;
                for (&index, write) in indices.iter().zip(writes) {
                    rows[index].claim.acquire(
                        claim.claim_id.clone(),
                        claim.lease_token.clone(),
                        owner.clone(),
                        fence.fencing_token,
                        write.next_claim_fencing_token,
                    );
                }
                claim
            };
            claims.push(claim);
        }
        Ok(claims)
    }
}

struct HeldRunClaim {
    claim_id: String,
    lease_token: String,
    fencing_token: u64,
}

fn held_run_claim<'a>(
    indices: &[usize],
    fence: &crate::SessionExecutionLeaseAuthority,
    owner: &crate::LeaseOwnerIdentity,
    claim: impl Fn(usize) -> &'a ClaimHold,
) -> Result<Option<HeldRunClaim>, crate::StoreError> {
    let first = claim(indices[0]);
    let conflict = || crate::StoreError::QueuedRunConflict {
        session_id: fence.session_id.clone(),
    };
    if first.live_under(Some(fence.fencing_token)) {
        let claim_id = first.id().ok_or_else(conflict)?;
        let lease_token = first.token().ok_or_else(conflict)?;
        if !indices.iter().all(|&index| {
            let held = claim(index);
            held.live_under(Some(fence.fencing_token))
                && held.owner().as_ref() == Some(owner)
                && held.owned_by(&claim_id, &lease_token)
        }) {
            return Err(conflict());
        }
        return Ok(Some(HeldRunClaim {
            claim_id,
            lease_token,
            fencing_token: first.fencing_token,
        }));
    }
    if indices
        .iter()
        .any(|&index| claim(index).live_under(Some(fence.fencing_token)))
    {
        return Err(conflict());
    }
    Ok(None)
}

fn run_claim_groups<'a>(
    indices: &[usize],
    generation: u64,
    held: impl Fn(usize) -> &'a ClaimHold,
) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for &index in indices {
        let claim = held(index);
        let same = groups.last().is_some_and(|group| {
            let prior = held(group[0]);
            match (
                prior.live_under(Some(generation)),
                claim.live_under(Some(generation)),
            ) {
                (false, false) => true,
                (true, true) => prior.id() == claim.id() && prior.token() == claim.token(),
                _ => false,
            }
        });
        if same && let Some(group) = groups.last_mut() {
            group.push(index);
        } else {
            groups.push(vec![index]);
        }
    }
    groups
}
