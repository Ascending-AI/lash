// Operator-only maintenance surface: destructive, deployment-wide verbs that
// no chat participant may reach and that nothing schedules.

use lash::persistence::EmptyRootSetPolicy;

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

/// Operator-supplied plan for one store-growth maintenance pass.
///
/// Both levers are opt-in and neither has a default: a request that names no
/// session and asks for no sweep is rejected rather than quietly interpreted.
/// Unknown fields are rejected for the same reason `empty_root_set` has no
/// default: both lever fields *do* default, so a misspelled
/// `reclaim_attachments` would otherwise deserialize into "sweep nothing" and
/// report a successful pass that did none of what was asked. On a route whose
/// requests are composed by hand, a typo must be a `400`, not a silent reading.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RunStoreMaintenanceRequest {
    /// Sessions whose stores to vacuum, named one at a time.
    ///
    /// There is deliberately no "every session" form. `vacuum` is scoped to the
    /// session its store handle is bound to, and the host that can justify
    /// reclaiming a particular session's settled rows is the one that knows the
    /// session is not about to be resumed.
    #[serde(default)]
    vacuum_session_ids: Vec<String>,
    /// The attachment sweep, omitted when this pass is vacuum-only.
    #[serde(default)]
    reclaim_attachments: Option<ReclaimAttachmentsRequest>,
}

/// The two decisions `lash::persistence::AttachmentReclamationPolicy` leaves to
/// the host, lifted verbatim into the request so an operator states both.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReclaimAttachmentsRequest {
    /// Post-terminal retention *and* the delete-time freshness window — the one
    /// number standing between an in-flight upload and deletion. See the
    /// handler's doctrine: too small a value deletes live user content on a
    /// perfectly configured deployment.
    grace_period_ms: u64,
    /// How an empty live root set may be read. No serde default: the safe
    /// reading and the wipe-everything reading differ by one word, so the word
    /// is required.
    empty_root_set: EmptyRootSetAuthorization,
}

/// Operator authorization for the destructive reading of an empty root set.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum EmptyRootSetAuthorization {
    Refuse,
    AuthorizeDeleteAll,
}

impl EmptyRootSetAuthorization {
    fn policy(self) -> EmptyRootSetPolicy {
        match self {
            Self::Refuse => EmptyRootSetPolicy::Refuse,
            Self::AuthorizeDeleteAll => EmptyRootSetPolicy::AuthorizeDeleteAll,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RunStoreMaintenanceResponse {
    vacuumed: Vec<SessionVacuumReport>,
    reclaimed_attachments: Option<AttachmentReclamationSummary>,
    /// The policy the sweep actually ran under, echoed back the way the receipt
    /// prune above echoes `before_epoch_ms`. A destructive result is only
    /// readable next to the arguments that produced it: `reclaimed_count: 0`
    /// under a week-long grace period and the same zero under a one-second one
    /// are opposite findings, and an operator reading a stored response should
    /// not have to trust that the request they still have is the one that ran.
    reclaim_policy: Option<ReclaimAttachmentsRequest>,
}

/// One session's `lash::persistence::VacuumReport`, kept keyed by session
/// because the lever is session-scoped and a deployment-wide total would hide
/// which session actually grew.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct SessionVacuumReport {
    session_id: String,
    removed_node_count: usize,
    removed_pending_turn_input_tombstone_count: usize,
}

/// The whole of `lash::persistence::AttachmentReclamationReport`, projected to
/// JSON.
///
/// Every field is surfaced, including the ones that report a *degraded* sweep:
/// an operator who only ever sees `reclaimed_count` cannot tell a healthy empty
/// sweep from a root enumeration that failed, or a fenced delete from a
/// best-effort one that may have raced a writer.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct AttachmentReclamationSummary {
    scanned_blob_count: usize,
    reclaimed_count: usize,
    failed_ids: Vec<String>,
    condemn_deferred_ids: Vec<String>,
    deleted_while_referenced: Vec<String>,
    root_enumeration_failure: Option<String>,
    fence: SweepFence,
}

/// `lash::persistence::AttachmentGcFence` on the wire.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SweepFence {
    Fenced,
    BestEffort,
}

