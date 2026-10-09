use super::*;
use lash::SessionId;

// Coverage for `/api/admin/store-maintenance`: the two levers that bound
// session-store growth, and — the point of the route — the destruction it
// refuses to perform.

/// A 1x1 PNG, the smallest thing that survives the workbench upload contract.
const STORE_MAINTENANCE_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

/// Bytes that are never attached to a turn, so nothing ever roots them.
const STORE_MAINTENANCE_ORPHAN_BYTES: &[u8] = b"workbench-store-maintenance-orphan-blob";

/// A retention window no blob written by this test can be older than.
const RECLAIM_RETENTION_WINDOW_MS: u64 = 60 * 60 * 1000;

/// A grace period of zero, used **only** to make a just-written blob instantly
/// deletion-eligible so a test can observe the sweep at all.
///
/// This is never an operator example. `grace_period_ms` is the sole protection
/// for a blob the workbench has uploaded but no turn has referenced yet -- see
/// the doctrine on `run_store_maintenance` -- so a real deployment sizes it
/// against its slowest upload-to-send window. Zero there would delete live user
/// content. It is spelled as a named constant precisely so it cannot be copied
/// out of these tests as a plausible-looking default.
const TEST_ONLY_INSTANT_ELIGIBILITY_MS: u64 = 0;

struct DeleteFailingWorkbenchAttachmentStore {
    inner: Arc<dyn lash::persistence::AttachmentStore>,
    class: lash::persistence::AttachmentStoreFailureClass,
}

#[async_trait]
impl lash::persistence::AttachmentStore for DeleteFailingWorkbenchAttachmentStore {
    fn persistence(&self) -> lash::persistence::AttachmentStorePersistence {
        self.inner.persistence()
    }

    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: lash::attachments::AttachmentCreateMeta,
    ) -> Result<lash::attachments::AttachmentRef, lash::persistence::AttachmentStoreError> {
        self.inner.put(bytes, meta).await
    }

    async fn get(
        &self,
        id: &lash::attachments::AttachmentId,
        max_bytes: u64,
    ) -> Result<lash::persistence::StoredAttachment, lash::persistence::AttachmentStoreError> {
        self.inner.get(id, max_bytes).await
    }

    async fn delete(
        &self,
        id: &lash::attachments::AttachmentId,
    ) -> Result<(), lash::persistence::AttachmentStoreError> {
        Err(lash::persistence::AttachmentStoreError::Backend {
            operation: "delete",
            class: self.class,
            source: format!("scripted workbench delete failure for {id}").into(),
        })
    }

    async fn list(
        &self,
    ) -> Result<Vec<lash::persistence::StoredBlobRef>, lash::persistence::AttachmentStoreError>
    {
        self.inner.list().await
    }

    async fn head(
        &self,
        id: &lash::attachments::AttachmentId,
    ) -> Result<Option<lash::persistence::StoredBlobRef>, lash::persistence::AttachmentStoreError>
    {
        self.inner.head(id).await
    }
}

fn vacuum_only_request(session_id: &SessionId) -> RunStoreMaintenanceRequest {
    RunStoreMaintenanceRequest {
        vacuum_session_ids: vec![session_id.clone()],
        reclaim_attachments: None,
    }
}

fn reclaim_only_request(
    grace_period_ms: u64,
    empty_root_set: EmptyRootSetAuthorization,
) -> RunStoreMaintenanceRequest {
    RunStoreMaintenanceRequest {
        vacuum_session_ids: Vec::new(),
        reclaim_attachments: Some(ReclaimAttachmentsRequest {
            grace_period_ms,
            empty_root_set,
        }),
    }
}

