use super::*;
use lash::ProcessId;
use lash::SessionId;
use lash::TurnId;

#[path = "routes/host_streams.rs"]
mod host_streams;
pub(crate) use host_streams::*;

pub(crate) async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "service": "agent-workbench", "status": "ok" }))
}

pub(crate) async fn index() -> Html<&'static str> {
    Html(ui::INDEX_HTML)
}

pub(crate) async fn timeline_script() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        ui::TIMELINE_JS,
    )
}

pub(crate) async fn app_state(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<StateReadSnapshot>, AppError> {
    let session_id = state.admit_session(&query, "api.state").await?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: session_id.clone(),
        })?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::ManageApprovals)?;
    Ok(Json(read_state_snapshot(&state, &session_id).await?))
}

/// The settled state rendered after either a read or a session reset.
async fn read_state_snapshot(
    state: &AppState,
    session_id: &SessionId,
) -> Result<StateReadSnapshot, AppError> {
    let StateProjectionReads {
        read_view,
        durable,
        cursor,
        pending_turn_inputs,
        queued_work,
        turn_input_applications,
        turn_failure_settlements,
    } = read_state_projection(state, session_id).await?;
    // Each cursor is read before the data it covers, so a stream attached at
    // these cursors can only re-deliver what the snapshot already holds; the
    // page upserts by row identity, which makes that redelivery idempotent.
    let transcript = durable
        .transcript()
        .await
        .map_err(AppError::internal)?
        .into_records();
    let product_events = state.event_tx.snapshot(session_id);
    // Read after the product lane: a turn this reports as running has not
    // settled yet, so its `done` is still in (or after) the lane snapshot.
    let active_turn = state.active_turns.for_session(session_id);
    let unknown_turn_terminals = state.unknown_turn_terminals.for_session(session_id);
    let pending_approvals = state.approvals.pending().map_err(AppError::internal)?;
    let observation = RemoteSessionObservation::from_core(lash::observe::SessionObservation {
        read_view,
        cursor: cursor.clone(),
    });
    debug_assert_eq!(observation.cursor, cursor.to_string());
    Ok(StateReadSnapshot {
        transcript,
        state: StateSnapshot {
            settings: state.settings_for_session(session_id.clone()),
            observation,
            product_events,
            // The page reads `active_turns` as a list and asks it for a
            // length and a `turn_id`; the registry holding at most one is a
            // fact about the registry, not a change to that contract.
            active_turns: active_turn
                .into_iter()
                .map(|active_turn| active_turn.address)
                .collect(),
            pending_turn_inputs,
            queued_work,
            turn_input_applications,
            turn_failure_settlements,
            unknown_turn_terminals,
            pending_approvals,
        },
    })
}

pub(crate) const MAX_WORKBENCH_ATTACHMENT_BYTES: usize = 1024 * 1024;

