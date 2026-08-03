async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "service": "agent-workbench", "status": "ok" }))
}

async fn index() -> Html<&'static str> {
    Html(ui::INDEX_HTML)
}

async fn app_state(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<StateReadSnapshot>, AppError> {
    let session_id = query.resolve(&state)?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: session_id.clone(),
        })?;
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .map_err(|error| state.session_admission_error(&session_id, "api.state", error))?;
    let observation_snapshot = session.observe().recoverable_chat_snapshot();
    let active_turns = state.active_turns.for_session(&session_id);
    let active_turn_ids = active_turns
        .iter()
        .map(|address| address.turn_id.clone())
        .collect::<BTreeSet<_>>();
    let mut messages: Vec<_> = observation_snapshot
        .read_view
        .messages()
        .iter()
        .map(chat_message_from_committed)
        .collect();
    let mut transcript = transcript_rows_from_committed(&observation_snapshot.read_view);
    let committed_message_ids = messages
        .iter()
        .map(|message| message.id.clone())
        .collect::<BTreeSet<_>>();
    state
        .event_tx
        .reconcile_settled(&session_id, &committed_message_ids, &active_turn_ids);
    let product_events = state.event_tx.snapshot(&session_id);
    let product_messages = product_events
        .events
        .iter()
        .filter_map(|event| match &event.item {
            StreamItem::Message { message } => Some(message.clone()),
            StreamItem::TurnInput { .. } | StreamItem::Done { .. } => None,
        })
        .collect::<Vec<_>>();
    let mut message_ids = messages
        .iter()
        .map(|message| message.id.clone())
        .collect::<BTreeSet<_>>();
    for message in product_messages {
        if message_ids.insert(message.id.clone()) {
            transcript.push(TranscriptRow::Message {
                message: message.clone(),
            });
            messages.push(message);
        }
    }
    let pending_turn_inputs = session
        .pending_turn_inputs()
        .await
        // Audited: this facade read lowers TurnInputStore failures to RuntimeError::StoreCommitFailed without a typed cause.
        .map_err(AppError::internal)?;
    let queued_work = session.queued_work().await.map_err(AppError::internal)?;
    let turn_input_applications = session
        .remote_turn_input_applications()
        .await
        // Audited: application reconciliation lowers TurnInputStore failures to RuntimeError::StoreCommitFailed without a typed cause.
        .map_err(AppError::internal)?;
    let usage = session.usage_report();
    let observation =
        RemoteSessionObservation::from_core(lash::observe::SessionObservation {
            read_view: observation_snapshot.read_view,
            cursor: observation_snapshot.cursor,
        });
    drop(session);
    for address in &active_turns {
        let message_id = workbench_turn_user_message_id(&address.turn_id);
        if message_ids.contains(&message_id) {
            continue;
        }
        if let Some(text) = state
            .active_turns
            .prompt_for(&address.session_id, &address.turn_id)
        {
            let message = ChatMessage {
                id: message_id,
                role: "user".to_string(),
                text,
                at: String::new(),
            };
            message_ids.insert(message.id.clone());
            transcript.push(TranscriptRow::Message {
                message: message.clone(),
            });
            messages.push(message);
        }
    }
    Ok(Json(StateReadSnapshot {
        transcript,
        state: StateSnapshot {
            settings: state.settings_for_session(session_id.clone()),
            messages,
            observation,
            product_events,
            active_turns,
            pending_turn_inputs,
            queued_work,
            turn_input_applications,
            usage,
        },
    }))
}

const MAX_WORKBENCH_ATTACHMENT_BYTES: usize = 1024 * 1024;

