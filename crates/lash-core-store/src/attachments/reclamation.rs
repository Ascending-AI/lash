use super::*;
use std::collections::HashSet;

/// Pages through every blob in `backend`, computes the live root set from
/// `root_set` (committed refs plus intents whose durable owners can still
/// commit), and deletes every blob no session references. The policy's
/// `grace_period_ms` retention window delays reclamation after owner death and
/// protects freshly written blobs even if they currently look unreferenced.
/// An empty live root set refuses deletion by default; the host must explicitly
/// authorize the destructive interpretation through [`EmptyRootSetPolicy`].
/// This valve catches an empty root set, not a wrong store whose unrelated data
/// makes its root set non-empty. A refusal is an
/// [`AttachmentReclamationFailure`] carrying
/// [`MaintenanceRefusal::EmptyRootSetUnauthorized`](crate::store::MaintenanceRefusal::EmptyRootSetUnauthorized)
/// and the partial report accumulated before the first eligible blob.
/// Per-blob final-`HEAD`, delete, and condemnation-settlement failures are collected into
/// [`AttachmentReclamationReport::failed_ids`]; the sweep attempts the remaining
/// candidates, then returns `Ok(report)` classified as
/// [`MaintenanceSweep::Incomplete`](crate::store::MaintenanceSweep::Incomplete).
/// Failed or stalled deletes are completed sweep outcomes. `Err` is reserved
/// for a refusal or a failure that prevents the sweep from completing.
///
/// Blob listing retains at most one page. Live roots, settled condemnation ids
/// and accumulated report ids remain separate memory limits. Concurrent inserts
/// behind the cursor are deferred to the next sweep.
///
/// # Two reconciliation windows, one grace period
///
/// `grace_period_ms` gates two independent hazards, both keyed off the same
/// value:
///
/// * *Terminal-owner retention.* An uncommitted intent older than
///   `now - grace_period_ms` is forgotten only when its turn has been
///   superseded, its process row has been pruned, or it has no durable owner.
///   Age never proves a turn or process dead.
/// * *Delete-time freshness race.* The `list` snapshot's `last_modified` is
///   stale by the time the sweep reaches a candidate. Before deleting, the sweep
///   re-fetches the blob's freshness with [`AttachmentStore::head`] and spares
///   any blob touched within the window — covering the interleaving where a new
///   intent plus a `put` of the same content id lands after the root snapshot
///   was taken (the `put` refreshes the blob's modification time).
///
/// # The delete window is fenced, not raced
///
/// A snapshot-then-delete sweep has a window: between deciding a digest is
/// unreferenced and physically deleting it, a session can write the same content
/// and lose its bytes. No re-check closes that window, because a re-check is
/// still a read. The sweep closes it with a CAS state machine in the lash-owned
/// root authority instead — no locks, no leases, no TTLs, no wall clock:
///
/// 1. **Condemn before delete.** For each candidate the sweep runs
///    [`AttachmentRootSet::condemn_attachment`], one conditional mutation that
///    inserts a per-digest condemnation *only if no root or intent exists*. A
///    live root ([`AttachmentCondemnation::RootPresent`]) means the digest is
///    not garbage: skip it.
/// 2. **The writer's intent is the fence on the other side.** Every `put`
///    records its intent through
///    [`AttachmentReferrers::begin_attachment_write`] *before* the bytes land, in
///    the same conditional mutation that reads the condemnation. So a writer
///    either records first (and the condemn CAS fails) or arrives to a condemned
///    digest and claims it with a write token (and the sweep's arm CAS fails).
///    Whoever loses the CAS yields; nobody waits.
/// 3. **Only an armed digest is deleted.** The physical backend delete is issued
///    exclusively for a digest [`AttachmentRootSet::arm_attachment_delete`]
///    moved to `Deleting`. A successful delete, or bytes already gone, retires
///    the condemnation row outright, and condemnation has already removed the
///    upload evidence for the digest, so the bytes are unadoptable until somebody
///    puts them again. A writer that arrives while the delete is in flight
///    records nothing and retries. A failed put preserves `Condemned` unless
///    another edge or pending attempt roots it, in which
///    case that root returns it to `Free`.
///    That is why no SQL/blob-store atomicity is needed: the authority's state
///    machine, not the backend, decides whether bytes may die.
/// 4. **Skip on contention.** A digest a live peer sweep or a restoring writer
///    already holds is recorded in
///    [`AttachmentReclamationReport::condemn_deferred_ids`] and left for later.
///    The sweep never blocks a writer and never waits on a peer.
///
/// # Adoption first: a crashed sweep is finished by the next one
///
/// Every pass runs under its own generation
/// ([`AttachmentRootSet::begin_attachment_sweep`]), stamped on every
/// condemnation it creates, and the authority keeps that generation's
/// liveness for as long as the pass runs. Before it condemns anything new, a
/// pass adopts every condemnation an older generation left whose pass is
/// provably dead ([`AttachmentRootSet::adopt_attachment_condemnations`]) and
/// completes it: it arms a `Condemned` row, re-stats the bytes, deletes them
/// if present, and retires the row. Bytes already gone count as success, and a
/// row already settled is a no-op. Adoption is one CAS per row, so two
/// sweepers adopting at once claim each row exactly once and delete it once.
///
/// A failed final `HEAD` or physical delete keeps the row: it returns to
/// `Condemned`, so a writer can still reclaim the digest, with one more
/// failed attempt recorded. Later sweeps retry after a delay starting at one
/// second and doubling to a fifteen-minute cap, using the store clock. A
/// refusal (credentials, authorization, a terminal or contract failure) stalls
/// the row at once, and a retryable one stalls once
/// the policy's delete-attempt limit is reached (standard preset:
/// [`MAX_ATTACHMENT_DELETE_ATTEMPTS`]). A stalled row is
/// retried by later sweeps with capped backoff and stays listed until success
/// in [`AttachmentReclamationReport::stalled_ids`] and, with its typed reason,
/// in [`AttachmentRootSet::list_condemnations`].
///
/// Reclamation is still executed by the host: the sweep condemns only what it
/// was about to delete anyway, and lash expires nothing on a timer. Clearing a
/// condemnation is never host policy; there is no host lever for it.
///
/// # Unfenced authorities are best-effort
///
/// A root authority that does not implement the transitions reports
/// [`AttachmentGcFence::BestEffort`] (the default), and the sweep says so in
/// [`AttachmentReclamationReport::fence`] and in a warning. It then runs the
/// legacy path: a targeted root re-check
/// ([`AttachmentRootSet::has_live_attachment_ref`]) before the delete, and the
/// same probe again after it, recording any late root in
/// [`AttachmentReclamationReport::deleted_while_referenced`] as detection
/// telemetry. That path detects the loss; it cannot prevent it. Such deployments
/// should point the backend at recoverable deletion — object-store versioning or
/// a quarantine/soft-delete prefix — so the report's coordinates lead to bytes
/// that can be restored.
///
/// # Deployment assumption
///
/// The `backend` instance is assumed exclusive to this lash deployment: every
/// blob it holds was written by this deployment's sessions, so a blob with no
/// live ref is genuinely garbage. Sharing a bucket/directory across
/// deployments would let this sweep delete another deployment's live content.
///
/// # Policy is the host's (ADR-0014)
///
/// This is a lever, not a scheduler: the host passes an
/// [`AttachmentReclamationPolicy`] choosing `grace_period_ms` as a post-terminal
/// retention policy, explicitly decides whether an empty root set may authorize
/// deletion, and chooses when to run it. The window is not a correctness bound
/// on replay duration. The lever does no background work.
#[allow(
    clippy::result_large_err,
    reason = "boxing AttachmentReclamationFailure would change this public maintenance API"
)]
pub async fn reclaim_unreferenced_attachments<R>(
    root_set: &R,
    backend: &dyn AttachmentStore,
    policy: AttachmentReclamationPolicy,
) -> Result<AttachmentReclamationReport, AttachmentReclamationFailure>
where
    R: AttachmentRootSet + ?Sized,
{
    let grace_period_ms = policy.grace_period_ms;
    let mut fence = root_set.fence();
    let mut report = AttachmentReclamationReport {
        fence,
        ..AttachmentReclamationReport::default()
    };
    let now = now_epoch_ms();
    let live = match root_set.live_attachment_refs().await {
        Ok(live) => live,
        Err(err) => {
            let mut after = None;
            let mut eligible = false;
            loop {
                let page = match read_page(backend, after.as_ref()).await {
                    Ok(page) => page,
                    Err(error) if report.scanned_blob_count == 0 => {
                        return Err(AttachmentReclamationFailure::failed_before_any_work(error));
                    }
                    Err(error) => return Err(AttachmentReclamationFailure::failed(error, report)),
                };
                report.scanned_blob_count += page.blobs.len();
                eligible |= page
                    .blobs
                    .iter()
                    .any(|blob| !within_grace(blob.last_modified_epoch_ms, now, grace_period_ms));
                match page.next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
            let failure = format!("failed to enumerate live attachment refs: {err}");
            // Enumeration failure deliberately splits by destructive scope. An
            // eligible blob needed the unavailable roots to decide its fate,
            // so that is a backend failure (`Failed`). Grace-protected blobs
            // require no destructive step, but their scope remains unwitnessed,
            // so that is a policy refusal (`Refused(UnwitnessedScope)`).
            if eligible {
                return Err(AttachmentReclamationFailure::failed(
                    AttachmentStoreError::RootSetEnumerationFailed {
                        source: Box::new(err),
                    },
                    report,
                ));
            }
            tracing::warn!(
                scanned_blob_count = report.scanned_blob_count,
                grace_period_ms,
                root_enumeration_failure = %failure,
                "attachment GC could not enumerate live roots but found no deletion-eligible blobs"
            );
            report.root_enumeration_failure = Some(failure);
            if report.scanned_blob_count == 0 {
                // The backend itself enumerated completely and held nothing, so
                // no blob's fate ever depended on the root set. That is a
                // witnessed nothing-to-do — the case a fresh deployment or an
                // operator reset lands in — and the enumeration diagnostic
                // rides along on a report that is provably empty.
                return Ok(report);
            }
            // Blobs exist and only the grace window spared them. Their liveness
            // *would* have been decided by a root set nobody could enumerate, so
            // this sweep refuses rather than reporting itself healthy; the
            // diagnostic rides in the partial report (ADR 0067 §5).
            return Err(AttachmentReclamationFailure::refused(
                crate::store::MaintenanceRefusal::UnwitnessedScope {
                    scope: "live attachment root set",
                },
                report,
            ));
        }
    };
    // (0) Adoption first (ADR 0067 §6). A fenced pass opens its generation and
    // finishes every condemnation a dead predecessor left before it looks at a
    // single new candidate, so a crash between condemn and delete strands
    // neither the row nor the bytes.
    let generation = if fence == AttachmentGcFence::Fenced {
        Some(root_set.begin_attachment_sweep().await.map_err(|source| {
            AttachmentReclamationFailure::failed_before_any_work(
                AttachmentStoreError::RootSetOperationFailed {
                    operation: "begin sweep",
                    source: Box::new(source),
                },
            )
        })?)
    } else {
        None
    };
    let mut settled = HashSet::new();
    if let Some(generation) = &generation {
        let adoption = match root_set.adopt_attachment_condemnations(generation).await {
            Ok(adoption) => adoption,
            Err(source) => {
                return Err(AttachmentReclamationFailure::failed(
                    AttachmentStoreError::RootSetOperationFailed {
                        operation: "adopt condemnations",
                        source: Box::new(source),
                    },
                    report,
                ));
            }
        };
        let AttachmentCondemnationAdoption {
            adopted,
            held_by_live_pass,
            backing_off,
            stalled,
        } = adoption;
        settled.extend(held_by_live_pass.iter().cloned());
        settled.extend(backing_off.iter().cloned());
        settled.extend(stalled.iter().cloned());
        report.condemn_deferred_ids.extend(held_by_live_pass);
        report.condemn_deferred_ids.extend(backing_off);
        report.stalled_ids.extend(stalled);
        report.adopted_count = adopted.len();
        for condemnation in adopted {
            settled.insert(condemnation.digest.clone());
            complete_condemnation(
                root_set,
                backend,
                generation,
                &live,
                Candidate {
                    id: &condemnation.digest,
                    phase: condemnation.phase,
                    delete_attempts: condemnation.delete_attempts,
                    delete_attempt_limit: policy.delete_attempt_limit,
                    stalled: condemnation.stalled,
                },
                grace_period_ms,
                &mut report,
            )
            .await;
        }
    }
    if fence == AttachmentGcFence::BestEffort {
        tracing::warn!(
            scanned_blob_count = report.scanned_blob_count,
            "attachment GC is running against an unfenced root authority: deletes are \
             best-effort and a same-content write landing in the query/delete window can \
             lose its bytes. Point the backend at recoverable deletion (object-store \
             versioning or a quarantine prefix) or use a root authority that implements \
             the condemnation CAS."
        );
    }
    let mut after = None;
    loop {
        let page = match read_page(backend, after.as_ref()).await {
            Ok(page) => page,
            Err(error) if report.adopted_count == 0 && report.scanned_blob_count == 0 => {
                return Err(AttachmentReclamationFailure::failed_before_any_work(error));
            }
            Err(error) => return Err(AttachmentReclamationFailure::failed(error, report)),
        };
        report.scanned_blob_count += page.blobs.len();
        for blob in page.blobs {
            if settled.contains(&blob.id) || live.contains(&blob.id) {
                continue;
            }
            if within_grace(blob.last_modified_epoch_ms, now, grace_period_ms) {
                // Fresh write or in-flight intent per the (possibly stale) snapshot.
                continue;
            }
            // (a) Condemn before delete. One conditional mutation against the same
            // authority a writer records its intent in: from here on, a concurrent
            // writer for this digest either loses this CAS or revokes what it
            // created, and either way the physical delete below is never issued
            // behind a live intent. An unfenced authority answers `Unsupported` and
            // the legacy re-check path runs instead.
            if let Some(generation) = generation
                .as_ref()
                .filter(|_| fence == AttachmentGcFence::Fenced)
            {
                match root_set.condemn_attachment(&blob.id, generation).await {
                    Ok(AttachmentCondemnation::Condemned) => {
                        if live.is_empty()
                            && policy.empty_root_set != EmptyRootSetPolicy::AuthorizeDeleteAll
                        {
                            warn_empty_root_set_refused(&report, &blob.id, policy);
                            let _ = root_set
                                .settle_attachment_condemnation(
                                    &blob.id,
                                    generation,
                                    AttachmentCondemnationSettlement::Spared,
                                )
                                .await;
                            return Err(AttachmentReclamationFailure::refused(
                                crate::store::MaintenanceRefusal::EmptyRootSetUnauthorized,
                                report,
                            ));
                        }
                        complete_condemnation(
                            root_set,
                            backend,
                            generation,
                            &live,
                            Candidate {
                                id: &blob.id,
                                phase: AttachmentCondemnationPhase::Condemned,
                                delete_attempts: 0,
                                delete_attempt_limit: policy.delete_attempt_limit,
                                stalled: None,
                            },
                            grace_period_ms,
                            &mut report,
                        )
                        .await;
                        continue;
                    }
                    // A root appeared since the snapshot: not garbage.
                    Ok(AttachmentCondemnation::RootPresent) => continue,
                    // Skip-on-contention: a live peer sweep or a writer holds it.
                    Ok(AttachmentCondemnation::AlreadyCondemned) => {
                        report.condemn_deferred_ids.push(blob.id);
                        continue;
                    }
                    Ok(AttachmentCondemnation::Unsupported) => {
                        // A self-declared `Fenced` authority that cannot actually
                        // condemn is a partial implementation. Believe the
                        // transition, not the flag: this sweep's deletes run the
                        // unfenced path, so it reports itself best-effort like any
                        // other unfenced sweep.
                        tracing::error!(
                            attachment_id = %blob.id,
                            "attachment root authority reported `Fenced` but answered \
                             `Unsupported` to condemn_attachment; this sweep's deletes are \
                             NOT fenced and are reported best-effort. Implement the complete \
                             fence methods (including AttachmentReferrers::begin_attachment_write) \
                             or report AttachmentGcFence::BestEffort"
                        );
                        fence = AttachmentGcFence::BestEffort;
                        report.fence = AttachmentGcFence::BestEffort;
                    }
                    // Could not reach the authority: do not delete a blob we cannot
                    // fence.
                    Err(error) => {
                        record_reclamation_failure(
                            &mut report,
                            blob.id,
                            AttachmentStoreError::RootSetOperationFailed {
                                operation: "condemn",
                                source: Box::new(error),
                            },
                        );
                        continue;
                    }
                }
            }
            // (b) Unfenced path. Delete-time freshness re-check: the snapshot's
            // freshness is stale, so re-stat the blob immediately before deleting.
            // A concurrent new-intent-plus-`put` of the same content id — landed
            // after the root snapshot — refreshes the blob's modification time;
            // spare it so a newly-referenced blob is never reclaimed out from
            // under its intent.
            match backend.head(&blob.id).await {
                Ok(Some(fresh)) => {
                    if within_grace(
                        fresh.last_modified_epoch_ms,
                        now_epoch_ms(),
                        grace_period_ms,
                    ) {
                        continue;
                    }
                }
                // Already gone (a lifecycle expiry or concurrent delete).
                Ok(None) => continue,
                // Could not re-stat: treat as a per-blob failure rather than risk
                // deleting a blob we can no longer vouch for.
                Err(error) => {
                    record_reclamation_failure(&mut report, blob.id, error);
                    continue;
                }
            }
            // A targeted root re-check for THIS id. The `live` snapshot was taken
            // before the per-blob loop began; a session may have recorded a fresh
            // intent for this content id since. It observes an intent recorded
            // before the delete because the facade's `put` records the write-ahead
            // intent BEFORE the backend `put`. It cannot observe one recorded after
            // it, which is the window only the fence closes.
            match root_set.has_live_attachment_ref(&blob.id).await {
                Ok(true) => continue,
                Ok(false) => {}
                // Could not probe the root set: do not delete a blob we can no longer
                // prove is unreferenced.
                Err(error) => {
                    record_reclamation_failure(
                        &mut report,
                        blob.id,
                        AttachmentStoreError::RootSetOperationFailed {
                            operation: "probe live reference",
                            source: Box::new(error),
                        },
                    );
                    continue;
                }
            }
            if live.is_empty() && policy.empty_root_set != EmptyRootSetPolicy::AuthorizeDeleteAll {
                warn_empty_root_set_refused(&report, &blob.id, policy);
                // The refusal carries the work already done — including any
                // `deleted_while_referenced` detections — instead of discarding it.
                return Err(AttachmentReclamationFailure::refused(
                    crate::store::MaintenanceRefusal::EmptyRootSetUnauthorized,
                    report,
                ));
            }
            match delete_witnessed_attachment(backend, &live, &blob.id).await {
                Ok(()) => {
                    report.reclaimed_count += 1;
                    detect_deleted_while_referenced(root_set, &blob.id, fence, &mut report).await;
                }
                Err(error) => {
                    record_reclamation_failure(&mut report, blob.id, error);
                }
            }
        }
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    Ok(report)
}

async fn delete_witnessed_attachment(
    backend: &dyn AttachmentStore,
    live: &CompleteAttachmentRoots,
    id: &AttachmentId,
) -> Result<(), AttachmentStoreError> {
    if live.contains(id) {
        return Err(AttachmentStoreError::Contract(format!(
            "a rooted attachment {id} reached physical reclamation"
        )));
    }
    backend.delete(id).await
}

fn record_reclamation_failure(
    report: &mut AttachmentReclamationReport,
    id: AttachmentId,
    error: AttachmentStoreError,
) {
    tracing::warn!(attachment_id = %id, error = %error, retryable = error.is_retryable(),
        "attachment sweep item failed; the completed pass reports incomplete work");
    report.failed_ids.push(id);
}

fn warn_empty_root_set_refused(
    report: &AttachmentReclamationReport,
    id: &AttachmentId,
    policy: AttachmentReclamationPolicy,
) {
    tracing::warn!(
        live_root_count = 0,
        scanned_blob_count = report.scanned_blob_count,
        deletion_candidate_id = %id,
        grace_period_ms = policy.grace_period_ms,
        empty_root_set_policy = ?policy.empty_root_set,
        "attachment GC refused an empty live root set with a deletion-eligible blob"
    );
}

/// One condemnation a fenced pass owns and is about to complete.
struct Candidate<'a> {
    id: &'a AttachmentId,
    phase: AttachmentCondemnationPhase,
    /// Failed deletes recorded before this pass.
    delete_attempts: u32,
    delete_attempt_limit: std::num::NonZeroU32,
    stalled: Option<AttachmentDeleteStallReason>,
}

/// Arm (when still `Condemned`), re-stat, delete, and settle one condemnation
/// `generation` owns. Every outcome settles the row: bytes gone retire it,
/// refreshed bytes spare it, and a failure keeps it for the next sweep or
/// stalls it — the row is never dropped while the bytes may remain.
async fn complete_condemnation<R>(
    root_set: &R,
    backend: &dyn AttachmentStore,
    generation: &AttachmentSweepGeneration,
    live: &CompleteAttachmentRoots,
    candidate: Candidate<'_>,
    grace_period_ms: u64,
    report: &mut AttachmentReclamationReport,
) where
    R: AttachmentRootSet + ?Sized,
{
    let id = candidate.id;
    if live.contains(id) {
        let _ = root_set
            .settle_attachment_condemnation(
                id,
                generation,
                AttachmentCondemnationSettlement::Spared,
            )
            .await;
        return;
    }
    if candidate.phase == AttachmentCondemnationPhase::Condemned {
        // Arm before the final HEAD. Once `Deleting` is recorded, a writer
        // cannot revoke the condemnation between observing absent bytes and
        // retiring the condemnation row.
        match root_set.arm_attachment_delete(id, generation).await {
            Ok(AttachmentDeleteArming::Armed) => {}
            // This pass no longer owns the transition: a writer took the
            // digest back. It must not settle a writer's claim.
            Ok(AttachmentDeleteArming::Revoked) => {
                report.condemn_deferred_ids.push(id.clone());
                return;
            }
            // The row stays `Condemned` under this generation; the next sweep
            // adopts it.
            Err(error) => {
                record_reclamation_failure(
                    report,
                    id.clone(),
                    AttachmentStoreError::RootSetOperationFailed {
                        operation: "arm delete",
                        source: Box::new(error),
                    },
                );
                return;
            }
        }
    }
    // Delete-time freshness re-check. Under the fence this is a cheap
    // pre-filter: bytes refreshed within the window are spared.
    match backend.head(id).await {
        Ok(Some(fresh))
            if within_grace(
                fresh.last_modified_epoch_ms,
                now_epoch_ms(),
                grace_period_ms,
            ) =>
        {
            settle(
                root_set,
                id,
                generation,
                AttachmentCondemnationSettlement::Spared,
                report,
            )
            .await;
            return;
        }
        Ok(Some(_)) => {}
        // Already gone (a lifecycle expiry, a concurrent delete, or a crashed
        // predecessor's delete that landed): exactly as a completed delete.
        Ok(None) => {
            settle(
                root_set,
                id,
                generation,
                AttachmentCondemnationSettlement::Deleted,
                report,
            )
            .await;
            return;
        }
        Err(error) => {
            record_failed_delete(root_set, &candidate, generation, error, report).await;
            return;
        }
    }
    match delete_witnessed_attachment(backend, live, id).await {
        Ok(()) => {
            report.reclaimed_count += 1;
            detect_deleted_while_referenced(root_set, id, AttachmentGcFence::Fenced, report).await;
            settle(
                root_set,
                id,
                generation,
                AttachmentCondemnationSettlement::Deleted,
                report,
            )
            .await;
        }
        Err(error) => {
            record_failed_delete(root_set, &candidate, generation, error, report).await;
        }
    }
}

/// Keep a condemnation whose final `HEAD` or delete failed: it returns to
/// `Condemned` with one more attempt, and stalls when retrying cannot help or
/// the attempts reach the bound.
async fn record_failed_delete<R>(
    root_set: &R,
    candidate: &Candidate<'_>,
    generation: &AttachmentSweepGeneration,
    error: AttachmentStoreError,
    report: &mut AttachmentReclamationReport,
) where
    R: AttachmentRootSet + ?Sized,
{
    let attempts = candidate.delete_attempts.saturating_add(1);
    let stall = candidate.stalled.or_else(|| {
        if !error.is_retryable() {
            Some(AttachmentDeleteStallReason::Refused)
        } else if attempts >= candidate.delete_attempt_limit.get() {
            Some(AttachmentDeleteStallReason::AttemptsExhausted)
        } else {
            None
        }
    });
    let message = error.to_string();
    record_reclamation_failure(report, candidate.id.clone(), error);
    match root_set
        .settle_attachment_condemnation(
            candidate.id,
            generation,
            AttachmentCondemnationSettlement::Failed {
                stall,
                error: message.clone(),
            },
        )
        .await
    {
        Ok(AttachmentSettlementOutcome::Applied) => {
            if let Some(reason) = stall {
                tracing::error!(
                    attachment_id = %candidate.id,
                    attempts,
                    reason = reason.as_str(),
                    error = %message,
                    "attachment GC stalled a condemned digest whose delete keeps failing; \
                     later sweeps retry it with capped backoff and its bytes remain"
                );
                report.stalled_ids.push(candidate.id.clone());
            }
        }
        // Unreachable while this pass holds its generation; the row is
        // whoever owns it now.
        Ok(AttachmentSettlementOutcome::NotOwned) => {}
        // The row stays `Deleting` under this generation; the next sweep
        // adopts it and re-stats the bytes.
        Err(_) => {}
    }
}

/// Settle one owned condemnation, reporting a failed settlement. A row left
/// unsettled stays with this generation and the next sweep adopts it.
async fn settle<R>(
    root_set: &R,
    id: &AttachmentId,
    generation: &AttachmentSweepGeneration,
    settlement: AttachmentCondemnationSettlement,
    report: &mut AttachmentReclamationReport,
) where
    R: AttachmentRootSet + ?Sized,
{
    if let Err(error) = root_set
        .settle_attachment_condemnation(id, generation, settlement)
        .await
    {
        record_reclamation_failure(
            report,
            id.clone(),
            AttachmentStoreError::RootSetOperationFailed {
                operation: "settle condemnation",
                source: Box::new(error),
            },
        );
    }
}

/// Post-delete detection probe, taken while the digest is still `Deleting`
/// under a fenced authority. There it must never answer `true`: no writer can
/// record a root for an armed digest, so a root here means the fence is not
/// holding (transitions that are not atomic with intent recording, or two
/// authorities over one backend). Under an unfenced authority it is the loss
/// detector for the window the legacy path cannot close. Either way the bytes
/// are already gone; a failed probe cannot un-delete them.
async fn detect_deleted_while_referenced<R>(
    root_set: &R,
    id: &AttachmentId,
    fence: AttachmentGcFence,
    report: &mut AttachmentReclamationReport,
) where
    R: AttachmentRootSet + ?Sized,
{
    if let Ok(true) = root_set.has_live_attachment_ref(id).await {
        tracing::error!(
            attachment_id = %id,
            fence = ?fence,
            "attachment GC deleted a blob that a live root exists for; the \
             bytes are unrecoverable from lash. Under a fenced root authority \
             this indicates the condemnation CAS is not atomic with intent \
             recording; under an unfenced one it is the delete-window race \
             the fence exists to close"
        );
        report.deleted_while_referenced.push(id.clone());
    }
}

/// A backend that cannot report a modification time (`None`) is treated as past the window,
/// matching [`StoredBlobRef`].
fn within_grace(last_modified_epoch_ms: Option<u64>, now: u64, grace_period_ms: u64) -> bool {
    last_modified_epoch_ms.is_some_and(|modified| now.saturating_sub(modified) < grace_period_ms)
}

// A bounded batch for blob listing; no root/report-memory claim is made.
const BLOB_PAGE_LIMIT: std::num::NonZeroU32 = std::num::NonZeroU32::MIN.saturating_add(255);

async fn read_page(
    backend: &dyn AttachmentStore,
    after: Option<&AttachmentId>,
) -> Result<AttachmentBlobPage, AttachmentStoreError> {
    let page = backend.list_page(after, BLOB_PAGE_LIMIT).await?;
    let mut previous = after;
    for blob in &page.blobs {
        if previous.is_some_and(|id| &blob.id <= id) {
            return Err(AttachmentStoreError::Contract(
                "blob page did not progress exclusively by content id".into(),
            ));
        }
        previous = Some(&blob.id);
    }
    if page.blobs.len() > BLOB_PAGE_LIMIT.get() as usize
        || page
            .next
            .as_ref()
            .is_some_and(|next| Some(next) != previous || after.is_some_and(|after| next <= after))
        || (page.next.is_some() && page.blobs.is_empty())
    {
        return Err(AttachmentStoreError::Contract(
            "invalid blob page cursor or limit".into(),
        ));
    }
    Ok(page)
}
