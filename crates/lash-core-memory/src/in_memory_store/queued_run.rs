use super::*;

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
        let groups = run_claim_groups(rows, &indices, fence.fencing_token);
        let mut claims = Vec::with_capacity(groups.len());
        for indices in groups {
            let minted = reclaim_run_claim(
                rows,
                InMemoryClaimMint {
                    selected_indices: &indices,
                    enqueue_seq: rows[indices[0]].input.enqueue_seq,
                    dialect: crate::store::queued_work::ClaimIdDialect::RecordingTurnInput,
                    fencing_label: "turn_input_claim_fencing_token",
                    session_id: &fence.session_id,
                    owner,
                    generation: fence.fencing_token,
                    now,
                },
            )?;
            claims.push(crate::TurnInputClaim {
                session_id: fence.session_id.clone(),
                claim_id: minted.claim_id,
                owner: owner.clone(),
                lease_token: minted.lease_token,
                fencing_token: minted.fencing_token,
                session_lease_generation: fence.fencing_token,
                data: crate::TurnInputClaimData {
                    mode: crate::TurnInputClaimMode::NextTurn,
                    inputs: indices
                        .iter()
                        .map(|index| rows[*index].input.clone())
                        .collect(),
                    applications: Vec::new(),
                },
            });
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
        let groups = run_claim_groups(rows, &indices, fence.fencing_token);
        let mut claims = Vec::with_capacity(groups.len());
        for indices in groups {
            let minted = reclaim_run_claim(
                rows,
                InMemoryClaimMint {
                    selected_indices: &indices,
                    enqueue_seq: rows[indices[0]].batch.enqueue_seq,
                    dialect: crate::store::queued_work::ClaimIdDialect::RecordingQueuedWork,
                    fencing_label: "queued_work_claim_fencing_token",
                    session_id: &fence.session_id,
                    owner,
                    generation: fence.fencing_token,
                    now,
                },
            )?;
            claims.push(crate::QueuedWorkClaim {
                session_id: fence.session_id.clone(),
                claim_id: minted.claim_id,
                owner: owner.clone(),
                lease_token: minted.lease_token,
                fencing_token: minted.fencing_token,
                session_lease_generation: fence.fencing_token,
                data: crate::QueuedWorkClaimData {
                    batches: indices
                        .iter()
                        .map(|index| rows[*index].batch.clone())
                        .collect(),
                    abandon_restore_claim_id: minted.abandon_restore_claim_id,
                    abandon_restore_claim_token: minted
                        .abandon_restore_claim_token
                        .map(String::into_boxed_str),
                },
            });
        }
        Ok(claims)
    }
}

fn reclaim_run_claim<R: InMemoryClaimRow>(
    rows: &mut [R],
    mint: InMemoryClaimMint<'_>,
) -> Result<super::claim_hold::MintedInMemoryClaim, crate::StoreError> {
    let first = rows[mint.selected_indices[0]].claim();
    if first.live_under(Some(mint.generation)) {
        let conflict = || crate::StoreError::QueuedRunConflict {
            session_id: mint.session_id.clone(),
        };
        let claim_id = first.id().ok_or_else(conflict)?;
        let lease_token = first.token().ok_or_else(conflict)?;
        if first.owner().as_ref() != Some(mint.owner)
            || !mint.selected_indices.iter().all(|&index| {
                let held = rows[index].claim();
                held.live_under(Some(mint.generation))
                    && held.owner().as_ref() == Some(mint.owner)
                    && held.owned_by(&claim_id, &lease_token)
            })
        {
            return Err(conflict());
        }
        return Ok(super::claim_hold::MintedInMemoryClaim {
            claim_id,
            lease_token,
            fencing_token: first.fencing_token,
            abandon_restore_claim_id: None,
            abandon_restore_claim_token: None,
        });
    }
    if mint
        .selected_indices
        .iter()
        .any(|&index| rows[index].claim().live_under(Some(mint.generation)))
    {
        return Err(crate::StoreError::QueuedRunConflict {
            session_id: mint.session_id.clone(),
        });
    }
    mint_in_memory_claim(rows, mint)
}

fn run_claim_groups<R: InMemoryClaimRow>(
    rows: &[R],
    indices: &[usize],
    generation: u64,
) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for &index in indices {
        let claim = rows[index].claim();
        let same = groups.last().is_some_and(|group| {
            let prior = rows[group[0]].claim();
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