/// Vacuum takes the settled rows and nothing else.
///
/// Two inputs are admitted into the same session's durable ingress lane
/// behind a running turn; one is cancelled, which is a terminal
/// `TurnInputState`, and the other is left pending. One vacuum pass must
/// remove exactly the cancelled row's evidence: the live pending input has to
/// survive, because a host that vacuums a session it still intends to resume
/// would otherwise silently drop work the caller was told had been accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_maintenance_vacuum_reclaims_only_settled_rows() {
    let mut gate = GatedProvider::new();
    let workbench = Workbench::builder(gate.provider.clone()).build().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    // The running turn holds the session, so the next-turn inputs stay
    // pending while the law asserts what the vacuum keeps.
    send_text(state, None, "hold the session")
        .await
        .expect("the holding send is admitted");
    gate.next_call().await;
    let session = state
        .open_session(&session_id, "test")
        .await
        .expect("open the vacuum test session");
    // The holding turn's own input is the run's, not one this law queued.
    let holding = session
        .durable()
        .pending_turn_inputs()
        .await
        .expect("read the holding turn's input")
        .iter()
        .map(|input| input.input.input_id.to_string())
        .collect::<BTreeSet<_>>();

    let Json(retained) = enqueue_turn_input(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnInputRequest {
            text: "this input must survive the vacuum".to_string(),
            ingress: TurnInputIngressRequest::NextTurn,
        }),
    )
    .await
    .expect("admit the input that stays pending");
    let Json(settled) = enqueue_turn_input(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnInputRequest {
            text: "this input settles before the vacuum".to_string(),
            ingress: TurnInputIngressRequest::NextTurn,
        }),
    )
    .await
    .expect("admit the input that is cancelled");

    let cancelled = session
        .durable()
        .cancel_pending_turn_inputs([lash::PendingTurnInputCancelTarget::input_id(
            lash::InputId::fixture(settled.input_id.as_str()),
        )])
        .await
        .expect("cancel the second input");
    assert!(
        matches!(
            cancelled.as_slice(),
            [receipt] if matches!(
                &receipt.outcome,
                lash::PendingTurnInputCancelOutcome::Cancelled(input)
                    if input.input_id == settled.input_id
            )
        ),
        "the second input must reach a terminal state before the vacuum: {cancelled:?}"
    );
    let pending_ids = || async {
        session
            .durable()
            .pending_turn_inputs()
            .await
            .expect("read pending inputs")
            .iter()
            .map(|input| input.input.input_id.to_string())
            .filter(|input_id| !holding.contains(input_id))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        pending_ids().await,
        vec![retained.input_id.to_string()],
        "only the uncancelled input is still pending"
    );

    let Json(swept) =
        run_store_maintenance(State(state.clone()), Json(vacuum_only_request(&session_id)))
            .await
            .expect("run the vacuum lever");
    assert!(
        swept.reclaimed_attachments.is_none(),
        "a vacuum-only request must not sweep the attachment backend"
    );
    assert_eq!(
        swept.vacuumed.len(),
        1,
        "one named session, one vacuum report"
    );
    let report = &swept.vacuumed[0];
    assert_eq!(report.session_id, session_id);
    // Exactly the cancelled input's evidence row is reclaimed.
    assert_eq!(report.removed_pending_turn_input_tombstone_count, 1);
    // No graph node was tombstoned, so vacuum deletes no node rows.
    assert_eq!(report.removed_node_count, 0);
    assert_eq!(
        pending_ids().await,
        vec![retained.input_id.to_string()],
        "the live pending input survives the vacuum untouched"
    );

    let Json(second_pass) =
        run_store_maintenance(State(state.clone()), Json(vacuum_only_request(&session_id)))
            .await
            .expect("run the vacuum lever again");
    assert_eq!(
        second_pass.vacuumed[0].removed_pending_turn_input_tombstone_count, 0,
        "a second pass finds nothing settled left to reclaim"
    );

    let unknown = run_store_maintenance(
        State(state.clone()),
        Json(vacuum_only_request(&SessionId::from(
            "no-such-workbench-session",
        ))),
    )
    .await
    .expect_err("vacuuming an unknown session must not create one");
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);

    let empty = run_store_maintenance(
        State(state.clone()),
        Json(RunStoreMaintenanceRequest {
            vacuum_session_ids: Vec::new(),
            reclaim_attachments: None,
        }),
    )
    .await
    .expect_err("a maintenance pass that names no lever is rejected");
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
}

