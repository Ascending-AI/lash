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

pub(crate) async fn app_state(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<StateReadSnapshot>, AppError> {
    let session_id = SessionId::from(state.admit_session(&query, "api.state").await?);
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: session_id.clone(),
        })?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::ManageApprovals)?;
    let active_turns = state.active_turns.for_session(&session_id);
    let StateProjectionReads {
        read_view,
        cursor,
        pending_turn_inputs,
        queued_work,
        turn_input_applications,
        usage,
    } = read_state_projection(&state, &session_id, !active_turns.is_empty()).await?;
    // The badge reads the dialect this session recorded, from the same read
    // view the transcript labels its cells from (FIG-1306).
    // Strict (FIG-1979): a recorded bag that does not decode is an error the
    // operator sees, never a default dialect quietly labelling the transcript.
    let recorded_dialect = lash::rlm::rlm_session_dialect(read_view.protocol_turn_options())
        .map_err(AppError::internal)?;
    let active_turn_ids = active_turns
        .iter()
        .map(|address| address.turn_id.clone())
        .collect::<BTreeSet<_>>();
    let committed_message_ids = read_view
        .messages()
        .iter()
        .map(|message| message.id.clone())
        .collect::<BTreeSet<_>>();
    let current_frame_input_turn_ids = read_view
        .messages()
        .iter()
        .filter_map(|message| match message.origin.as_ref() {
            Some(lash::messages::MessageOrigin::TurnInput { turn_id, .. }) => Some(turn_id.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut pending_message_nodes = read_view.message_tree();
    let mut committed_input_turn_ids = BTreeSet::new();
    while let Some(node) = pending_message_nodes.pop() {
        if let Some(lash::messages::MessageOrigin::TurnInput { turn_id, .. }) =
            node.message.origin.as_ref()
        {
            committed_input_turn_ids.insert(turn_id.clone());
        }
        pending_message_nodes.extend(node.children);
    }
    state.event_tx.reconcile_settled(
        &session_id,
        &committed_message_ids,
        &committed_input_turn_ids,
        &active_turn_ids,
    );
    let product_events = state.event_tx.snapshot(&session_id);
    let product_messages = product_events
        .events
        .iter()
        .filter_map(|event| match &event.item {
            StreamItem::Message { message } => Some(message.clone()),
            StreamItem::TurnInput { .. }
            | StreamItem::ModelCallRecorded { .. }
            | StreamItem::Done { .. } => None,
        })
        .collect::<Vec<_>>();
    let ChatProjection {
        messages,
        transcript,
    } = project_chat(
        &state,
        &read_view,
        recorded_dialect,
        &active_turns,
        &committed_input_turn_ids,
        &current_frame_input_turn_ids,
        product_messages,
    );
    let pending_approvals = state.approvals.pending().map_err(AppError::internal)?;
    let turn_failure_settlements = read_view.turn_failure_settlements().to_vec();
    let observation = RemoteSessionObservation::from_core(lash::observe::SessionObservation {
        read_view,
        cursor: cursor.clone(),
    });
    debug_assert_eq!(observation.cursor, cursor.to_string());
    Ok(Json(StateReadSnapshot {
        transcript,
        state: StateSnapshot {
            settings: state.settings_for_session(session_id.clone(), recorded_dialect),
            messages,
            observation,
            product_events,
            active_turns,
            pending_turn_inputs,
            queued_work,
            turn_input_applications,
            turn_failure_settlements,
            usage,
            pending_approvals,
        },
    }))
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
    let stored = match state.attachment_store.get(&parsed_id).await {
        Ok(stored) => stored,
        Err(lash::persistence::AttachmentStoreError::NotFound(_)) => {
            return Err(AppError::not_found(format!(
                "attachment `{attachment_id}` was not found"
            )));
        }
        // Audited: the content-addressed attachment store has no session identity or tombstone error variant.
        Err(err) => return Err(AppError::internal(err)),
    };
    Ok(Response::builder()
        .status(StatusCode::OK)
        // StoredAttachment is bytes-only by design; image/png is host knowledge from this
        // PNG-only upload contract, not metadata supplied by the blob store.
        .header(header::CONTENT_TYPE, "image/png")
        .header("x-content-type-options", "nosniff")
        .header(header::CACHE_CONTROL, "private, no-store")
        .header("x-lash-attachment-id", attachment_id)
        .body(Body::from(stored.bytes))
        .expect("valid attachment response"))
}

pub(crate) async fn commit_and_submit_user_turn(
    state: AppState,
    cleanup: ActiveTurnSubmissionGuard,
    request: restate::WorkbenchTurnWorkflowRequest,
    chat_attachments: Vec<ChatAttachment>,
) -> Result<lash_restate::RestateInvocationId, AppError> {
    state.push_message_with_id_and_attachments_for_session(
        &request.session_id,
        workbench_turn_user_message_id(&request.turn_id),
        "user",
        request.text.clone(),
        chat_attachments,
    );
    state.trace_for_session(
        &request.session_id,
        "api.turn.admission_committed",
        json!({ "turn_id": request.turn_id }),
    );
    let invocation_id = restate::submit_user_turn(&state, request).await?;
    cleanup.complete();
    Ok(invocation_id)
}

pub(crate) async fn submit_tracked_queued_turn(
    cleanup: ActiveTurnSubmissionGuard,
    restate_http: reqwest::Client,
    restate_ingress_url: String,
    request: restate::WorkbenchQueuedTurnWorkflowRequest,
) -> Result<lash_restate::RestateInvocationId, AppError> {
    let invocation_id =
        restate::submit_queued_turn_request(&restate_http, &restate_ingress_url, &request).await?;
    cleanup.complete();
    Ok(invocation_id)
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
    let attachment_id = request
        .attachment_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    let session_id = SessionId::from(state.admit_session(&query, "api.turn").await?);
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::EnqueueTurn {
            session_id: session_id.clone(),
        })?;
    // Last-active is a fact about use, so it moves when a turn is sent rather
    // than when a poll reads the session.
    state.sessions.touch(&session_id);
    let attachment = match attachment_id.as_deref() {
        None => None,
        Some(attachment_id) => match state
            .attachment_store
            .get(
                // Request-body id: untrusted, so a malformed one is a bad
                // request rather than a store lookup.
                &lash::attachments::AttachmentId::parse(attachment_id)
                    .map_err(|err| AppError::bad_request(err.to_string()))?,
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
    let turn_model = model_spec_for_request(
        &state.selected_model(),
        request.model.as_deref(),
        request.model_variant.as_deref(),
    )?;
    state.trace_for_session(
        &session_id,
        "api.turn.request",
        json!({
            "text": text.clone(),
            "attachment_id": attachment_id,
            "model": serde_json::to_value(&turn_model).unwrap_or(Value::Null),
        }),
    );
    state.set_selected_model(ModelSelection::from_spec(&turn_model));
    // A session runs one turn at a time, and the durable authorities say so: the
    // session execution lease and the commit CAS refuse the second writer. So a
    // send that arrives while a turn is running cannot start one, and answering
    // `accepted` while starting a doomed turn is a lie the browser then renders
    // (FIG-1000). Admit it as the next turn's input instead: the message is held
    // durably, every viewer sees a queued receipt, and the queued-work drain
    // that runs at terminalization answers it as its own turn.
    //
    // The initial read selects the ordinary busy path without opening a runtime
    // session. The atomic reservation below rechecks after that open, closing
    // the race with another send or a queued-work runner.
    if !state.active_turns.for_session(&session_id).is_empty() {
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
            .open_session(&session_id)
            .await
            .map_err(|error| state.session_admission_error(&session_id, "api.turn", error))?,
    );
    let turn_id = TurnId::from(format!("workbench-turn-{}", uuid::Uuid::new_v4()));
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
    tokio::spawn(commit_and_submit_user_turn(
        state,
        cleanup,
        restate::WorkbenchTurnWorkflowRequest {
            turn_id,
            session_id: session_id.clone(),
            text,
            model: ModelSelection::from_spec(&turn_model),
            attachment_id,
        },
        chat_attachments,
    ))
    .await
    .map_err(|error| AppError::internal(format!("turn admission task failed: {error}")))??;
    Ok(Json(TurnAccepted::started()))
}

pub(crate) async fn button_trigger(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    Json(request): Json<ButtonEventRequest>,
) -> Result<Json<CommandAccepted>, AppError> {
    // Side-effect ingress: the fence refuses before any message is pushed or
    // any workflow submitted for a retired session.
    let session_id = SessionId::from(state.admit_session(&query, "api.button_trigger").await?);
    let turn_model = model_spec_for_request(
        &state.selected_model(),
        request.model.as_deref(),
        request.model_variant.as_deref(),
    )?;
    let model = ModelSelection::from_spec(&turn_model);
    state.set_selected_model(model.clone());
    state.trace_for_session(
        &session_id,
        "api.button_trigger.request",
        json!({
            "button": request.button,
            "model": serde_json::to_value(&turn_model).unwrap_or(Value::Null),
        }),
    );
    let pressed_at = Utc::now().to_rfc3339();
    state.push_message_for_session(
        &session_id,
        "event",
        format!("{} button trigger occurrence", request.button.lower()),
    );
    restate::submit_button_trigger(
        &state,
        restate::WorkbenchButtonTriggerWorkflowRequest {
            operation_id: format!("workbench-button-{}", uuid::Uuid::new_v4()),
            session_id: session_id.clone(),
            button: request.button,
            model,
            pressed_at,
        },
    )
    .await?;
    Ok(Json(CommandAccepted { accepted: true }))
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
    let session_id = SessionId::from(state.admit_session(&query, "api.triggers.list").await?);
    let records = state
        .trigger_store
        .list_subscriptions(lash::triggers::TriggerSubscriptionFilter::for_session(
            &session_id,
        ))
        .await
        // Audited: first-party trigger-store reads have no session tombstone path or effect-controller boundary.
        .map_err(AppError::internal)?;
    Ok(Json(
        records
            .iter()
            .map(WorkbenchTriggerRegistration::from)
            .collect(),
    ))
}

pub(crate) async fn set_trigger_enabled(
    AxumPath(subscription_key): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    Json(request): Json<TriggerEnabledRequest>,
) -> Result<Json<TriggerMutationResponse>, AppError> {
    let session_id = SessionId::from(state.admit_session(&query, "api.triggers.enable").await?);
    let record = trigger_record_for_session(&state, &session_id, &subscription_key).await?;
    let changed = record.enabled != request.enabled;
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
    let registration = lash::triggers::TriggerRegistration::from(&receipt.record_snapshot);
    // Do not gate this sync on `changed`: redundant mutations reconcile stale Restate state.
    // A sync failure leaves the mutation durable and Restate stale; the next sync reconciles.
    restate::sync_cron_jobs_after_trigger_mutation(
        &state,
        &session_id,
        if request.enabled {
            "trigger_enabled"
        } else {
            "trigger_disabled"
        },
        &receipt.record_snapshot,
    )
    .await?;
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
    let session_id = SessionId::from(state.admit_session(&query, "api.triggers.delete").await?);
    let record = trigger_record_for_session(&state, &session_id, &subscription_key).await?;
    restate::cancel_cron_job_before_trigger_delete(&state, &session_id, &record).await?;
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
    enqueue_tool_catalog_refresh(&state, "account_added").await?;
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
    enqueue_tool_catalog_refresh(&state, "account_removed").await?;
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
/// The enqueue asks the host-owned queued-work driver to submit a Restate
/// workflow for the batch; that workflow drains it with a durable handler
/// context and the runtime commits the refreshed surface to the SQLite session store.
/// Nothing here executes effects in the foreground — the workbench runs
/// Restate + SQLite only.
pub(crate) async fn enqueue_tool_catalog_refresh(
    state: &AppState,
    reason: &str,
) -> Result<lash::SessionCommandReceipt, AppError> {
    let session_id = SessionId::from(state.current_session_id());
    let session = state.open_session(&session_id).await.map_err(|error| {
        state.session_admission_error(&session_id, "mail.tool_catalog.refresh", error)
    })?;
    let receipt = session
        .admin()
        .commands()
        .refresh_tool_catalog(
            reason,
            format!(
                "workbench-refresh-tool-catalog:{}:{}:{}",
                session_id,
                reason,
                uuid::Uuid::new_v4()
            ),
        )
        .await
        .map_err(AppError::runtime)?;
    session.close().await.map_err(AppError::session_open)?;
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
    let session_id = SessionId::from(state.admit_session(&query, "api.accounts.inject").await?);
    let turn_model = model_spec_for_request(
        &state.selected_model(),
        request.model.as_deref(),
        request.model_variant.as_deref(),
    )?;
    let model = ModelSelection::from_spec(&turn_model);
    state.set_selected_model(model.clone());
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
    state.push_message_for_session(
        &session_id,
        "event",
        format!("message delivered to `inbox.{}`: {}", slug, message.title),
    );
    restate::submit_mail_received(
        &state,
        restate::WorkbenchMailReceivedWorkflowRequest {
            operation_id: format!("workbench-mail-{}", uuid::Uuid::new_v4()),
            session_id: session_id.clone(),
            model,
            delivery,
        },
    )
    .await?;
    Ok(Json(CommandAccepted { accepted: true }))
}
pub(crate) async fn reset_chat(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<StateSnapshot>, AppError> {
    let old_session_id = SessionId::from(
        state
            .admit_session_for_delete(&query, "api.session.delete")
            .await?,
    );
    retire_session(&state, &old_session_id).await?;
    state.event_tx.remove(&old_session_id);
    let retired_dialect = state.requested_dialect(&old_session_id);
    let (new_session_id, replaced_current) =
        state.sessions.replace(&old_session_id, retired_dialect);
    let new_session_id = SessionId::from(new_session_id);
    state.trace_for_session(
        &old_session_id,
        "api.reset",
        json!({
            "old_session_id": old_session_id,
            "new_session_id": new_session_id.clone(),
            "replaced_current": replaced_current,
        }),
    );
    let session = state
        .open_session(&new_session_id)
        .await
        .map_err(AppError::session_open)?;
    let selected_model = model_spec_from_selection(state.selected_model());
    session
        .admin()
        .config()
        .update(lash::SessionConfigPatch {
            model: Some(selected_model),
            ..lash::SessionConfigPatch::default()
        })
        .await
        // The setter returns only after the model override is durable; queue
        // rejection or settlement failure remains an internal control error.
        .map_err(AppError::internal)?;
    if replaced_current {
        state.messages.lock_recover().clear();
        state.lashlang_execution.clear();
        state.mail_world.clear();
    }
    // The rotated session has committed nothing yet, so it has recorded no
    // dialect: the badge shows the dialect it will be opened with, which the
    // rotation carried over from the session it replaced.
    let rotated_dialect = state.requested_dialect(&new_session_id);
    Ok(Json(StateSnapshot {
        settings: state.settings_for_session(new_session_id.clone(), rotated_dialect),
        messages: Vec::new(),
        observation: session.observe().current_remote_observation(),
        product_events: ProductEventSnapshot::default(),
        active_turns: Vec::new(),
        pending_turn_inputs: Vec::new(),
        queued_work: Vec::new(),
        turn_input_applications: Vec::new(),
        turn_failure_settlements: Vec::new(),
        usage: session.usage_report(),
        pending_approvals: state.approvals.pending().map_err(AppError::internal)?,
    }))
}

pub(crate) async fn list_work(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<Vec<WorkItem>>, AppError> {
    // Only the explicit form is session-bound: the default query serves the
    // runtime-wide registry snapshot (including work retired by a session
    // delete), so it is not fenced on whatever session happens to be current.
    let session_id = SessionId::from(if query.is_explicit() {
        state.admit_session(&query, "api.work.list").await?
    } else {
        query.resolve(&state)?
    });
    let observed = if query.is_explicit() {
        state
            .process_observer
            .snapshot_for_session(session_id.clone())
            .await
            // Audited: process observation reads the global registry, which has no session tombstone contract.
            .map_err(AppError::internal)?
            .items
    } else {
        let retired_since_ms =
            lash::runtime::ClockWallTime::timestamp_ms(&lash::runtime::SystemClock)
                .saturating_sub(10_000);
        state
            .process_observer
            .snapshot_all(&lash::process::ProcessListFilter {
                status: lash::process::ProcessStatusFilter::Any,
                retired_since_ms: Some(retired_since_ms),
                ..lash::process::ProcessListFilter::default()
            })
            .await
            // Audited: runtime-wide process observation reads the global registry without a session store.
            .map_err(AppError::internal)?
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
    let session_id = SessionId::from(state.admit_session(&query, "api.queued_work.list").await?);
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: session_id.clone(),
        })?;
    let session = state
        .open_session_for_observation(&session_id)
        .await
        .map_err(|error| {
            state.session_admission_error(&session_id, "api.queued_work.list", error)
        })?;
    Ok(Json(
        session.queued_work().await.map_err(AppError::internal)?,
    ))
}

pub(crate) async fn run_queued_work_batch(
    AxumPath(batch_id): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<QueuedWorkBatchAction>, AppError> {
    let session_id = SessionId::from(state.admit_session(&query, "api.queued_work.run").await?);
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::ManageQueuedWork {
            session_id: session_id.clone(),
        })?;
    if !state.active_turns.for_session(&session_id).is_empty() {
        return Err(AppError::conflict(
            "queued work cannot be run while this session has an active turn",
        ));
    }
    let session = state.open_session(&session_id).await.map_err(|error| {
        state.session_admission_error(&session_id, "api.queued_work.run", error)
    })?;
    if !session
        .queued_work()
        .await
        .map_err(AppError::internal)?
        .iter()
        .any(|batch| batch.batch_id == batch_id)
    {
        return Err(AppError::not_found(format!(
            "queued-work batch `{batch_id}` is not pending"
        )));
    }

    let turn_id = TurnId::from(format!("workbench-queued-{}", uuid::Uuid::new_v4()));
    let request = restate::WorkbenchQueuedTurnWorkflowRequest {
        turn_id: turn_id.clone(),
        session_id: session_id.clone(),
        reason: "workbench_manual_batch_run".to_string(),
        batch_ids: vec![batch_id.clone()],
        drain_id: Some(format!("workbench-queued-batch:{batch_id}")),
    };
    let cleanup =
        ActiveTurnSubmissionGuard::queued_turn(state.active_turns.clone(), &session_id, &turn_id);
    match state
        .active_turns
        .try_insert_for_idle_session(&session_id, &turn_id)
    {
        ActiveTurnClaim::Claimed => {}
        ActiveTurnClaim::Busy => {
            cleanup.complete();
            return Err(AppError::conflict(
                "queued work cannot be run while this session has an active turn",
            ));
        }
        ActiveTurnClaim::Refused(retirement) => {
            cleanup.complete();
            return Err(state.retirement_fence_refusal(
                &session_id,
                "api.queued_work.run",
                retirement,
            ));
        }
    }
    tokio::spawn(submit_tracked_queued_turn(
        cleanup,
        state.restate_http.clone(),
        state.restate_ingress_url.clone(),
        request,
    ))
    .await
    .map_err(|error| {
        AppError::internal(format!("queued-turn submission task failed: {error}"))
    })??;
    state.trace_for_session(
        &session_id,
        "api.queued_work.run_submitted",
        json!({ "batch_id": batch_id, "turn_id": turn_id }),
    );
    Ok(Json(QueuedWorkBatchAction {
        accepted: true,
        batch_id,
    }))
}

pub(crate) async fn cancel_queued_work_batch(
    AxumPath(batch_id): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<QueuedWorkBatchAction>, AppError> {
    let session_id = SessionId::from(
        state
            .admit_session(&query, "api.queued_work.cancel")
            .await?,
    );
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::ManageQueuedWork {
            session_id: session_id.clone(),
        })?;
    let session = state.open_session(&session_id).await.map_err(|error| {
        state.session_admission_error(&session_id, "api.queued_work.cancel", error)
    })?;
    if session
        .cancel_queued_work_batch(&batch_id)
        .await
        .map_err(AppError::internal)?
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
    let process_id = ProcessId::from(process_id);
    let process = state
        .process_observer
        .process(&process_id)
        .await
        // Audited: process lookup reads the global registry, which has no session tombstone contract.
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::not_found(format!("unknown process `{process_id}`")))?;
    if process.terminal {
        return Err(AppError::conflict(format!(
            "process `{process_id}` is already terminal"
        )));
    }
    let session_id = match &process.originator {
        lash::process::ProcessOriginator::Session { session_id, .. } => session_id.clone(),
        lash::process::ProcessOriginator::Host { .. } => {
            SessionId::from(state.current_session_id())
        }
    };
    let operation_id = format!("workbench-process-cancel-{}", uuid::Uuid::new_v4());
    restate::submit_process_cancel(
        &state,
        restate::WorkbenchProcessCancelWorkflowRequest {
            operation_id: operation_id.clone(),
            session_id: session_id.clone(),
            process_id: process_id.clone(),
        },
    )
    .await?;
    state.trace_for_session(
        &session_id,
        "api.work.cancel_submitted",
        json!({
            "operation_id": operation_id,
            "process_id": process_id,
        }),
    );
    Ok(Json(ProcessCancelAccepted {
        accepted: true,
        operation_id,
        process_id: process_id.clone(),
    }))
}

/// Wait for one durable work item to reach a terminal state, then return its
/// outcome and the authoritative event log.
///
/// This is the host-facing "wait for the work item" flow. It routes through
/// the configured process-work port
/// (ADR 0016) — the Restate ingress attach, never a store poll loop — and bounds
/// the wait with `tokio::time::timeout` so a still-running or unknown-to-this-pod
/// process cannot pin the request. On terminal it reconciles from `events_after`
/// (ADR 0017): the durable log is the truth; the best-effort event sink is only
/// freshness and may have dropped events.
pub(crate) async fn await_work(
    AxumPath(process_id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Json<WorkAwaitResult>, AppError> {
    const AWAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
    let process_id = ProcessId::from(process_id);
    let outcome = match tokio::time::timeout(
        AWAIT_TIMEOUT,
        state.core.processes().await_output(&process_id),
    )
    .await
    {
        Ok(Ok(outcome)) => outcome,
        // Audited: the Restate process attachment lowers workflow/transport failures to untyped PluginError::Session values.
        Ok(Err(err)) => return Err(AppError::internal(err)),
        Err(_elapsed) => {
            return Err(AppError::gateway_timeout(format!(
                "timed out waiting for process `{process_id}` to terminate"
            )));
        }
    };
    let events: Vec<WorkAwaitEvent> = state
        .core
        .processes()
        .events(&process_id, 0)
        .await
        // Audited: process-event reads use the global registry and have no session tombstone contract.
        .map_err(AppError::internal)?
        .into_iter()
        .map(|event| WorkAwaitEvent {
            sequence: event.sequence,
            event_type: event.event_type,
        })
        .collect();
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
    let session_id = SessionId::from(state.admit_session(&query, "api.lashlang_graphs").await?);
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
    let session_id = SessionId::from(state.admit_session(&query, "api.lashlang_graph").await?);
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
            TurnEvent::AssistantProseDelta { text } => {
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

pub(crate) async fn enqueue_button_trigger_command(
    state: &AppState,
    session_id: &SessionId,
    button: ButtonChoice,
    pressed_at: &str,
    operation_id: &str,
    scoped_effect_controller: lash::runtime::ScopedEffectController<'_>,
) -> AnyhowResult<lash::triggers::TriggerEmitReport> {
    let payload = json!({
        "pressed_at": pressed_at,
        "button": button.as_str(),
        "message": format!("user pressed the {} button", button.lower()),
    });
    let source_key = lash::triggers::empty_trigger_source_key(BUTTON_TRIGGER_SOURCE_TYPE)
        .context("button source key")?;
    state.trace_for_session(
        session_id,
        "trigger.emit",
        json!({
            "resource_type": BUTTON_TRIGGER_RESOURCE,
            "alias": BUTTON_TRIGGER_ALIAS,
            "event": BUTTON_TRIGGER_EVENT,
            "source_type": BUTTON_TRIGGER_SOURCE_TYPE,
            "source_key": source_key,
            "payload": payload.clone(),
        }),
    );
    state
        .core
        .triggers()
        .emit(
            lash::triggers::TriggerOccurrenceRequest::new(
                BUTTON_TRIGGER_SOURCE_TYPE,
                source_key,
                payload,
                format!("workbench-button-trigger:{operation_id}"),
            )
            .with_source(json!({}))
            .for_session(session_id),
            scoped_effect_controller,
        )
        .await
        .context("emit button trigger occurrence")
}

pub(crate) async fn enqueue_mail_received_trigger_command(
    state: &AppState,
    session_id: &SessionId,
    message: &mail::MailDelivery,
    operation_id: &str,
    scoped_effect_controller: lash::runtime::ScopedEffectController<'_>,
) -> AnyhowResult<lash::triggers::TriggerEmitReport> {
    let payload = json!({
        "account": message.account,
        "title": message.title,
        "text": message.text,
    });
    let source_key = lash::triggers::empty_trigger_source_key(MAIL_RECEIVED_SOURCE_TYPE)
        .context("mail source key")?;
    state.trace_for_session(
        session_id,
        "trigger.emit",
        json!({
            "resource_type": MAIL_EVENT_RESOURCE,
            "alias": MAIL_EVENT_ALIAS,
            "event": MAIL_EVENT_EVENT,
            "source_type": MAIL_RECEIVED_SOURCE_TYPE,
            "source_key": source_key,
            "payload": payload.clone(),
        }),
    );
    state
        .core
        .triggers()
        .emit(
            lash::triggers::TriggerOccurrenceRequest::new(
                MAIL_RECEIVED_SOURCE_TYPE,
                source_key,
                payload,
                format!("workbench-mail-trigger:{operation_id}"),
            )
            .with_source(json!({}))
            .for_session(session_id),
            scoped_effect_controller,
        )
        .await
        .context("emit mail received trigger occurrence")
}

pub(crate) fn workbench_lashlang_abilities() -> lashlang::LashlangAbilities {
    lashlang::LashlangAbilities::default()
        .with_processes()
        .with_sleep()
        .with_process_signals()
        .with_triggers()
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
            },
        ))
        .await;
        sink.emit(TurnActivity::new(
            lash::TurnActivityId::new("failed"),
            TurnEvent::AssistantProseDelta {
                text: "discarded ".into(),
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