impl From<lash::persistence::AttachmentGcFence> for SweepFence {
    fn from(fence: lash::persistence::AttachmentGcFence) -> Self {
        match fence {
            lash::persistence::AttachmentGcFence::Fenced => Self::Fenced,
            lash::persistence::AttachmentGcFence::BestEffort => Self::BestEffort,
        }
    }
}

/// Operator-invoked pass over the two levers that bound session-store growth.
///
/// **The caller owns the safety argument here too, and it has two halves.**
///
/// *Vacuum* physically deletes rows that have already settled: tombstoned graph
/// nodes, and pending-turn-input evidence in a terminal state. It never touches
/// live history, and it is scoped to one session's store — so the request names
/// sessions rather than offering a catalog-wide sweep the contract does not
/// have.
///
/// *Attachment reclamation* deletes bytes. It is a mark-and-sweep over the blob
/// backend whose mark phase is the **root set** this host supplies: the session
/// store factory, which is the deployment's
/// [`lash::persistence::AttachmentRootSet`]. That is the whole safety argument
/// in one sentence — every blob not reachable from that authority is deleted,
/// so pointing the sweep at the wrong factory deletes live content. The
/// deployment assumption the lever documents applies verbatim: the attachment
/// backend must be exclusive to this deployment.
///
/// The sharpest edge is an *empty* root set. A root authority that enumerates
/// zero live refs is far more often a misconfiguration — an empty catalog, a
/// factory pointed at the wrong directory — than a deployment that genuinely
/// references nothing, and reading it as "delete everything" would turn that
/// misconfiguration into total data loss. Lash therefore refuses it by default
/// and makes the destructive reading an explicit host assertion
/// ([`lash::persistence::EmptyRootSetPolicy`]). This route keeps the refusal
/// visible instead of papering over it: `empty_root_set` has no default, and a
/// refusal comes back as a `409` naming what was refused rather than as a
/// success report claiming a quiet zero-deletion sweep.
///
/// **A correctly configured factory is not on its own enough, and this host is
/// the proof.** The workbench's upload endpoint writes bytes with a bare
/// [`lash::persistence::AttachmentStore::put`] and no manifest intent, so a
/// freshly uploaded blob is an *unscoped host put*: nothing references it, and
/// the root authority — correctly configured, enumerating every session it owns
/// — cannot see it. Until the user actually sends the message that commits the
/// ref, `grace_period_ms` is the only thing keeping it alive; it is the
/// age-only fallback the root set documents for exactly this case. So the
/// grace period is not a tidiness knob. **A grace period shorter than the
/// window between "operator uploads a file" and "operator sends the turn that
/// attaches it" deletes live user content on a perfectly configured
/// deployment**, and it does so silently, because a blob nobody has referenced
/// yet is indistinguishable from an orphan. Size it against the slowest
/// upload-to-send path this deployment permits — composing a message, walking away
/// mid-draft, a queued turn waiting on a lease — not against how quickly the
/// bytes would ideally be collected. Hosts that want a tighter window should
/// give uploads a manifest intent so they are roots from the moment they land,
/// rather than shrinking the number that is protecting them.
///
/// Like the receipt prune above, it is wired as an explicit request with no
/// button in the UI and no periodic job. The workbench schedules neither lever.
async fn run_store_maintenance(
    State(state): State<AppState>,
    Json(request): Json<RunStoreMaintenanceRequest>,
) -> Result<Json<RunStoreMaintenanceResponse>, AppError> {
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::RunStoreMaintenance)?;
    if request.vacuum_session_ids.is_empty() && request.reclaim_attachments.is_none() {
        return Err(AppError::bad_request(
            "store maintenance names its levers: pass vacuum_session_ids, reclaim_attachments, \
             or both",
        ));
    }
    // Destruction that already happened is traced before any error is
    // propagated. This route runs a sequence of irreversible steps, and the
    // interesting failures are all mid-sequence: session `b` 404s after `a` was
    // vacuumed, or the sweep is refused after three sessions were. Returning
    // the error alone would leave the operator with a 404 and no record of the
    // rows that are already gone -- an audit trail that reports only the passes
    // that completed cleanly is worse than none, because it reads as if nothing
    // happened.
    let mut vacuumed = Vec::with_capacity(request.vacuum_session_ids.len());
    for session_id in &request.vacuum_session_ids {
        match vacuum_session_store(&state, session_id).await {
            Ok(report) => vacuumed.push(report),
            Err(error) => {
                trace_store_maintenance(
                    &state,
                    "aborted",
                    &vacuumed,
                    None,
                    request.reclaim_attachments,
                );
                return Err(error);
            }
        }
    }
    let reclaimed_attachments = match request.reclaim_attachments {
        Some(reclaim) => match reclaim_workbench_attachments(&state, reclaim).await {
            Ok(summary) => Some(summary),
            Err(error) => {
                trace_store_maintenance(
                    &state,
                    "aborted",
                    &vacuumed,
                    None,
                    request.reclaim_attachments,
                );
                return Err(error);
            }
        },
        None => None,
    };
    trace_store_maintenance(
        &state,
        "completed",
        &vacuumed,
        reclaimed_attachments.as_ref(),
        request.reclaim_attachments,
    );
    Ok(Json(RunStoreMaintenanceResponse {
        vacuumed,
        reclaimed_attachments,
        reclaim_policy: request.reclaim_attachments,
    }))
}