async fn upload_attachment(
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
    let attachment = state
        .attachment_store
        .put(
            bytes,
            lash::attachments::AttachmentCreateMeta::new(media_type, None, Some(name.to_string())),
        )
        .await
        // Audited: the content-addressed attachment store has no session identity or tombstone error variant.
        .map_err(AppError::internal)?;
    let retrieve_url = format!("/api/attachments/{}", attachment.id);
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

async fn retrieve_attachment(
    AxumPath(attachment_id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Response, AppError> {
    let attachment_id = attachment_id.trim();
    if attachment_id.is_empty() {
        return Err(AppError::bad_request("attachment id is required"));
    }
    let stored = match state
        .attachment_store
        .get(&lash::attachments::AttachmentId::new(attachment_id))
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
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "private, no-store")
        .header("x-lash-attachment-id", attachment_id)
        .body(Body::from(stored.bytes))
        .expect("valid attachment response"))
}

fn chat_message_from_committed(message: &lash::messages::Message) -> ChatMessage {
    ChatMessage {
        id: message.id.clone(),
        role: lash::message_role(message).to_string(),
        text: lash::message_text(message),
        // The durable session graph records ordering but not a presentation
        // timestamp. The workbench does not render this field, so keep the
        // established wire shape without fabricating a time during resume.
        at: String::new(),
    }
}

fn transcript_rows_from_committed(
    read_view: &lash::persistence::SessionReadView,
) -> Vec<TranscriptRow> {
    read_view
        .chronological_projection()
        .into_entries()
        .into_iter()
        .filter_map(|entry| match entry.payload {
            lash::persistence::ChronologicalPayload::Message(message) => {
                Some(TranscriptRow::Message {
                    message: chat_message_from_committed(&message),
                })
            }
            lash::persistence::ChronologicalPayload::ProtocolEvent(event) => {
                match lash_protocol_rlm::decode_rlm_protocol_event(&event) {
                    Some(lash_rlm_types::RlmProtocolEvent::RlmAssistantContent(content))
                        if !content.reasoning.trim().is_empty() =>
                    {
                        Some(TranscriptRow::Reasoning {
                            id: content.id,
                            text: content.reasoning,
                        })
                    }
                    Some(lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(step))
                        if !step.code.trim().is_empty() =>
                    {
                        let mut output = step.output.join("\n");
                        if let Some(final_output) = step.final_output {
                            let final_output = serde_json::to_string_pretty(&final_output)
                                .unwrap_or_else(|_| final_output.to_string());
                            if !output.is_empty() {
                                output.push('\n');
                            }
                            output.push_str(&final_output);
                        }
                        Some(TranscriptRow::CodeBlock {
                            id: step.id,
                            language: "lashlang".to_string(),
                            code: step.code,
                            output,
                            success: step.error.is_none(),
                            error: step.error,
                        })
                    }
                    _ => None,
                }
            }
        })
        .collect()
}

async fn session_events(
    State(state): State<AppState>,
    Query(query): Query<ProductEventsQuery>,
) -> Result<Response, AppError> {
    let session_id = SessionQuery {
        session_id: query.session_id.clone(),
    }
    .resolve(&state)?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: session_id.clone(),
        })?;
    let (replay, mut product_events) = state
        .event_tx
        .subscribe_after(&session_id, query.cursor.unwrap_or(0));
    let event_registry = state.event_tx.clone();
    let (tx, rx) = mpsc::channel::<ProductStreamItem>(64);
    tokio::spawn(async move {
        for event in replay {
            if tx.send(ProductStreamItem::Event { event }).await.is_err() {
                return;
            }
        }
        loop {
            match product_events.recv().await {
                Ok(event) => {
                    if tx.send(ProductStreamItem::Event { event }).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_count)) => {
                    let _ = tx
                        .send(ProductStreamItem::Resync {
                            snapshot: event_registry.snapshot(&session_id),
                        })
                        .await;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    Ok(ndjson_response(rx))
}

async fn session_observations(
    State(state): State<AppState>,
    Query(query): Query<EventsQuery>,
) -> Result<Response, AppError> {
    let session_id = SessionQuery {
        session_id: query.session_id.clone(),
    }
    .resolve(&state)?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: session_id.clone(),
        })?;
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .map_err(|error| {
            state.session_admission_error(&session_id, "api.observations", error)
        })?;
    let cursor = match query.cursor.as_deref().filter(|cursor| !cursor.trim().is_empty()) {
        Some(cursor) => serde_json::from_value::<SessionCursor>(json!(cursor))
            .map_err(|err| AppError::bad_request(format!("invalid session cursor: {err}")))?,
        None => session.observe().recoverable_chat_snapshot().cursor,
    };
    let (tx, rx) = mpsc::channel::<ObservationStreamItem>(64);
    tokio::spawn(async move {
        forward_session_observations(session, cursor, tx).await;
    });
    Ok(ndjson_response(rx))
}

async fn send_turn(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    Json(request): Json<TurnRequest>,
) -> Result<Json<CommandAccepted>, AppError> {
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
    let session_id = query.resolve(&state)?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::EnqueueTurn {
            session_id: session_id.clone(),
        })?;
    drop(
        state
            .core
            .session(session_id.clone())
            .open()
            .await
            .map_err(|error| {
                state.session_admission_error(&session_id, "api.turn", error)
            })?,
    );
    if let Some(attachment_id) = attachment_id.as_deref() {
        match state
            .attachment_store
            .get(&lash::attachments::AttachmentId::new(attachment_id))
            .await
        {
            Ok(_) => {}
            Err(lash::persistence::AttachmentStoreError::NotFound(_)) => {
                return Err(AppError::not_found(format!(
                    "attachment `{attachment_id}` was not found"
                )));
            }
            // Audited: the content-addressed attachment store has no session identity or tombstone error variant.
            Err(err) => return Err(AppError::internal(err)),
        }
    }
    let turn_model = model_spec_for_request(
        &state,
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
    let turn_id = format!("workbench-turn-{}", uuid::Uuid::new_v4());
    state.push_message_with_id_for_session(
        &session_id,
        workbench_turn_user_message_id(&turn_id),
        "user",
        text.clone(),
    );
    state.track_turn_prompt(&session_id, &turn_id, text.clone());
    if let Err(err) = restate::submit_user_turn(
        &state,
        restate::WorkbenchTurnWorkflowRequest {
            turn_id: turn_id.clone(),
            session_id: session_id.clone(),
            text,
            model: ModelSelection::from_spec(&turn_model),
            attachment_id,
        },
    )
    .await
    {
        state.active_turns.remove(&session_id, &turn_id);
        return Err(err);
    }
    Ok(Json(CommandAccepted { accepted: true }))
}

async fn enqueue_turn_input(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    Json(request): Json<TurnInputRequest>,
) -> Result<Json<TurnInputReceipt>, AppError> {
    let text = request.text.trim().to_string();
    if text.is_empty() {
        return Err(AppError::bad_request("message text is required"));
    }
    let session_id = query.resolve(&state)?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::EnqueueTurnInput {
            session_id: session_id.clone(),
        })?;
    let ingress = match request.ingress {
        TurnInputIngressRequest::ActiveTurn => {
            let active = state.active_turns.for_session(&session_id);
            let [address] = active.as_slice() else {
                return Err(AppError::conflict(
                    "inject now requires exactly one running turn",
                ));
            };
            lash::persistence::TurnInputIngress::active_turn(
                address.turn_id.clone(),
                lash::persistence::TurnInputCheckpointBoundary::AfterWork,
            )
        }
        TurnInputIngressRequest::NextTurn => lash::persistence::TurnInputIngress::next_turn(),
    };
    let source_id = format!("workbench-turn-input-{}", uuid::Uuid::new_v4());
    let acceptance = state
        .core
        .enqueue_turn_input(
            session_id.clone(),
            lash::TurnInput::text(text.clone()),
            ingress,
            Some(source_id),
        )
        .await
        .map_err(|error| {
            state.session_admission_error(&session_id, "api.turn.input", error)
        })?;
    reject_if_active_turn_settled(&state, &acceptance).await?;
    let accepted_state = match acceptance.ingress {
        lash::persistence::TurnInputIngress::ActiveTurn { .. } => {
            lash::persistence::TurnInputState::PendingActive
        }
        lash::persistence::TurnInputIngress::NextTurn => {
            lash::persistence::TurnInputState::DeferredNextTurn
        }
    };
    let receipt = TurnInputReceipt {
        accepted: true,
        input_id: acceptance.input_id.clone(),
        ingress: acceptance.ingress.clone(),
        state: accepted_state,
        text,
    };
    state.trace_for_session(
        &session_id,
        "turn_input.enqueued",
        serde_json::to_value(&receipt).unwrap_or(Value::Null),
    );
    state.publish_for_session_identified(
        &session_id,
        format!("turn-input:{}", receipt.input_id),
        StreamItem::TurnInput {
            receipt: receipt.clone(),
        },
    );
    Ok(Json(receipt))
}