pub(crate) async fn upload_attachment(
    State(state): State<AppState>,
    Json(request): Json<AttachmentUploadRequest>,
) -> Result<Json<AttachmentUploadResponse>, AppError> {
    let name = request.name.trim();
    if name.is_empty() {
        return Err(AppError::bad_request("attachment name is required"));
    }
    if name.chars().count() > 200 {
        return Err(AppError::bad_request(
            "attachment name must be at most 200 characters",
        ));
    }
    let media_type = lash::attachments::MediaType::parse(&request.mime).map_err(|_| {
        AppError::bad_request(
            "the workbench turn contract currently accepts PNG image attachments only",
        )
    })?;
    if media_type.as_str() != "image/png" {
        return Err(AppError::bad_request(
            "the workbench turn contract currently accepts PNG image attachments only",
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(request.data_base64.trim())
        .map_err(|_| AppError::bad_request("attachment data_base64 is not valid base64"))?;
    if bytes.is_empty() {
        return Err(AppError::bad_request("attachment file is empty"));
    }
    if bytes.len() > MAX_WORKBENCH_ATTACHMENT_BYTES {
        return Err(AppError::bad_request(format!(
            "attachment exceeds the {} byte workbench limit",
            MAX_WORKBENCH_ATTACHMENT_BYTES
        )));
    }
    let type_metadata = png_dimensions(&bytes).map(|(width, height)| {
        lash::attachments::AttachmentTypeMetadata::image(Some(width), Some(height))
    });
    let attachment = state
        .attachment_store
        .put(
            bytes,
            lash::attachments::AttachmentCreateMeta::new(
                media_type,
                type_metadata,
                Some(name.to_string()),
            ),
        )
        .await
        // Audited: the content-addressed attachment store has no session identity or tombstone error variant.
        .map_err(AppError::internal)?;
    let retrieve_url = attachment_retrieve_url(&attachment.id.to_string());
    state.trace(
        "api.attachment.uploaded",
        json!({
            "attachment_id": attachment.id,
            "mime": attachment.media_type.as_str(),
            "byte_len": attachment.byte_len,
            "name": name,
        }),
    );
    Ok(Json(AttachmentUploadResponse {
        attachment,
        retrieve_url,
    }))
}

// Retrieval is deliberately not session-gated: reloads and retired-session transcripts must
// still render. The unguessable BLAKE3 content address is the bearer capability, and the URL
// carries no session data. That capability does not expire and blobs outlive sessions; reclaiming
// them belongs to ADR 0024 retention work. If ids are not content addresses, or an id can reach a
// viewer who may not read the blob, this route MUST be protected by an authorization gate.
pub(crate) async fn retrieve_attachment(
    AxumPath(attachment_id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Response, AppError> {
    let attachment_id = attachment_id.trim();
    // The id arrives from the URL path, so it is untrusted: a malformed one is
    // a bad request, never a store lookup.
    let parsed_id = lash::attachments::AttachmentId::parse(attachment_id)
        .map_err(|err| AppError::bad_request(err.to_string()))?;
    let stored = match state
        .attachment_store
        .get(
            &parsed_id,
            lash::persistence::AttachmentReadPolicy::DEFAULT.max_blob_bytes,
        )
        .await
    {
        Ok(stored) => stored,
        Err(lash::persistence::AttachmentStoreError::NotFound(_)) => {
            return Err(AppError::not_found(format!(
                "attachment `{attachment_id}` was not found"
            )));
        }
        // Audited: the content-addressed attachment store has no session identity or tombstone error variant.
        Err(err) => return Err(AppError::internal(err)),
    };
    Response::builder()
        .status(StatusCode::OK)
        // MCP can retain arbitrary binary resources beside PNG uploads.
        .header(
            header::CONTENT_TYPE,
            if png_dimensions(&stored.bytes).is_some() {
                "image/png"
            } else {
                "application/octet-stream"
            },
        )
        .header("x-content-type-options", "nosniff")
        .header(header::CACHE_CONTROL, "private, no-store")
        .header("x-lash-attachment-id", attachment_id)
        .body(Body::from(stored.bytes))
        .map_err(|err| AppError::internal(format!("build attachment response: {err}")))
}

/// Show the user's row, hand the input to the session's `send()`, and follow
/// the run it starts. The claim passes to the follower, which releases it
/// once the run is settled on the page.
pub(crate) async fn commit_and_start_user_turn(
    state: AppState,
    cleanup: ActiveTurnSubmissionGuard,
    request: turns::UserTurnRequest,
    chat_attachments: Vec<ChatAttachment>,
    client_nonce: Option<String>,
) -> Result<tokio::task::JoinHandle<turns::TurnSettlement>, AppError> {
    let active = state
        .active_turns
        .for_session(&request.session_id)
        .filter(|active| active.address.turn_id == request.turn_id)
        .ok_or_else(|| AppError::conflict("the UI input no longer owns its turn claim"))?;
    let mut input = ui_input_message_from_active_turn(&active)
        .ok_or_else(|| AppError::conflict("the UI input claim has no prompt row"))?;
    input.attachments = chat_attachments;
    input.client_nonce = client_nonce;
    state.push_prepared_message_for_session(&request.session_id, input);
    state.trace_for_session(
        &request.session_id,
        "api.turn.admission_committed",
        json!({ "turn_id": request.turn_id }),
    );
    let follower = turns::start_user_turn(&state, request).await?;
    cleanup.complete();
    Ok(follower)
}

pub(crate) async fn send_turn(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    Json(request): Json<TurnRequest>,
) -> Result<Json<TurnAccepted>, AppError> {
    let text = request.text.trim().to_string();
    if text.is_empty() {
        return Err(AppError::bad_request("message text is required"));
    }
    let client_nonce = request.client_nonce.clone();
    if client_nonce.as_deref().is_some_and(|nonce| {
        nonce.is_empty()
            || nonce.len() > 64
            || !nonce
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    }) {
        return Err(AppError::bad_request(
            "client_nonce must be 1-64 ASCII letters, digits or '-'",
        ));
    }
    let attachment_id = request
        .attachment_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    let session_id = state.admit_session(&query, "api.turn").await?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::EnqueueTurn {
            session_id: session_id.clone(),
        })?;
    // Last-active is a fact about use, so it moves when a turn is sent rather
    // than when a poll reads the session; an unnamed session takes its first
    // prompt as its title.
    state.sessions.record_prompt(&session_id, &text);
    let attachment = match attachment_id.as_deref() {
        None => None,
        Some(attachment_id) => match state
            .attachment_store
            .get(
                // Request-body id: untrusted, so a malformed one is a bad
                // request rather than a store lookup.
                &lash::attachments::AttachmentId::parse(attachment_id)
                    .map_err(|err| AppError::bad_request(err.to_string()))?,
                lash::persistence::AttachmentReadPolicy::DEFAULT.max_blob_bytes,
            )
            .await
        {
            Ok(stored) => Some(stored),
            Err(lash::persistence::AttachmentStoreError::NotFound(_)) => {
                return Err(AppError::not_found(format!(
                    "attachment `{attachment_id}` was not found"
                )));
            }
            // Audited: the content-addressed attachment store has no session identity or tombstone error variant.
            Err(err) => return Err(AppError::internal(err)),
        },
    };
    let turn_profile = llm_profile_selection_for_request(
        &state.selected_llm_profile(),
        request.model.as_deref(),
        request.model_variant.as_deref(),
    )?;
    state.trace_for_session(
        &session_id,
        "api.turn.request",
        json!({
            "text": text.clone(),
            "attachment_id": attachment_id,
            "model": serde_json::to_value(&turn_profile).unwrap_or(Value::Null),
        }),
    );
    state.set_selected_llm_profile(turn_profile.clone());
    // A session runs one turn at a time, and the durable authorities say so: the
    // session execution lease and the commit CAS refuse the second writer. So a
    // send that arrives while a turn is running cannot start one, and answering
    // `accepted` while starting a doomed turn is a lie the browser then renders
    // (FIG-1000). Admit it as the next turn's input instead: the message is held
    // durably, every viewer sees a queued receipt, and the session's engine
    // answers it as its own turn once the running one settles.
    //
    // The initial read selects the ordinary busy path without opening a runtime
    // session. The atomic reservation below rechecks after that open, closing
    // the race with another send or a queued-work runner.
    if state.active_turns.for_session(&session_id).is_some() {
        return admit_queued_send(
            &state,
            &session_id,
            text,
            attachment.map(|attachment| attachment.bytes),
        )
        .await;
    }
    drop(
        state
            .create_or_open_session(&session_id, "api.turn")
            .await
            .map_err(|error| state.session_admission_error(&session_id, "api.turn", error))?,
    );
    let turn_id = TurnId::prefixed("workbench-turn-", uuid::Uuid::new_v4());
    let chat_attachments = attachment_id
        .iter()
        .cloned()
        .map(ChatAttachment::from_id)
        .collect();
    let cleanup = ActiveTurnSubmissionGuard::user_turn(&state, &session_id, &turn_id);
    state.trace_for_session(&session_id, "api.turn.claim_ready", json!({}));
    match state.active_turns.try_insert_with_prompt_for_idle_session(
        &session_id,
        &turn_id,
        WorkbenchTurnKind::User,
        Some(text.clone()),
        attachment_id.clone(),
    ) {
        ActiveTurnClaim::Claimed => {}
        ActiveTurnClaim::Busy => {
            cleanup.complete();
            return admit_queued_send(
                &state,
                &session_id,
                text,
                attachment.map(|attachment| attachment.bytes),
            )
            .await;
        }
        // The delete fenced this session after the admission read above; the
        // claim is where that ordering is decided, so refuse here exactly as
        // the admission read would have.
        ActiveTurnClaim::Refused(retirement) => {
            cleanup.complete();
            return Err(state.retirement_fence_refusal(&session_id, "api.turn", retirement));
        }
    }
    // The follower settles the turn on its own; the route answers once the
    // input is accepted.
    drop(
        tokio::spawn(commit_and_start_user_turn(
            state,
            cleanup,
            turns::UserTurnRequest {
                turn_id: turn_id.clone(),
                session_id: session_id.clone(),
                text,
                model: turn_profile.clone(),
                attachment_id,
            },
            chat_attachments,
            client_nonce,
        ))
        .await
        .map_err(|error| AppError::internal(format!("turn admission task failed: {error}")))??,
    );
    Ok(Json(TurnAccepted::started(turn_id)))
}

pub(crate) async fn button_trigger(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    Json(request): Json<ButtonEventRequest>,
) -> Result<Json<CommandAccepted>, AppError> {
    // Side-effect ingress: the fence refuses before any message is pushed or
    // any workflow submitted for a retired session.
    let session_id = state.admit_session(&query, "api.button_trigger").await?;
    let turn_profile = llm_profile_selection_for_request(
        &state.selected_llm_profile(),
        request.model.as_deref(),
        request.model_variant.as_deref(),
    )?;
    let model = turn_profile.clone();
    state.set_selected_llm_profile(model.clone());
    state.trace_for_session(
        &session_id,
        "api.button_trigger.request",
        json!({
            "button": request.button,
            "model": serde_json::to_value(&turn_profile).unwrap_or(Value::Null),
        }),
    );
    // The press ran as an engine workflow that recorded its occurrence
    // (FIG-5036); it waits for L3 (FIG-5172).
    let _ = model;
    Err(AppError::no_engine("a button trigger"))
}

pub(crate) async fn list_accounts(
    State(state): State<AppState>,
) -> Json<Vec<mail::AccountSummary>> {
    Json(state.mail_world.account_summaries())
}

pub(crate) async fn list_triggers(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<Vec<WorkbenchTriggerRegistration>>, AppError> {
    let session_id = state.admit_session(&query, "api.triggers.list").await?;
    let records = state
        .trigger_store
        .list_subscriptions(lash::triggers::TriggerSubscriptionFilter::for_session(
            &session_id,
        ))
        .await
        // Audited: first-party trigger-store reads have no session tombstone path or effect-controller boundary.
        .map_err(AppError::internal)?;
    let mut registrations = Vec::with_capacity(records.len());
    for record in &records {
        let last_fired_at_ms = state
            .trigger_store
            .list_deliveries_by_subscription_id(&record.subscription_id)
            .await
            // Audited: first-party trigger-store reads have no session tombstone path or effect-controller boundary.
            .map_err(AppError::internal)?
            .iter()
            .map(|delivery| delivery.created_at_ms)
            .max();
        registrations.push(WorkbenchTriggerRegistration::new(record, last_fired_at_ms));
    }
    Ok(Json(registrations))
}

pub(crate) async fn set_trigger_enabled(
    AxumPath(subscription_key): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    Json(request): Json<TriggerEnabledRequest>,
) -> Result<Json<TriggerMutationResponse>, AppError> {
    let session_id = state.admit_session(&query, "api.triggers.enable").await?;
    let record = trigger_record_for_session(&state, &session_id, &subscription_key).await?;
    let changed = record.lifecycle.enabled() != request.enabled;
    let command = if request.enabled {
        lash::triggers::TriggerCommand::Enable {
            owner_scope: record.owner_scope.clone(),
            actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(
                &session_id,
            )),
            subscription_key: record.subscription_key.clone(),
            expected_revision: record.revision,
        }
    } else {
        lash::triggers::TriggerCommand::Disable {
            owner_scope: record.owner_scope.clone(),
            actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(
                &session_id,
            )),
            subscription_key: record.subscription_key.clone(),
            expected_revision: record.revision,
        }
    };
    let outcome = state
        .trigger_store
        .execute_command(
            &format!("workbench-trigger-enabled-{}", uuid::Uuid::new_v4()),
            command,
        )
        .await
        // Audited: first-party trigger mutation stores return only local validation/backend PluginError values.
        .map_err(AppError::internal)?
        // Audited: TriggerOperationError carries only conflict, validation, or string-valued store failures.
        .map_err(AppError::internal)?;
    let lash::triggers::TriggerCommandOutcome::Mutation { receipt } = outcome else {
        // Audited: this locally generated error guards an impossible command/outcome shape.
        return Err(AppError::internal(
            "trigger mutation returned a list outcome",
        ));
    };
    state.trace_for_session(
        &session_id,
        "api.triggers.enabled",
        json!({
            "subscription_key": subscription_key,
            "enabled": request.enabled,
            "changed": changed,
        }),
    );
    let registration = lash::triggers::TriggerRegistration::from(&receipt.record);
    Ok(Json(TriggerMutationResponse {
        changed,
        registration: Some(registration),
    }))
}

pub(crate) async fn delete_trigger(
    AxumPath(subscription_key): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<TriggerMutationResponse>, AppError> {
    let session_id = state.admit_session(&query, "api.triggers.delete").await?;
    let record = trigger_record_for_session(&state, &session_id, &subscription_key).await?;
    state
        .trigger_store
        .execute_command(
            &format!("workbench-trigger-delete-{}", uuid::Uuid::new_v4()),
            lash::triggers::TriggerCommand::Delete {
                owner_scope: record.owner_scope.clone(),
                actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(
                    &session_id,
                )),
                subscription_key: record.subscription_key.clone(),
                expected_revision: record.revision,
            },
        )
        .await
        // Audited: first-party trigger mutation stores return only local validation/backend PluginError values.
        .map_err(AppError::internal)?
        // Audited: TriggerOperationError carries only conflict, validation, or string-valued store failures.
        .map_err(AppError::internal)?;
    let changed = true;
    state.trace_for_session(
        &session_id,
        "api.triggers.delete",
        json!({ "subscription_key": subscription_key, "changed": changed }),
    );
    Ok(Json(TriggerMutationResponse {
        changed,
        registration: None,
    }))
}

pub(crate) async fn trigger_record_for_session(
    state: &AppState,
    session_id: &SessionId,
    subscription_key: &str,
) -> Result<lash::triggers::TriggerSubscriptionRecord, AppError> {
    let mut filter = lash::triggers::TriggerSubscriptionFilter::for_session(session_id);
    filter.subscription_key = Some(subscription_key.to_string());
    state
        .trigger_store
        .list_subscriptions(filter)
        .await
        // Audited: first-party trigger-store reads have no session tombstone path or effect-controller boundary.
        .map_err(AppError::internal)?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::not_found(format!("unknown trigger `{subscription_key}`")))
}

pub(crate) async fn add_account(
    State(state): State<AppState>,
    Json(request): Json<AddAccountRequest>,
) -> Result<Json<mail::AccountSummary>, AppError> {
    let summary = state
        .mail_world
        .add_account(&request.name)
        .map_err(AppError::bad_request)?;
    state.trace(
        "api.accounts.add",
        json!({ "slug": summary.slug, "authority": summary.authority }),
    );
    Box::pin(enqueue_tool_catalog_refresh(&state, "account_added")).await?;
    state.push_message(
        "event",
        format!("connected mock account `{}`", summary.authority),
    );
    Ok(Json(summary))
}

pub(crate) async fn delete_account(
    AxumPath(slug): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Json<CommandAccepted>, AppError> {
    state
        .mail_world
        .remove_account(&slug)
        .map_err(AppError::not_found)?;
    state.trace("api.accounts.remove", json!({ "slug": slug }));
    Box::pin(enqueue_tool_catalog_refresh(&state, "account_removed")).await?;
    state.push_message("event", format!("removed mock account `inbox.{slug}`"));
    Ok(Json(CommandAccepted { accepted: true }))
}

pub(crate) async fn delete_message(
    AxumPath((slug, id)): AxumPath<(String, String)>,
    State(state): State<AppState>,
) -> Result<Json<CommandAccepted>, AppError> {
    state
        .mail_world
        .remove_message(&slug, &id)
        .map_err(AppError::not_found)?;
    state.trace(
        "api.accounts.message.delete",
        json!({ "account": slug, "id": id }),
    );
    Ok(Json(CommandAccepted { accepted: true }))
}

pub(crate) async fn account_inbox(
    AxumPath(slug): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Json<Vec<mail::MailMessage>>, AppError> {
    let inbox = state.mail_world.inbox(&slug).map_err(AppError::not_found)?;
    Ok(Json(inbox))
}

/// Enqueue a durable tool-catalog refresh for the chat session.
///
/// The enqueue writes the batch as session work; the session's actor drains
/// it and commits the refreshed surface to the session store. Nothing here
/// executes effects in the foreground.
pub(crate) async fn enqueue_tool_catalog_refresh(
    state: &AppState,
    reason: &str,
) -> Result<lash::SessionCommandReceipt, AppError> {
    let session_id = state.current_session_id();
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .map_err(|error| {
            state.session_admission_error(&session_id, "mail.tool_catalog.refresh", error)
        })?;
    let receipt = Box::pin(session.admin().commands().refresh_tool_catalog(
        reason,
        format!(
            "workbench-refresh-tool-catalog:{}:{}:{}",
            session_id,
            reason,
            uuid::Uuid::new_v4()
        ),
    ))
    .await
    .map_err(AppError::runtime)?;
    drop(session);
    state.trace_for_session(
        &session_id,
        "mail.tool_catalog.refresh_enqueued",
        json!({
            "reason": reason,
            "session_id": session_id,
            "command_batch_id": receipt.batch_id,
            "command_source_key": receipt.source_key,
        }),
    );
    Ok(receipt)
}

pub(crate) async fn inject_message(
    AxumPath(slug): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    Json(request): Json<InjectMessageRequest>,
) -> Result<Json<CommandAccepted>, AppError> {
    // Side-effect ingress: the fence refuses before mail is delivered or any
    // workflow submitted for a retired session.
    let session_id = state.admit_session(&query, "api.accounts.inject").await?;
    let turn_profile = llm_profile_selection_for_request(
        &state.selected_llm_profile(),
        request.model.as_deref(),
        request.model_variant.as_deref(),
    )?;
    let model = turn_profile;
    state.set_selected_llm_profile(model.clone());
    let delivered = state
        .mail_world
        .deliver(&slug, &request.title, &request.text)
        .map_err(AppError::not_found)?;
    let message = delivered.message;
    let delivery = delivered.delivery;
    state.trace_for_session(
        &session_id,
        "api.accounts.inject",
        json!({ "account": slug, "title": message.title }),
    );
    // The delivery ran as an engine workflow that recorded its occurrence
    // (FIG-5036); it waits for L3 (FIG-5172).
    let _ = (model, delivery);
    Err(AppError::no_engine("a mail delivery"))
}
/// Retire `old_session_id` and report the session that replaced it.
///
/// Two things changed here, and both are about what happens when the browser's
/// request is not there to finish the job (FIG-3136).
///
/// The retirement runs on its own task. The durable delete of a busy session
/// takes as long as it takes — twenty seconds for a few hundred processes and
/// a couple of cron jobs — and everything that takes the page off the old
/// session used to be a continuation of that request: a request that went away
/// stopped the sequence between the delete and the rotation, leaving the
/// roster's current on a tombstone that every surface refuses, with nothing
/// printed and no trace written. A task outlives the request that spawned it,
/// so the rotation happens whether or not anyone is still listening.
///
/// And a reset of an id whose delete already settled is answered with that
/// replacement instead of a refusal. The fence refuses a `Retired` id for
/// every use including a second delete, which is correct for the store and was
/// a dead end for the operator: the page's only repair was a reset, and reset
/// was the one thing the fence would not allow.
pub(crate) async fn retire_for_reset(
    state: &AppState,
    old_session_id: &SessionId,
) -> Result<(SessionId, bool), AppError> {
    if state.active_turns.retirement(old_session_id) == Some(SessionRetirement::Retired) {
        return Ok(state.sessions.replace_retired(old_session_id));
    }
    state
        .admit_session_id_for_delete(old_session_id, "api.session.delete")
        .await?;
    let rotation = tokio::spawn({
        let state = state.clone();
        let old_session_id = old_session_id.clone();
        async move {
            let outcome = retire_session(&state, &old_session_id).await;
            let settled_retired =
                state.active_turns.retirement(&old_session_id) == Some(SessionRetirement::Retired);
            match outcome {
                Ok(()) => {}
                // Every exit from a reset either names a replacement or says
                // why there is none. The silent one was the defect.
                Err(error) if !settled_retired => {
                    eprintln!(
                        "agent-workbench reset left session {:?} live: {error}",
                        old_session_id.as_str()
                    );
                    return Err(error);
                }
                Err(error) => eprintln!(
                    "agent-workbench reset is replacing session {:?} whose delete settled as retired despite a failed call: {error}",
                    old_session_id.as_str()
                ),
            }
            state.event_tx.remove(&old_session_id);
            Ok(state.sessions.replace_retired(&old_session_id))
        }
    });
    match rotation.await {
        Ok(replacement) => replacement,
        Err(join_error) => {
            eprintln!(
                "agent-workbench reset rotation task for session {:?} did not finish: {join_error}",
                old_session_id.as_str()
            );
            Err(AppError::internal(format!(
                "the reset of `{old_session_id}` did not complete: {join_error}"
            )))
        }
    }
}

pub(crate) async fn reset_chat(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<StateReadSnapshot>, AppError> {
    let old_session_id = query.resolve(&state)?;
    let (new_session_id, replaced_current) = retire_for_reset(&state, &old_session_id).await?;
    settle_retired_slot(
        &state,
        &old_session_id,
        &new_session_id,
        replaced_current,
        "api.reset",
    )
    .await?;
    Ok(Json(read_state_snapshot(&state, &new_session_id).await?))
}

/// What a reset and a delete both owe once a session is retired: forget what
/// only that id had a reader for, and make sure the session that took its
/// place exists.
pub(crate) async fn settle_retired_slot(
    state: &AppState,
    retired_session_id: &SessionId,
    successor_session_id: &SessionId,
    replaced_current: bool,
    surface: &str,
) -> Result<(), AppError> {
    // The retired id is never served again, so its unknown-terminal disclosures
    // have no reader left; drop them rather than hold them for the process's life.
    state.unknown_turn_terminals.remove(retired_session_id);
    state.trace_for_session(
        retired_session_id,
        surface,
        json!({
            "old_session_id": retired_session_id,
            "new_session_id": successor_session_id,
            "replaced_current": replaced_current,
        }),
    );
    if replaced_current {
        state.messages.lock_recover().clear();
        state.lashlang_execution.clear();
        state.mail_world.clear();
    }
    state
        .create_or_open_session(successor_session_id, surface)
        .await
        .map_err(AppError::session_open)?;
    Ok(())
}

/// How long a terminal process stays on the rail after leaving the live set.
const WORK_RAIL_RETIRED_WINDOW_MS: u64 = 10_000;

/// The runtime-wide work snapshot: every process whose outcome is still open,
/// plus the rows that retired recently. Live rows are eligible regardless of
/// age, so a row leaves the rail when its outcome is recorded, never because
/// time passed (FIG-3155).
async fn runtime_wide_work(
    state: &AppState,
) -> Result<Vec<lash::process::ObservedWorkItem>, AppError> {
    let retired_since_ms = lash::runtime::ClockWallTime::timestamp_ms(&lash::runtime::SystemClock)
        .saturating_sub(WORK_RAIL_RETIRED_WINDOW_MS);
    let mut observed = state
        .process_observer
        .snapshot_all(&lash::process::ProcessListFilter {
            status: lash::process::ProcessStatusFilter::Any,
            retired_since_ms: Some(retired_since_ms),
            ..lash::process::ProcessListFilter::default()
        })
        .await
        // Audited: runtime-wide process observation reads the global registry without a session store.
        .map_err(AppError::internal)?;
    observed.sort_by(|left, right| {
        right
            .process
            .updated_at_ms
            .cmp(&left.process.updated_at_ms)
            .then_with(|| right.process.created_at_ms.cmp(&left.process.created_at_ms))
            .then_with(|| left.process.process_id.cmp(&right.process.process_id))
    });
    Ok(observed)
}

pub(crate) async fn list_work(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<Vec<WorkItem>>, AppError> {
    // Only the explicit form is session-bound: the default query serves the
    // runtime-wide registry snapshot (including work retired by a session
    // delete), so it is not fenced on whatever session happens to be current.
    let session_id = if query.is_explicit() {
        state.admit_session(&query, "api.work.list").await?
    } else {
        query.resolve(&state)?
    };
    let observed = if query.is_explicit() {
        state
            .process_observer
            .snapshot_for_session(session_id.clone())
            .await
            // Audited: process observation reads the global registry, which has no session tombstone contract.
            .map_err(AppError::internal)?
            .items
    } else {
        runtime_wide_work(&state).await?
    };
    let work = observed
        .into_iter()
        .map(work_item_from_observed)
        .collect::<Vec<_>>();
    state.trace_for_session(
        &session_id,
        "api.work.response",
        json!({
            "count": work.len(),
            "items": work.iter().map(trace_work_item).collect::<Vec<_>>(),
        }),
    );
    Ok(Json(work))
}

#[derive(Debug, Serialize)]
pub(crate) struct QueuedWorkBatchAction {
    pub(crate) accepted: bool,
    pub(crate) batch_id: String,
}

pub(crate) async fn list_queued_work(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<Vec<lash::persistence::QueuedWorkBatch>>, AppError> {
    let session_id = state.admit_session(&query, "api.queued_work.list").await?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: session_id.clone(),
        })?;
    // A read-only probe reads the durable records directly. Opening the session
    // to list them claimed the execution lease and raced the running turn for
    // it (FIG-3144); a Durable Session resolves the queue without one.
    let durable = state
        .session_builder(session_id.clone())
        .durable()
        .await
        .map_err(|error| {
            state.session_admission_error(&session_id, "api.queued_work.list", error)
        })?;
    Ok(Json(durable.queued_work().await.map_err(|error| {
        state.session_admission_error(&session_id, "api.queued_work.list", error)
    })?))
}

pub(crate) async fn cancel_queued_work_batch(
    AxumPath(batch_id): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<QueuedWorkBatchAction>, AppError> {
    let session_id = state
        .admit_session(&query, "api.queued_work.cancel")
        .await?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::ManageQueuedWork {
            session_id: session_id.clone(),
        })?;
    // Cancelling a queued batch is a durable queue mutation: a Durable
    // Session issues it beside a running turn, where `open` would claim the
    // execution lease the turn holds (ADR 0119).
    let durable = state
        .session_builder(session_id.clone())
        .durable()
        .await
        .map_err(|error| {
            state.session_admission_error(&session_id, "api.queued_work.cancel", error)
        })?;
    if durable
        .cancel_queued_work_batch(&lash::BatchId::parse(batch_id.as_str())?)
        .await
        .map_err(|error| {
            state.session_admission_error(&session_id, "api.queued_work.cancel", error)
        })?
        .is_none()
    {
        return Err(AppError::conflict(format!(
            "queued-work batch `{batch_id}` was already claimed, completed, or cancelled"
        )));
    }
    state.trace_for_session(
        &session_id,
        "api.queued_work.cancelled",
        json!({ "batch_id": batch_id }),
    );
    Ok(Json(QueuedWorkBatchAction {
        accepted: true,
        batch_id,
    }))
}

pub(crate) async fn cancel_work(
    AxumPath(process_id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Json<ProcessCancelAccepted>, AppError> {
    let process_id = ProcessId::parse(&process_id)
        .map_err(|_| AppError::not_found(format!("unknown process `{process_id}`")))?;
    let process = state
        .process_observer
        .process(&process_id)
        .await
        // Audited: process lookup reads the global registry, which has no session tombstone contract.
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::not_found(format!("unknown process `{process_id}`")))?;
    if process.terminal() {
        return Err(AppError::conflict(format!(
            "process `{process_id}` is already terminal"
        )));
    }
    let session_id = match &process.originator {
        lash::process::ProcessOriginator::Session { session_id, .. } => session_id.clone(),
        lash::process::ProcessOriginator::Host { .. } => state.current_session_id(),
    };
    // Process cancellation ran as an engine workflow; it waits for L3
    // (FIG-5172).
    let _ = session_id;
    Err(AppError::no_engine("a process cancel"))
}

/// Wait for one durable work item to reach a terminal state, then return its
/// outcome and the authoritative event log.
///
/// This is the host-facing "wait for the work item" flow. It routes through
/// the configured process-work port
/// (ADR 0016) — the process's terminal, never a host poll loop — and bounds
/// the wait with `tokio::time::timeout` so a still-running or unknown-to-this-pod
/// process cannot pin the request. On terminal it reconciles from paged events
/// (ADR 0017): the durable log is the truth; the best-effort event sink is only
/// freshness and may have dropped events.
pub(crate) async fn await_work(
    AxumPath(process_id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Json<WorkAwaitResult>, AppError> {
    const AWAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
    let process_id = ProcessId::parse(&process_id)
        .map_err(|_| AppError::not_found(format!("unknown process `{process_id}`")))?;
    let outcome = match tokio::time::timeout(
        AWAIT_TIMEOUT,
        state.core.processes().await_output(&process_id),
    )
    .await
    {
        Ok(Ok(outcome)) => outcome,
        // Audited: the process terminal wait lowers store failures to untyped PluginError values.
        Ok(Err(err)) => return Err(AppError::internal(err)),
        Err(_elapsed) => {
            return Err(AppError::gateway_timeout(format!(
                "timed out waiting for process `{process_id}` to terminate"
            )));
        }
    };
    let mut events = Vec::new();
    let mut from = lash::process::ProcessEventsFrom::Start(process_id.clone());
    loop {
        let read = state
            .core
            .processes()
            .events(
                from,
                std::num::NonZeroUsize::new(256).unwrap_or(std::num::NonZeroUsize::MIN),
                lash::process::ProcessEventQueryMode::Lite,
            )
            .await
            .map_err(AppError::internal)?;
        let page = match read.outcome {
            lash::process::ProcessEventReadOutcome::Retained(page) => page,
            lash::process::ProcessEventReadOutcome::NoLongerRetained(retention) => {
                return Err(AppError::internal(format!(
                    "process event history is no longer retained: {retention:?}"
                )));
            }
        };
        let lash::process::ProcessEventPageEvents::Lite(page_events) = page.events else {
            unreachable!("lite process event query returned a full page");
        };
        events.extend(page_events.into_iter().map(|event| WorkAwaitEvent {
            sequence: event.sequence,
            event_type: event.event_type,
        }));
        from = match (page.more, read.cursor) {
            (lash::process::ProcessEventPageMore::More { .. }, Some(cursor)) => {
                lash::process::ProcessEventsFrom::After(cursor)
            }
            _ => break,
        };
    }
    state.trace(
        "api.work.await",
        json!({
            "process_id": process_id,
            "terminal_state": format!("{:?}", outcome.terminal_status()),
            "event_count": events.len(),
        }),
    );
    Ok(Json(WorkAwaitResult {
        process_id: process_id.clone(),
        outcome,
        events,
    }))
}

pub(crate) async fn list_lashlang_graphs(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<execution_graphs::LashlangGraphIndex>, AppError> {
    let session_id = state.admit_session(&query, "api.lashlang_graphs").await?;
    let index = execution_graphs::index_for_session(
        &state.process_observer,
        &session_id,
        state.lashlang_execution.graphs(),
    )
    .await?;
    Ok(Json(index))
}

pub(crate) async fn lashlang_graph(
    AxumPath(graph_key): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<TraceLashlangGraph>, AppError> {
    let session_id = state.admit_session(&query, "api.lashlang_graph").await?;
    let graph = execution_graphs::visible_graph_by_key(
        &state.process_observer,
        &session_id,
        state.lashlang_execution.graphs(),
        &graph_key,
    )
    .await?;
    Ok(Json(graph))
}

#[derive(Default)]
pub(crate) struct TurnStreamState {
    pub(crate) assistant_prose: Vec<TurnStreamProseChunk>,
    pub(crate) model_attempt_reset_count: usize,
}

pub(crate) struct TurnStreamProseChunk {
    pub(crate) correlation_id: lash::TurnActivityId,
    pub(crate) text: String,
}

impl TurnStreamState {
    pub(crate) fn apply(&mut self, activity: &TurnActivity) {
        match &activity.event {
            TurnEvent::AssistantProseDelta { text, .. } => {
                self.assistant_prose.push(TurnStreamProseChunk {
                    correlation_id: activity.correlation_id.clone(),
                    text: text.to_string(),
                });
            }
            TurnEvent::ModelAttemptReset {
                assistant_prose_correlation_ids,
                ..
            } => {
                self.model_attempt_reset_count += 1;
                self.assistant_prose.retain(|chunk| {
                    !assistant_prose_correlation_ids.contains(&chunk.correlation_id)
                });
            }
            _ => {}
        }
    }

    pub(crate) fn assistant_prose(&self) -> String {
        self.assistant_prose
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn model_attempt_reset_count(&self) -> usize {
        self.model_attempt_reset_count
    }

    pub(crate) fn settle_terminal(&mut self) {
        self.assistant_prose.clear();
    }
}

pub(crate) struct ChannelTurnEvents {
    pub(crate) turn_state: Arc<Mutex<TurnStreamState>>,
}

#[async_trait]
impl TurnActivitySink for ChannelTurnEvents {
    async fn emit(&self, activity: TurnActivity) {
        let mut turn_state = self.turn_state.lock_recover();
        turn_state.apply(&activity);
    }
}

#[cfg(test)]
pub(crate) fn fold_turn_activities<'a>(
    activities: impl IntoIterator<Item = &'a TurnActivity>,
) -> TurnStreamState {
    let mut state = TurnStreamState::default();
    for activity in activities {
        state.apply(activity);
    }
    state
}

pub(crate) fn workbench_lashlang_abilities() -> lash::rlm::lang::LashlangAbilities {
    lash::rlm::lang::LashlangAbilities::default().with_sleep()
}

#[cfg(test)]
mod turn_stream_state_tests {
    use super::*;

    #[tokio::test]
    async fn workbench_turn_stream_state_retracts_only_superseded_prose() {
        let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
        let sink = ChannelTurnEvents {
            turn_state: Arc::clone(&turn_state),
        };
        sink.emit(TurnActivity::new(
            lash::TurnActivityId::new("prior"),
            TurnEvent::AssistantProseDelta {
                text: "kept ".into(),
                block: lash::direct::StreamBlockIdentity::new("text:0", 0),
            },
        ))
        .await;
        sink.emit(TurnActivity::new(
            lash::TurnActivityId::new("failed"),
            TurnEvent::AssistantProseDelta {
                text: "discarded ".into(),
                block: lash::direct::StreamBlockIdentity::new("text:0", 0),
            },
        ))
        .await;
        sink.emit(TurnActivity::independent(TurnEvent::ModelAttemptReset {
            assistant_prose_correlation_ids: vec![lash::TurnActivityId::new("failed")],
            reasoning_correlation_ids: Vec::new(),
        }))
        .await;
        sink.emit(TurnActivity::new(
            lash::TurnActivityId::new("successful"),
            TurnEvent::AssistantProseDelta {
                text: "answer".into(),
                block: lash::direct::StreamBlockIdentity::new("text:0", 0),
            },
        ))
        .await;

        assert_eq!(turn_state.lock_recover().assistant_prose(), "kept answer");
    }

    #[tokio::test]
    async fn workbench_terminal_settlement_clears_provisional_stream_state() {
        let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
        let sink = ChannelTurnEvents {
            turn_state: Arc::clone(&turn_state),
        };
        sink.emit(TurnActivity::new(
            lash::TurnActivityId::new("cancelled-attempt"),
            TurnEvent::AssistantProseDelta {
                text: "provisional text".into(),
                block: lash::direct::StreamBlockIdentity::new("text:0", 0),
            },
        ))
        .await;
        {
            let mut projection = turn_state.lock_recover();
            assert_eq!(projection.assistant_prose(), "provisional text");
            projection.settle_terminal();
            assert!(projection.assistant_prose().is_empty());
        }
    }

    #[tokio::test]
    async fn empty_reset_throttle_storm_is_boundary_evidence_not_retract_all() {
        let mut activities = vec![TurnActivity::new(
            lash::TurnActivityId::new("visible-before-boundaries"),
            TurnEvent::AssistantProseDelta {
                text: "must remain visible".into(),
                block: lash::direct::StreamBlockIdentity::new("text:0", 0),
            },
        )];
        activities.extend((0..11).map(|_| {
            TurnActivity::independent(TurnEvent::ModelAttemptReset {
                assistant_prose_correlation_ids: Vec::new(),
                reasoning_correlation_ids: Vec::new(),
            })
        }));

        let live_state = Arc::new(Mutex::new(TurnStreamState::default()));
        let live_sink = ChannelTurnEvents {
            turn_state: Arc::clone(&live_state),
        };
        for activity in activities.iter().cloned() {
            live_sink.emit(activity).await;
        }
        {
            let projection = live_state.lock_recover();
            assert_eq!(projection.assistant_prose(), "must remain visible");
            assert_eq!(projection.model_attempt_reset_count(), 11);
        }

        let replay_projection = fold_turn_activities(&activities);
        assert_eq!(replay_projection.assistant_prose(), "must remain visible");
        assert_eq!(replay_projection.model_attempt_reset_count(), 11);
    }
}