/// Record what this pass destroyed, on the success and failure paths alike.
///
/// `outcome` is passed rather than inferred, so a reader is never left guessing
/// whether a short `vacuumed` list means "that is all that was asked for" or
/// "the rest never ran" -- and so the distinction cannot quietly decay into a
/// heuristic over the fields it is supposed to qualify. The policy is traced
/// whenever one was requested, including when the sweep failed before producing
/// a report, because the grace period is the number a post-incident reader will
/// want first.
fn trace_store_maintenance(
    state: &AppState,
    outcome: &str,
    vacuumed: &[SessionVacuumReport],
    reclaimed_attachments: Option<&AttachmentReclamationSummary>,
    reclaim_policy: Option<ReclaimAttachmentsRequest>,
) {
    state.trace(
        "admin.store_maintenance.ran",
        json!({
            "outcome": outcome,
            "vacuumed": vacuumed,
            "reclaimed_attachments": reclaimed_attachments,
            "reclaim_policy": reclaim_policy,
        }),
    );
}

/// Vacuum one named session's store.
///
/// The handle comes from the factory, which is what binds it to the session;
/// `vacuum` refuses an unbound handle rather than widening to a catalog-wide
/// sweep. `open_existing_store` opens only what is already durable — the
/// create-shaped request carries a policy it never applies on this path — so an
/// unknown session is a `404` and never a freshly created empty store.
async fn vacuum_session_store(
    state: &AppState,
    session_id: &str,
) -> Result<SessionVacuumReport, AppError> {
    let request = lash::persistence::SessionStoreCreateRequest {
        session_id: session_id.to_string(),
        relation: lash::persistence::SessionRelation::Root,
        policy: lash::runtime::SessionPolicy::new(lash::TurnBudget::Unbounded),
    };
    let store = lash::persistence::SessionStoreFactory::open_existing_store(
        state.session_store_factory.as_ref(),
        &request,
    )
    .await
    .map_err(AppError::internal)?
    .ok_or_else(|| {
        AppError::not_found(format!(
            "session `{session_id}` has no durable store to vacuum"
        ))
    })?;
    let report = vacuum_bound_store(store.as_ref()).await?;
    Ok(session_vacuum_report(session_id, report))
}

/// Reclaim the settled rows of the session `store` is bound to.
///
/// Taking the bound handle as the parameter is the point: `vacuum` is scoped to
/// that binding and refuses an unbound handle rather than widening into a
/// catalog-wide sweep, so the binding is what the caller has to get right.
async fn vacuum_bound_store(
    store: &dyn lash::persistence::RuntimePersistence,
) -> Result<lash::persistence::VacuumReport, AppError> {
    lash::persistence::StoreMaintenance::vacuum(store)
        .await
        // Audited: session-scoped store maintenance reclaims rows that already
        // settled; it crosses no effect-controller boundary and produces no
        // tombstone cause of its own.
        .map_err(AppError::internal)
}