async fn reject_if_active_turn_settled(
    state: &AppState,
    acceptance: &lash::TurnInputAcceptanceReceipt,
) -> Result<(), AppError> {
    let Some(turn_id) = acceptance.ingress.active_turn_id() else {
        return Ok(());
    };
    if state
        .active_turns
        .contains(&acceptance.session_id, turn_id)
    {
        return Ok(());
    }

    let session = state
        .core
        .session(acceptance.session_id.clone())
        .open()
        .await
        .map_err(AppError::runtime)?;
    let outcome = session
        .cancel_pending_turn_input(&acceptance.input_id)
        .await
        .map_err(AppError::runtime)?;
    match outcome {
        lash::PendingTurnInputCancelOutcome::Cancelled(_)
        | lash::PendingTurnInputCancelOutcome::AlreadyCancelled(_) => Err(AppError::conflict(
            "the running turn settled before the input could be injected",
        )),
        lash::PendingTurnInputCancelOutcome::AlreadyClaimed { .. }
        | lash::PendingTurnInputCancelOutcome::AlreadyCompleted(_) => Ok(()),
        // Audited: this is a locally synthesized reconciliation-invariant failure, not a propagated store error.
        lash::PendingTurnInputCancelOutcome::NotFound => Err(AppError::internal(format!(
            "active-turn input `{}` disappeared during settle reconciliation",
            acceptance.input_id
        ))),
    }
}