/// Reclamation is a mark-and-sweep, and the mark phase is the root set.
///
/// One attachment is uploaded and then referenced by a committed turn; a second
/// blob is written straight to the backend and never referenced by anything.
/// With a zero grace period every blob is deletion-eligible on age, so the only
/// thing standing between the referenced blob and deletion is the root set the
/// route hands the sweep.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_maintenance_reclaims_only_unreferenced_attachments() {
    let workbench =
        Workbench::replying("<typescript>\nfinish(\"attachment retained\");\n</typescript>").await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let attachment_store = workbench.stores.attachment_store();

    let Json(uploaded) = upload_attachment(
        State(state.clone()),
        Json(AttachmentUploadRequest {
            name: "referenced.png".to_string(),
            mime: "image/png".to_string(),
            data_base64: STORE_MAINTENANCE_PNG_BASE64.to_string(),
        }),
    )
    .await
    .expect("upload the attachment a turn will reference");
    let referenced_id = uploaded.attachment.id.clone();
    let Json(accepted) = send_turn(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnRequest {
            attachment: Some(uploaded.attachment.clone()),
            ..turn_request("Describe the attached PNG briefly.")
        }),
    )
    .await
    .expect("send the attachment turn");
    let turn_id = started_turn_id(&accepted);
    wait_for_turn_released(state, &session_id, &turn_id).await;

    // A blob nothing ever attached: no committed ref, no intent, no owner.
    let orphan = attachment_store
        .put(
            STORE_MAINTENANCE_ORPHAN_BYTES.to_vec(),
            lash::attachments::AttachmentCreateMeta::new(
                lash::attachments::MediaType::parse("application/octet-stream")
                    .expect("orphan media type"),
                None,
                Some("orphan".to_string()),
            ),
        )
        .await
        .expect("write the unreferenced blob");
    assert_ne!(orphan.id, referenced_id);

    // A retention window wide enough to cover both blobs protects even the
    // unreferenced one: `grace_period_ms` is a post-terminal retention policy,
    // not a hint, so a freshly written orphan is not garbage yet.
    let Json(protected) = run_store_maintenance(
        State(state.clone()),
        Json(reclaim_only_request(
            RECLAIM_RETENTION_WINDOW_MS,
            EmptyRootSetAuthorization::Refuse,
        )),
    )
    .await
    .expect("run the reclamation lever inside the retention window");
    let protected = protected
        .reclaimed_attachments
        .expect("a reclamation pass reports its sweep");
    assert_eq!(protected.scanned_blob_count, 2);
    // The retention window spared the unreferenced blob.
    assert_eq!(protected.reclaimed_count, 0);
    // A pass that completes and reclaims nothing is a *witnessed* nothing-to-do,
    // and the response says so rather than leaving the operator to infer it from
    // a zero that a swallowed failure could also have produced.
    assert_eq!(protected.sweep, SweepOutcome::NothingToDo);
    attachment_store
        .get(
            &orphan.id,
            lash::persistence::AttachmentReadPolicy::STANDARD.max_blob_bytes,
        )
        .await
        .expect("the retention window kept the unreferenced blob");

    let Json(swept) = run_store_maintenance(
        State(state.clone()),
        Json(reclaim_only_request(
            TEST_ONLY_INSTANT_ELIGIBILITY_MS,
            EmptyRootSetAuthorization::Refuse,
        )),
    )
    .await
    .expect("run the attachment reclamation lever");
    assert!(swept.vacuumed.is_empty());
    let summary = swept
        .reclaimed_attachments
        .expect("a reclamation pass reports its sweep");
    // Both blobs in the backend were considered.
    assert_eq!(summary.scanned_blob_count, 2);
    // Exactly the unreferenced blob was reclaimed; the root set spared the other.
    assert_eq!(summary.reclaimed_count, 1);
    // The same pass classifies itself as a sweep, the other arm of the contract.
    assert_eq!(summary.sweep, SweepOutcome::Swept);
    // The SQLite session store factory implements the condemnation CAS, so its
    // deletes are fenced against concurrent writers rather than best-effort.
    assert_eq!(summary.fence, SweepFence::Fenced);
    // An uncontended fenced sweep reports no failures, no deferrals, no late roots.
    assert!(summary.failed_ids.is_empty(), "{summary:?}");
    assert!(summary.condemn_deferred_ids.is_empty(), "{summary:?}");
    assert!(summary.deleted_while_referenced.is_empty(), "{summary:?}");
    // The root authority answered, so this is not a degraded no-candidate pass.
    assert_eq!(summary.root_enumeration_failure, None);
    // The response says which policy produced that count. A reclaimed count is
    // unreadable without the grace period it ran under.
    let echoed = swept
        .reclaim_policy
        .expect("a reclamation pass echoes the policy it ran under");
    assert_eq!(echoed.grace_period_ms, TEST_ONLY_INSTANT_ELIGIBILITY_MS);
    assert_eq!(echoed.empty_root_set, EmptyRootSetAuthorization::Refuse);

    attachment_store
        .get(
            &referenced_id,
            lash::persistence::AttachmentReadPolicy::STANDARD.max_blob_bytes,
        )
        .await
        .expect("the committed turn's attachment survives the sweep");
    match attachment_store
        .get(
            &orphan.id,
            lash::persistence::AttachmentReadPolicy::STANDARD.max_blob_bytes,
        )
        .await
    {
        Err(lash::persistence::AttachmentStoreError::NotFound(id)) => assert_eq!(id, orphan.id),
        other => panic!("the unreferenced blob must be gone, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_maintenance_serves_incomplete_sweep_with_failure_counts() {
    let workbench = Workbench::silent().await;
    let inner = workbench.stores.attachment_store();
    let orphan = inner
        .put(
            b"workbench-incomplete-sweep".to_vec(),
            lash::attachments::AttachmentCreateMeta::new(
                lash::attachments::MediaType::parse("application/octet-stream")
                    .expect("incomplete-sweep media type"),
                None,
                Some("incomplete-sweep-orphan".to_string()),
            ),
        )
        .await
        .expect("seed the deletion candidate");
    let mut state = workbench.state.clone();
    state.attachment_store = Arc::new(DeleteFailingWorkbenchAttachmentStore {
        inner: Arc::clone(&inner),
        class: lash::persistence::AttachmentStoreFailureClass::Transient,
    });

    let Json(response) = run_store_maintenance(
        State(state.clone()),
        Json(reclaim_only_request(
            TEST_ONLY_INSTANT_ELIGIBILITY_MS,
            EmptyRootSetAuthorization::AuthorizeDeleteAll,
        )),
    )
    .await
    .expect("per-item failures complete the route as an incomplete sweep");
    assert_eq!(
        Json(response.clone()).into_response().status(),
        StatusCode::OK,
        "an incomplete sweep is a successful HTTP response"
    );

    let body = serde_json::to_value(response).expect("serialize maintenance response body");
    let summary = &body["reclaimed_attachments"];
    assert_eq!(summary["sweep"], "incomplete");
    assert_eq!(summary["failed_count"], 1);
    assert_eq!(summary["stalled_count"], 0);
    assert_eq!(summary["stalled_ids"], json!([]));
    assert_eq!(summary["condemn_deferred_count"], 0);
    assert_eq!(summary["failed_ids"], json!([orphan.id.to_string()]));
    inner
        .get(
            &orphan.id,
            lash::persistence::AttachmentReadPolicy::STANDARD.max_blob_bytes,
        )
        .await
        .expect("the failed delete leaves the blob intact");
}

/// An empty root set is a refused destruction, not a silent wipe-all.
///
/// This is the case the route exists to get right. A root authority that
/// enumerates zero live refs is overwhelmingly a misconfiguration — an empty
/// catalog, a factory pointed somewhere else — and reading it as "nothing is
/// referenced, so delete everything" turns that misconfiguration into total
/// loss of the deployment's attachment bytes. The default therefore refuses,
/// the bytes stay, and only a second request carrying the explicit
/// authorization deletes them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_maintenance_refuses_an_empty_root_set() {
    let workbench = Workbench::silent().await;
    let state = &workbench.state;
    let attachment_store = workbench.stores.attachment_store();

    // A real session exists — the catalog is present and answerable — it simply
    // references no attachment. The root set is honestly empty, which is
    // exactly the shape a misconfigured factory produces.
    drop(
        state
            .create_or_open_session(&state.current_session_id(), "test")
            .await
            .expect("open the empty-root-set session"),
    );

    // Put an orphan through the bytes port. The upload route creates a live
    // upload holder, so a route upload would not leave the root set empty.
    let orphan = attachment_store
        .put(
            STORE_MAINTENANCE_ORPHAN_BYTES.to_vec(),
            lash::attachments::AttachmentCreateMeta::new(
                lash::attachments::MediaType::parse("application/octet-stream")
                    .expect("orphan media type"),
                None,
                Some("empty-root-set-orphan".to_string()),
            ),
        )
        .await
        .expect("put a blob nothing references");
    let blob_id = orphan.id;

    let refused = run_store_maintenance(
        State(state.clone()),
        Json(reclaim_only_request(
            TEST_ONLY_INSTANT_ELIGIBILITY_MS,
            EmptyRootSetAuthorization::Refuse,
        )),
    )
    .await
    .expect_err("an empty root set with a deletion-eligible blob must be refused");
    // The refusal is reported as a refusal, not as a successful empty sweep.
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert!(
        refused
            .message
            .contains("empty_root_set=authorize_delete_all"),
        "the refusal must name the assertion that would authorize it: {}",
        refused.message
    );
    attachment_store
        .get(
            &blob_id,
            lash::persistence::AttachmentReadPolicy::STANDARD.max_blob_bytes,
        )
        .await
        .expect("the refused sweep deleted nothing");

    let Json(authorized) = run_store_maintenance(
        State(state.clone()),
        Json(reclaim_only_request(
            TEST_ONLY_INSTANT_ELIGIBILITY_MS,
            EmptyRootSetAuthorization::AuthorizeDeleteAll,
        )),
    )
    .await
    .expect("the explicit authorization permits the same sweep");
    let summary = authorized
        .reclaimed_attachments
        .expect("the authorized pass reports its sweep");
    // The blob the refusal spared is deleted once the host asserts it meant to.
    assert_eq!(summary.reclaimed_count, 1);
    match attachment_store
        .get(
            &blob_id,
            lash::persistence::AttachmentReadPolicy::STANDARD.max_blob_bytes,
        )
        .await
    {
        Err(lash::persistence::AttachmentStoreError::NotFound(id)) => assert_eq!(id, blob_id),
        other => panic!("the authorized sweep must delete the blob, got {other:?}"),
    }
}