/// Project one session's vacuum report onto the wire.
fn session_vacuum_report(
    session_id: &str,
    report: lash::persistence::VacuumReport,
) -> SessionVacuumReport {
    SessionVacuumReport {
        session_id: session_id.to_string(),
        removed_node_count: report.removed_node_count,
        removed_pending_turn_input_tombstone_count: report
            .removed_pending_turn_input_tombstone_count,
    }
}

/// Sweep the attachment backend against an explicitly named root authority.
async fn reclaim_workbench_attachments(
    state: &AppState,
    request: ReclaimAttachmentsRequest,
) -> Result<AttachmentReclamationSummary, AppError> {
    // The root set is named, not inferred. This host's session store factory is
    // its `AttachmentRootSet`, and handing it over explicitly is what makes the
    // sweep's blast radius reviewable: everything this authority cannot reach
    // is about to be deleted.
    let root_set: &dyn lash::persistence::AttachmentRootSet =
        state.session_store_factory.as_ref();
    sweep_unreferenced_attachments(
        root_set,
        state.attachment_store.as_ref(),
        attachment_reclamation_policy(request.grace_period_ms, request.empty_root_set.policy()),
    )
    .await
}

/// The two decisions ADR 0014 leaves to the host, in one place.
///
/// `retention_ms` is a post-terminal retention window *and* the delete-time
/// freshness window; `empty_roots` is the assertion that decides whether an
/// empty root set may authorize deletion at all.
fn attachment_reclamation_policy(
    retention_ms: u64,
    empty_roots: EmptyRootSetPolicy,
) -> lash::persistence::AttachmentReclamationPolicy {
    lash::persistence::AttachmentReclamationPolicy {
        grace_period_ms: retention_ms,
        empty_root_set: empty_roots,
    }
}

/// Run one mark-and-sweep pass and project its report.
///
/// The root authority, the backend and the policy are all parameters rather
/// than things this function reaches for: which authority marks the live set is
/// the entire safety argument, so it is stated by the caller and readable in
/// this signature.
async fn sweep_unreferenced_attachments(
    root_set: &dyn lash::persistence::AttachmentRootSet,
    backend: &dyn lash::persistence::AttachmentStore,
    policy: lash::persistence::AttachmentReclamationPolicy,
) -> Result<AttachmentReclamationSummary, AppError> {
    let report =
        lash::persistence::reclaim_unreferenced_attachments(root_set, backend, policy).await;
    let report = report.map_err(|error| match error {
        lash::persistence::AttachmentStoreError::EmptyRootSetRefused => AppError::conflict(
            "attachment reclamation refused: the root authority enumerated zero live \
             attachment refs while a deletion-eligible blob was present, so proceeding \
             would have deleted every blob in the backend. Confirm the store factory is \
             the one that owns this deployment's sessions; re-send with \
             empty_root_set=authorize_delete_all only if deleting all of it is intended",
        ),
        // Audited: the content-addressed attachment backend has no session
        // identity or tombstone error variant.
        other => AppError::internal(other),
    })?;
    Ok(attachment_reclamation_summary(report))
}

/// Project the sweep's report onto the wire, degraded signals and all.
fn attachment_reclamation_summary(
    report: lash::persistence::AttachmentReclamationReport,
) -> AttachmentReclamationSummary {
    AttachmentReclamationSummary {
        scanned_blob_count: report.scanned_blob_count,
        reclaimed_count: report.reclaimed_count,
        failed_ids: attachment_ids_to_strings(&report.failed_ids),
        condemn_deferred_ids: attachment_ids_to_strings(&report.condemn_deferred_ids),
        deleted_while_referenced: attachment_ids_to_strings(&report.deleted_while_referenced),
        fence: SweepFence::from(report.fence),
        root_enumeration_failure: report.root_enumeration_failure,
    }
}

fn attachment_ids_to_strings(ids: &[lash::attachments::AttachmentId]) -> Vec<String> {
    ids.iter().map(ToString::to_string).collect()
}