async fn button_trigger(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    Json(request): Json<ButtonEventRequest>,
) -> Result<Json<CommandAccepted>, AppError> {
    let session_id = query.resolve(&state)?;
    let turn_model = model_spec_for_request(
        &state,
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
            session_id,
            button: request.button,
            model,
            pressed_at,
        },
    )
    .await?;
    Ok(Json(CommandAccepted { accepted: true }))
}

async fn list_accounts(State(state): State<AppState>) -> Json<Vec<mail::AccountSummary>> {
    Json(state.mail_world.account_summaries())
}

async fn list_triggers(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<Vec<WorkbenchTriggerRegistration>>, AppError> {
    let session_id = query.resolve(&state)?;
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

async fn set_trigger_enabled(
    AxumPath(subscription_key): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
    Json(request): Json<TriggerEnabledRequest>,
) -> Result<Json<TriggerMutationResponse>, AppError> {
    let session_id = query.resolve(&state)?;
    let record = trigger_record_for_session(&state, &session_id, &subscription_key).await?;
    let changed = record.enabled != request.enabled;
    let command = if request.enabled {
        lash::triggers::TriggerCommand::Enable {
            owner_scope: record.owner_scope.clone(),
            actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(&session_id)),
            subscription_key: record.subscription_key.clone(),
            expected_revision: record.revision,
        }
    } else {
        lash::triggers::TriggerCommand::Disable {
            owner_scope: record.owner_scope.clone(),
            actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(&session_id)),
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
        return Err(AppError::internal("trigger mutation returned a list outcome"));
    };
    let registration = lash::triggers::TriggerRegistration::from(&receipt.record_snapshot);
    state.trace_for_session(
        &session_id,
        "api.triggers.enabled",
        json!({
            "subscription_key": subscription_key,
            "enabled": request.enabled,
            "changed": changed,
        }),
    );
    Ok(Json(TriggerMutationResponse {
        changed,
        registration: Some(registration),
    }))
}

async fn delete_trigger(
    AxumPath(subscription_key): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<TriggerMutationResponse>, AppError> {
    let session_id = query.resolve(&state)?;
    let record = trigger_record_for_session(&state, &session_id, &subscription_key).await?;
    state
        .trigger_store
        .execute_command(
            &format!("workbench-trigger-delete-{}", uuid::Uuid::new_v4()),
            lash::triggers::TriggerCommand::Delete {
                owner_scope: record.owner_scope.clone(),
                actor: lash::process::ProcessOriginator::session(lash::process::SessionScope::new(&session_id)),
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

async fn trigger_record_for_session(
    state: &AppState,
    session_id: &str,
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

async fn add_account(
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

async fn delete_account(
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

async fn delete_message(
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

async fn account_inbox(
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
async fn enqueue_tool_catalog_refresh(
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
    let receipt = session
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
        // Audited: session-command enqueue lowers store and queued-work failures to RuntimeError without a tombstone cause.
        .map_err(AppError::internal)?;
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

async fn inject_message(
    AxumPath(slug): AxumPath<String>,
    State(state): State<AppState>,
    Json(request): Json<InjectMessageRequest>,
) -> Result<Json<CommandAccepted>, AppError> {
    let turn_model = model_spec_for_request(
        &state,
        request.model.as_deref(),
        request.model_variant.as_deref(),
    )?;
    let model = ModelSelection::from_spec(&turn_model);
    state.set_selected_model(model.clone());
    let delivered = state
        .mail_world
        .deliver(
            &slug,
            request.title.as_deref().unwrap_or_default(),
            request.text.as_deref().unwrap_or_default(),
        )
        .map_err(AppError::not_found)?;
    let message = delivered.message;
    let delivery = delivered.delivery;
    state.trace(
        "api.accounts.inject",
        json!({ "account": slug, "title": message.title }),
    );
    state.push_message(
        "event",
        format!("message delivered to `inbox.{}`: {}", slug, message.title),
    );
    restate::submit_mail_received(
        &state,
        restate::WorkbenchMailReceivedWorkflowRequest {
            operation_id: format!("workbench-mail-{}", uuid::Uuid::new_v4()),
            session_id: state.current_session_id(),
            model,
            delivery,
        },
    )
    .await?;
    Ok(Json(CommandAccepted { accepted: true }))
}

async fn cancel_turn(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<TurnCancelResponse>, AppError> {
    let session_id = query.resolve(&state)?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::CancelTurn {
            session_id: session_id.clone(),
        })?;
    let cancellations = state.cancel_turns_for_session(&session_id).await?;
    state.trace_for_session(
        &session_id,
        "api.turn.cancel",
        json!({ "session_id": session_id, "cancellations": cancellations }),
    );
    Ok(Json(TurnCancelResponse {
        accepted: !cancellations.is_empty(),
        cancellations,
    }))
}

async fn reset_chat(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<StateSnapshot>, AppError> {
    let old_session_id = query.resolve(&state)?;
    restate::cancel_cron_jobs_for_session(&state, &old_session_id, "reset").await?;
    let execution_scope = state
        .core
        .session_delete_scope(&old_session_id)
        .await
        // Audited: first-party existence probes return absence or untyped factory/backend errors, never SessionDeleted.
        .map_err(AppError::internal)?;
    restate::submit_session_delete(
        &state,
        restate::WorkbenchSessionDeleteWorkflowRequest {
            operation_id: format!("workbench-delete-{}", uuid::Uuid::new_v4()),
            session_id: old_session_id.clone(),
            execution_scope,
        },
    )
    .await?;
    state.event_tx.remove(&old_session_id);
    let (rotated_old, new_session_id) = state.session_ids.rotate();
    if rotated_old != old_session_id {
        eprintln!(
            "warning: workbench session changed during reset; deleted {old_session_id}, rotated {rotated_old}"
        );
    }
    state.trace_for_session(
        &old_session_id,
        "api.reset",
        json!({
            "old_session_id": old_session_id,
            "new_session_id": new_session_id.clone(),
        }),
    );
    let session = state
        .core
        .session(new_session_id.clone())
        .open()
        .await
        .map_err(AppError::session_open)?;
    let selected_model = model_spec_from_selection(state.selected_model());
    session
        .configure(lash::SessionConfigPatch {
            model: Some(selected_model),
            ..lash::SessionConfigPatch::default()
        })
        .await
        // Audited: session configuration updates only resident state and its current implementation is infallible.
        .map_err(AppError::internal)?;
    state.messages.lock().expect("messages lock").clear();
    state.lashlang_execution.clear();
    state.mail_world.clear();
    Ok(Json(StateSnapshot {
        settings: state.settings_for_session(new_session_id),
        messages: Vec::new(),
        observation: session.observe().current_remote_observation(),
        product_events: ProductEventSnapshot::default(),
        active_turns: Vec::new(),
        pending_turn_inputs: Vec::new(),
        queued_work: Vec::new(),
        turn_input_applications: Vec::new(),
        usage: session.usage_report(),
    }))
}

async fn list_work(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<Vec<WorkItem>>, AppError> {
    let session_id = query.resolve(&state)?;
    let observed = if query.is_explicit() {
        state
            .process_observer
            .snapshot_for_session(session_id.clone())
            .await
            // Audited: process observation reads the global registry, which has no session tombstone contract.
            .map_err(AppError::internal)?
            .items
    } else {
        state
            .process_observer
            .snapshot_all(&lash::process::ProcessListFilter {
                status: lash::process::ProcessStatusFilter::Any,
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
struct QueuedWorkBatchAction {
    accepted: bool,
    batch_id: String,
}

async fn list_queued_work(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<Vec<lash::persistence::QueuedWorkBatch>>, AppError> {
    let session_id = query.resolve(&state)?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: session_id.clone(),
        })?;
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .map_err(|error| {
            state.session_admission_error(&session_id, "api.queued_work.list", error)
        })?;
    Ok(Json(session.queued_work().await.map_err(AppError::internal)?))
}

async fn run_queued_work_batch(
    AxumPath(batch_id): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<QueuedWorkBatchAction>, AppError> {
    let session_id = query.resolve(&state)?;
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
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .map_err(|error| {
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

    let turn_id = format!("workbench-queued-{}", uuid::Uuid::new_v4());
    let request = restate::WorkbenchQueuedTurnWorkflowRequest {
        turn_id: turn_id.clone(),
        session_id: session_id.clone(),
        reason: "workbench_manual_batch_run".to_string(),
        batch_ids: vec![batch_id.clone()],
        drain_id: Some(format!("workbench-queued-batch:{batch_id}")),
    };
    state.active_turns.insert(&session_id, &turn_id);
    if let Err(error) = restate::submit_queued_turn_request(
        &state.restate_http,
        &state.restate_ingress_url,
        &request,
    )
    .await
    {
        state.active_turns.remove(&session_id, &turn_id);
        return Err(error);
    }
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

async fn cancel_queued_work_batch(
    AxumPath(batch_id): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<QueuedWorkBatchAction>, AppError> {
    let session_id = query.resolve(&state)?;
    state
        .authorization
        .authorize(WorkbenchAuthorizationAction::ManageQueuedWork {
            session_id: session_id.clone(),
        })?;
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .map_err(|error| {
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

async fn cancel_work(
    AxumPath(process_id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Json<ProcessCancelAccepted>, AppError> {
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
        lash::process::ProcessOriginator::Session { session_id } => session_id.clone(),
        lash::process::ProcessOriginator::Host { .. } => state.current_session_id(),
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
        process_id,
    }))
}

/// Wait for one durable work item to reach a terminal state, then return its
/// outcome and the authoritative event log.
///
/// This is the host-facing "wait for the work item" flow. It routes through
/// [`ProcessWorkDriver::await_terminal`](lash::process::ProcessWorkDriver::await_terminal)
/// (ADR 0016) — the Restate ingress attach, never a store poll loop — and bounds
/// the wait with `tokio::time::timeout` so a still-running or unknown-to-this-pod
/// process cannot pin the request. On terminal it reconciles from `events_after`
/// (ADR 0017): the durable log is the truth; the best-effort event sink is only
/// freshness and may have dropped events.
async fn await_work(
    AxumPath(process_id): AxumPath<String>,
    State(state): State<AppState>,
) -> Result<Json<WorkAwaitResult>, AppError> {
    const AWAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
    let outcome = match tokio::time::timeout(
        AWAIT_TIMEOUT,
        state.process_work_driver.await_terminal(&process_id),
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
        .process_work_driver
        .process_registry()
        .events_after(&process_id, 0)
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
        process_id,
        outcome,
        events,
    }))
}

async fn list_lashlang_graphs(
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<execution_graphs::LashlangGraphIndex>, AppError> {
    let session_id = query.resolve(&state)?;
    let index = execution_graphs::index_for_session(
        &state.process_observer,
        &session_id,
        state.lashlang_execution.graphs(),
    )
    .await?;
    Ok(Json(index))
}

async fn lashlang_graph(
    AxumPath(graph_key): AxumPath<String>,
    State(state): State<AppState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<TraceLashlangGraph>, AppError> {
    let session_id = query.resolve(&state)?;
    let graph = execution_graphs::visible_graph_by_key(
        &state.process_observer,
        &session_id,
        state.lashlang_execution.graphs(),
        &graph_key,
    )
    .await?;
    Ok(Json(graph))
}

fn ndjson_response<T>(rx: mpsc::Receiver<T>) -> Response
where
    T: Serialize + Send + 'static,
{
    let stream = ReceiverStream::new(rx).map(|item| {
        let mut line = serde_json::to_string(&item).unwrap_or_else(|_err| {
            json!({
                "type": "unavailable",
            })
            .to_string()
        });
        line.push('\n');
        Ok::<Bytes, Infallible>(Bytes::from(line))
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .expect("valid streaming response")
}

async fn forward_session_observations(
    session: lash::LashSession,
    cursor: SessionCursor,
    tx: mpsc::Sender<ObservationStreamItem>,
) {
    if tx
        .send(ObservationStreamItem::Cursor {
            cursor: cursor.to_string(),
        })
        .await
        .is_err()
    {
        return;
    }
    let mut stream = session.observe().subscribe_recoverable_chat(cursor);
    let mut sequence = 0;
    while let Some(item) = stream.next().await {
        match item {
            Ok(lash::recoverable_chat::RecoverableChatUpdate::Event { event, .. }) => {
                let event = RemoteSessionObservationEvent::from_core(sequence, event);
                sequence = sequence.saturating_add(1);
                if tx
                    .send(ObservationStreamItem::Observation {
                        event: Box::new(event),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(lash::recoverable_chat::RecoverableChatUpdate::TerminalReplacement {
                event,
                snapshot,
                ..
            }) => {
                let event = RemoteSessionObservationEvent::from_core(sequence, event);
                sequence = sequence.saturating_add(1);
                if tx
                    .send(ObservationStreamItem::TerminalReplacement {
                        cursor: snapshot.cursor.to_string(),
                        event: Box::new(event),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(lash::recoverable_chat::RecoverableChatUpdate::ReplayGap { snapshot, gap }) => {
                let observation =
                    RemoteSessionObservation::from_core(lash::observe::SessionObservation {
                        read_view: snapshot.read_view,
                        cursor: snapshot.cursor,
                    });
                let gap = RemoteLiveReplayGap::from(gap);
                if tx
                    .send(ObservationStreamItem::ReplayGap {
                        observation: Box::new(observation),
                        gap: Box::new(gap),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(err) => {
                eprintln!("warning: workbench Lash observation stream stopped: {err}");
                break;
            }
        }
    }
}

#[derive(Default)]
struct TurnStreamState {
    assistant_prose: Vec<TurnStreamProseChunk>,
}

struct TurnStreamProseChunk {
    correlation_id: lash::TurnActivityId,
    text: String,
}

impl TurnStreamState {
    fn assistant_prose(&self) -> String {
        self.assistant_prose
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect()
    }

    fn settle_terminal(&mut self) {
        self.assistant_prose.clear();
    }
}

struct ChannelTurnEvents {
    turn_state: Arc<Mutex<TurnStreamState>>,
}

#[async_trait]
impl TurnActivitySink for ChannelTurnEvents {
    async fn emit(&self, activity: TurnActivity) {
        let mut turn_state = self.turn_state.lock().expect("turn state lock");
        match activity.event {
            TurnEvent::AssistantProseDelta { text } => {
                turn_state.assistant_prose.push(TurnStreamProseChunk {
                    correlation_id: activity.correlation_id,
                    text: text.to_string(),
                });
            }
            TurnEvent::ModelAttemptReset {
                assistant_prose_correlation_ids,
                ..
            } => {
                turn_state.assistant_prose.retain(|chunk| {
                    !assistant_prose_correlation_ids.contains(&chunk.correlation_id)
                });
            }
            _ => {}
        }
    }
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
        sink.emit(TurnActivity::independent(
            TurnEvent::ModelAttemptReset {
                assistant_prose_correlation_ids: vec![lash::TurnActivityId::new("failed")],
                reasoning_correlation_ids: Vec::new(),
            },
        ))
        .await;
        sink.emit(TurnActivity::new(
            lash::TurnActivityId::new("successful"),
            TurnEvent::AssistantProseDelta {
                text: "answer".into(),
            },
        ))
        .await;

        assert_eq!(
            turn_state
                .lock()
                .expect("turn state lock")
                .assistant_prose(),
            "kept answer"
        );
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
            let mut projection = turn_state.lock().expect("turn state lock");
            assert_eq!(projection.assistant_prose(), "provisional text");
            projection.settle_terminal();
            assert!(projection.assistant_prose().is_empty());
        }
    }
}

pub(crate) async fn enqueue_button_trigger_command(
    state: &AppState,
    session_id: &str,
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
    session_id: &str,
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

fn workbench_lashlang_abilities() -> lashlang::LashlangAbilities {
    lashlang::LashlangAbilities::default()
        .with_processes()
        .with_sleep()
        .with_process_signals()
        .with_triggers()
}