#[test]
fn store_maintenance_is_absent_from_the_workbench_ui() {
    assert!(
        !ui::INDEX_HTML.contains("/api/admin/store-maintenance"),
        "store maintenance is operator-only: it must never be one click away"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_maintenance_serves_stalled_delete_counts() {
    let workbench = Workbench::silent().await;
    let inner = workbench.stores.attachment_store();
    let orphan = inner
        .put(
            b"workbench-stalled-sweep".to_vec(),
            lash::attachments::AttachmentCreateMeta::new(
                lash::attachments::MediaType::parse("application/octet-stream")
                    .expect("media type"),
                None,
                None,
            ),
        )
        .await
        .expect("seed deletion candidate");
    let mut state = workbench.state.clone();
    state.attachment_store = Arc::new(DeleteFailingWorkbenchAttachmentStore {
        inner: Arc::clone(&inner),
        class: lash::persistence::AttachmentStoreFailureClass::Credentials,
    });
    let Json(response) = run_store_maintenance(
        State(state.clone()),
        Json(reclaim_only_request(
            TEST_ONLY_INSTANT_ELIGIBILITY_MS,
            EmptyRootSetAuthorization::AuthorizeDeleteAll,
        )),
    )
    .await
    .expect("a stalled delete is a completed incomplete sweep");
    assert_eq!(
        Json(response.clone()).into_response().status(),
        StatusCode::OK
    );
    let body = serde_json::to_value(response).expect("serialize response");
    let summary = &body["reclaimed_attachments"];
    assert_eq!(summary["sweep"], "incomplete");
    assert_eq!(summary["failed_count"], 1);
    assert_eq!(summary["stalled_count"], 1);
    assert_eq!(summary["stalled_ids"], json!([orphan.id.to_string()]));
    let listed = state
        .session_store_factory
        .list_condemnations()
        .await
        .expect("stalled listing");
    assert_eq!(
        listed[0].stalled,
        Some(lash::persistence::AttachmentDeleteStallReason::Refused)
    );
    inner
        .get(
            &orphan.id,
            lash::persistence::AttachmentReadPolicy::STANDARD.max_blob_bytes,
        )
        .await
        .expect("stalled bytes remain");
}
