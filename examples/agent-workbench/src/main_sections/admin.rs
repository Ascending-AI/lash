// Operator-only maintenance surface: destructive, deployment-wide verbs that
// no chat participant may reach and that nothing schedules.

/// Operator-supplied retention bound for `prune_trigger_mutation_receipts`.
///
/// There is no default and no relative form ("older than 30 days"): the caller
/// states an absolute instant, because the only person who can justify the
/// bound is the one who knows every retry horizon this deployment runs.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct PruneTriggerMutationReceiptsRequest {
    before_epoch_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PruneTriggerMutationReceiptsResponse {
    pruned: usize,
    before_epoch_ms: u64,
}

/// Operator-invoked reclamation of trigger mutation idempotency receipts.
///
/// **The caller owns the safety argument, and it is not a small one.** A
/// mutation receipt is what makes a retried trigger command a replay instead
/// of a second execution. Deleting one does not merely free a row: it converts
/// every still-possible retry of that `operation_id` into a fresh evaluation
/// against whatever the world looks like *now*. An outcome the caller already
/// observed is recomputed, and a retry that was a safe no-op can turn into a
/// terminal refusal against state the original call itself created — see
/// `pruning_a_mutation_receipt_turns_a_safe_redrive_into_a_terminal_conflict`.
/// So `before_epoch_ms` must be proven to sit outside
/// **every** retry horizon this deployment can produce — Restate invocation
/// retention, cron redrive windows, an operator's own manual replay, a queued
/// turn that has been parked for a week — not merely "older than it looks
/// useful". Lash cannot prove that for the host, which is exactly why
/// [`lash::triggers::TriggerStore::prune_mutation_receipts`] is a bare
/// primitive with no facade and no schedule behind it (FIG-653 owns the
/// terminal-gated eligibility rule that would make one safe).
///
/// It is therefore wired as an explicit request an operator has to compose,
/// with an absolute cutoff and no default. There is deliberately no button in
/// the workbench UI and no periodic job: this must never be one click or one
/// timer away, and the workbench does not schedule it.
async fn prune_trigger_mutation_receipts(
    State(state): State<AppState>,
    Json(request): Json<PruneTriggerMutationReceiptsRequest>,
) -> Result<Json<PruneTriggerMutationReceiptsResponse>, AppError> {
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::PruneTriggerMutationReceipts)?;
    let pruned = state
        .trigger_store
        .prune_mutation_receipts(request.before_epoch_ms)
        .await
        // Audited: first-party trigger-store maintenance has no session
        // tombstone path or effect-controller boundary.
        .map_err(AppError::internal)?;
    state.trace(
        "admin.trigger_mutation_receipts.pruned",
        json!({
            "before_epoch_ms": request.before_epoch_ms,
            "pruned": pruned,
        }),
    );
    Ok(Json(PruneTriggerMutationReceiptsResponse {
        pruned,
        before_epoch_ms: request.before_epoch_ms,
    }))
}
