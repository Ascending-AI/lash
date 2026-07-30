impl AppState {
    fn current_session_id(&self) -> String {
        self.session_ids.current()
    }

    fn selected_model(&self) -> ModelSelection {
        self.selected_model
            .lock()
            .expect("selected model lock")
            .clone()
    }

    fn set_selected_model(&self, model: ModelSelection) {
        *self.selected_model.lock().expect("selected model lock") = model;
    }

    fn settings_for_session(&self, session_id: String) -> Settings {
        let selected_model = self.selected_model();
        Settings {
            model: selected_model.model,
            model_variant: selected_model.model_variant,
            web_configured: self.web_configured,
            model_variants: vec!["", "low", "medium", "high"],
            session_id,
        }
    }

    #[cfg(test)]
    fn messages_snapshot(&self) -> Vec<ChatMessage> {
        self.messages.lock().expect("messages lock").clone()
    }

    fn trace(&self, name: &str, payload: Value) {
        self.trace_for_session(&self.current_session_id(), name, payload);
    }

    fn trace_for_session(&self, session_id: &str, name: &str, payload: Value) {
        emit_workbench_trace(
            &self.trace_sink,
            Some(session_id.to_string()),
            name,
            payload,
        );
    }

    fn session_admission_error(
        &self,
        session_id: &str,
        surface: &str,
        error: lash::EmbedError,
    ) -> AppError {
        if let Some((deleted_session_id, context)) = deleted_session_details(&error) {
            self.trace_for_session(
                session_id,
                "session.admission_refused",
                json!({
                    "session_id": session_id,
                    "surface": surface,
                    "consulted_state": {
                        "kind": "session_store_tombstone",
                        "freshness": "admission_read",
                        "session_id": deleted_session_id,
                    },
                    "tombstone_outcome": "retired",
                    "outcome": "refused",
                    "store_context": context,
                }),
            );
        }
        AppError::session_open(error)
    }

    fn publish_for_session_identified(
        &self,
        session_id: &str,
        event_id: impl Into<String>,
        item: StreamItem,
    ) {
        let _ = self
            .event_tx
            .publish_identified(session_id, event_id, item);
    }

    fn publish_turn_done(&self, session_id: &str, turn_id: &str) {
        self.publish_for_session_identified(
            session_id,
            format!("turn:{turn_id}:done"),
            StreamItem::Done {
                turn_id: Some(turn_id.to_string()),
            },
        );
    }

    fn publish_trigger_dispatch_done(&self, session_id: &str, operation_id: &str) {
        if self.active_turns.for_session(session_id).is_empty() {
            self.publish_for_session_identified(
                session_id,
                format!("operation:{operation_id}:done"),
                StreamItem::Done { turn_id: None },
            );
        }
    }

    #[cfg(test)]
    fn track_turn(&self, session_id: &str, turn_id: &str) {
        self.active_turns.insert(session_id, turn_id);
    }

    fn track_turn_prompt(&self, session_id: &str, turn_id: &str, prompt: String) {
        self.active_turns
            .insert_with_prompt(session_id, turn_id, Some(prompt));
    }

    /// Fan out exact-address cooperative cancellation to the active turns the
    /// UI submitted for `session_id`.
    async fn cancel_turns_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<TurnCancelReceipt>, AppError> {
        let active = self.active_turns.for_session(session_id);
        let session = self
            .core
            .session(session_id)
            .open()
            .await
            .map_err(|error| {
                self.session_admission_error(session_id, "api.turn.cancel", error)
            })?;
        let mut receipts = Vec::with_capacity(active.len());
        let mut operation_ids = Vec::with_capacity(active.len());
        for tracked_address in active {
            let address = session.turn_address(&tracked_address.turn_id);
            let request_id = format!("workbench-stop-{}", uuid::Uuid::new_v4());
            operation_ids.push(format!("{}:{request_id}", address.turn_id));
            let driver = self.core.turn_work_driver();
            let receipt = driver
                .request_cancel(lash::TurnCancelRequest::new(
                    address.clone(),
                    request_id.clone(),
                    Some("user".to_string()),
                ).with_reason("workbench Stop control"))
                .await
                .map_err(|err| AppError::internal(err.to_string()))?;
            let (terminal, terminal_error) = if matches!(
                &receipt.outcome,
                lash::TurnCancelOutcome::Requested(_)
                    | lash::TurnCancelOutcome::AlreadyRequested(_)
            ) {
                match driver
                    .await_terminal_with_timeout(&address, TURN_TERMINAL_ATTACH_TIMEOUT)
                    .await
                {
                    Ok(terminal) => (Some(terminal), None),
                    Err(err) if err.code.as_str() == "turn_terminal_await_timeout" => {
                        (None, Some(err))
                    }
                    Err(err) => return Err(AppError::internal(err.to_string())),
                }
            } else {
                (None, None)
            };
            let routing_retained = if terminal.is_none()
                && terminal_error
                    .as_ref()
                    .is_some_and(|err| err.code.as_str() == "turn_terminal_await_timeout")
            {
                match self.restate_turn_is_active(&address).await {
                    Ok(true) => true,
                    Ok(false) => {
                        self.active_turns
                            .remove(&address.session_id, &address.turn_id);
                        false
                    }
                    Err(err) => {
                        self.trace_for_session(
                            &address.session_id,
                            "turn.cancel_liveness_unknown",
                            json!({
                                "session_id": address.session_id,
                                "turn_id": address.turn_id,
                                "error": err.to_string(),
                            }),
                        );
                        true
                    }
                }
            } else {
                self.active_turns
                    .remove(&address.session_id, &address.turn_id);
                false
            };
            self.trace_for_session(
                &address.session_id,
                "turn.cancel_requested",
                json!({
                    "session_id": address.session_id,
                    "turn_id": address.turn_id,
                    "request_id": request_id,
                    "outcome": format!("{:?}", receipt.outcome),
                    "terminal": terminal,
                    "terminal_error": terminal_error,
                    "routing_retained": routing_retained,
                }),
            );
            receipts.push(TurnCancelReceipt {
                address,
                outcome: receipt.outcome,
                terminal,
                terminal_error,
            });
        }
        if !receipts.is_empty() {
            self.publish_for_session_identified(
                session_id,
                format!("turn-cancel:{}:done", operation_ids.join(",")),
                StreamItem::Done { turn_id: None },
            );
        }
        Ok(receipts)
    }

    async fn restate_turn_is_active(&self, address: &lash::TurnAddress) -> AnyhowResult<bool> {
        let workflow = if address.turn_id.starts_with("workbench-queued-") {
            "WorkbenchQueuedTurnWorkflow"
        } else {
            "WorkbenchTurnWorkflow"
        };
        let admin = lash_restate::RestateAdminClient::new(
            lash_restate::RestateConnection::with_client(
                self.restate_admin_url.clone(),
                self.restate_http.clone(),
            ),
        );
        Ok(admin
            .workflow_invocation_status(workflow, &address.turn_id, "run")
            .await?
            .is_some_and(|status| status.is_still_active()))
    }

    fn push_message(&self, role: impl Into<String>, text: impl Into<String>) -> ChatMessage {
        self.push_message_for_session(&self.current_session_id(), role, text)
    }

    fn push_message_for_session(
        &self,
        session_id: &str,
        role: impl Into<String>,
        text: impl Into<String>,
    ) -> ChatMessage {
        self.push_message_with_id_for_session(
            session_id,
            uuid::Uuid::new_v4().to_string(),
            role,
            text,
        )
    }

    fn push_message_with_id_for_session(
        &self,
        session_id: &str,
        id: impl Into<String>,
        role: impl Into<String>,
        text: impl Into<String>,
    ) -> ChatMessage {
        let message = ChatMessage {
            id: id.into(),
            role: role.into(),
            text: text.into(),
            at: Utc::now().to_rfc3339(),
        };
        let inserted = self.event_tx.publish_identified(
            session_id,
            format!("message:{}", message.id),
            StreamItem::Message {
                message: message.clone(),
            },
        );
        if inserted {
            self.messages
                .lock()
                .expect("messages lock")
                .push(message.clone());
        }
        message
    }
}

fn emit_workbench_trace(
    sink: &Option<Arc<dyn TraceSink>>,
    session_id: Option<String>,
    name: &str,
    payload: Value,
) {
    let Some(sink) = sink else {
        return;
    };
    let context = session_id
        .map(|session_id| TraceContext::default().for_session(session_id))
        .unwrap_or_default();
    let record = TraceRecord::new(
        context,
        TraceEvent::Custom {
            name: format!("agent_workbench.{name}"),
            payload,
        },
    );
    if let Err(err) = sink.append(&record) {
        eprintln!("warning: failed to append agent-workbench trace event `{name}`: {err}");
    }
}

fn trace_work_item(item: &WorkItem) -> Value {
    json!({
        "process_id": item.process.process_id.clone(),
        "graph_key": item.process.graph_key.clone(),
        "kind": item.kind.clone(),
        "label": item.label.clone(),
        "status_label": item.process.status_label.clone(),
        "terminal": item.process.terminal,
        "created_at_ms": item.process.created_at_ms,
        "updated_at_ms": item.process.updated_at_ms,
        "input": item.process.input.clone(),
        "events": item.events.iter().map(|event| {
            json!({
                "sequence": event.sequence,
                "event_type": event.event_type.clone(),
                "occurred_at_ms": event.occurred_at_ms,
                "payload": event.payload.clone(),
            })
        }).collect::<Vec<_>>(),
    })
}

#[derive(Clone, Debug)]
struct WorkbenchSessionIds {
    current: Arc<Mutex<String>>,
    path: Option<Arc<PathBuf>>,
}

impl WorkbenchSessionIds {
    #[cfg(test)]
    fn fresh() -> Self {
        Self {
            current: Arc::new(Mutex::new(new_session_id())),
            path: None,
        }
    }

    fn persistent(path: PathBuf) -> AnyhowResult<Self> {
        let current = match std::fs::read_to_string(&path) {
            Ok(session_id) if !session_id.trim().is_empty() => session_id,
            Ok(_) => new_session_id(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => new_session_id(),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("read workbench session id `{}`", path.display()));
            }
        };
        let ids = Self {
            current: Arc::new(Mutex::new(current)),
            path: Some(Arc::new(path)),
        };
        ids.persist();
        Ok(ids)
    }

    fn current(&self) -> String {
        self.current.lock().expect("session id lock").clone()
    }

    fn rotate(&self) -> (String, String) {
        let mut current = self.current.lock().expect("session id lock");
        let old = current.clone();
        let new = new_session_id();
        *current = new.clone();
        drop(current);
        self.persist();
        (old, new)
    }

    fn persist(&self) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let temporary = path.with_extension("tmp");
        let current = self.current();
        std::fs::write(&temporary, current)
            .unwrap_or_else(|err| panic!("write session id `{}`: {err}", temporary.display()));
        std::fs::rename(&temporary, path).unwrap_or_else(|err| {
            panic!(
                "replace session id `{}` from `{}`: {err}",
                path.display(),
                temporary.display()
            )
        });
    }
}

fn new_session_id() -> String {
    format!("{SESSION_ID_PREFIX}-{}", uuid::Uuid::new_v4().simple())
}

fn model_spec_for_request(
    state: &AppState,
    model: Option<&str>,
    model_variant: Option<&str>,
) -> Result<lash::ModelSpec, AppError> {
    let selected_model = state.selected_model();
    let model = model
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(selected_model.model.as_str())
        .to_string();
    let model_variant = model_variant_for_request(&selected_model, model_variant);
    lash::ModelSpec::from_token_limits(
        model,
        model_variant
            .map(lash::provider::ReasoningSelection::Effort)
            .unwrap_or_default(),
        DEFAULT_CONTEXT_WINDOW_TOKENS,
        None,
    )
        .map(with_workbench_model_capability)
        .map_err(AppError::bad_request)
}

fn model_variant_for_request(
    selected_model: &ModelSelection,
    model_variant: Option<&str>,
) -> Option<String> {
    match model_variant {
        Some(value) => {
            let value = value.trim();
            if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        }
        None => selected_model.model_variant.clone(),
    }
}

fn model_spec_from_selection(selection: ModelSelection) -> lash::ModelSpec {
    lash::ModelSpec::from_token_limits(
        selection.model,
        selection
            .model_variant
            .map(lash::provider::ReasoningSelection::Effort)
            .unwrap_or_default(),
        DEFAULT_CONTEXT_WINDOW_TOKENS,
        None,
    )
    .expect("workbench model selection should use a valid token limit")
    .with_capability(workbench_model_capability())
}

fn with_workbench_model_capability(model: lash::ModelSpec) -> lash::ModelSpec {
    model.with_capability(workbench_model_capability())
}

fn workbench_model_capability() -> lash::provider::ModelCapability {
    lash::provider::ModelCapability {
        reasoning: Some(lash::provider::ReasoningCapability {
            efforts: ["low", "medium", "high"]
                .into_iter()
                .map(String::from)
                .collect(),
            default_effort: Some("medium".to_string()),
            aliases: Default::default(),
            encoding: lash::provider::ReasoningEncoding::Effort,
            disable: None,
            mandatory: false,
        }),
        cache_control: Some(lash::provider::CacheControlDialect::Anthropic),
        stream_termination: None,
        sampling: lash::provider::SamplingCapability::Configurable,
    }
}

async fn apply_model_selection_to_session(
    state: &AppState,
    session: &lash::LashSession,
    model: lash::ModelSpec,
    reason: &str,
) -> Result<(), AppError> {
    state.set_selected_model(ModelSelection::from_spec(&model));
    session
        .configure(lash::SessionConfigPatch {
            model: Some(model.clone()),
            ..lash::SessionConfigPatch::default()
        })
        .await
        .map_err(AppError::internal)?;
    state.trace_for_session(
        &session.session_id(),
        "model_selection.applied",
        json!({
            "reason": reason,
            "model": serde_json::to_value(&model).unwrap_or(Value::Null),
        }),
    );
    Ok(())
}

fn assistant_text_for_display(output: &TurnResult, streamed_prose: &str) -> String {
    let terminal = output.final_value().map(terminal_value_text).or_else(|| {
        output
            .tool_value()
            .map(|(_tool_name, value)| terminal_value_text(value))
    });
    let assistant = (!streamed_prose.trim().is_empty())
        .then(|| streamed_prose.to_string())
        .or_else(|| {
            output
                .assistant_message()
                .filter(|text| !text.trim().is_empty())
                .map(str::to_string)
        });
    combine_assistant_display_parts(assistant, terminal)
}

fn combine_assistant_display_parts(assistant: Option<String>, terminal: Option<String>) -> String {
    let assistant = assistant.filter(|text| !text.trim().is_empty());
    let terminal = terminal.filter(|text| !text.trim().is_empty());
    match (assistant, terminal) {
        (Some(assistant), Some(terminal)) if assistant.trim() == terminal.trim() => assistant,
        (Some(assistant), Some(terminal)) => format!("{}\n\n{}", assistant.trim_end(), terminal),
        (Some(assistant), None) => assistant,
        (None, Some(terminal)) => terminal,
        (None, None) => String::new(),
    }
}

fn terminal_value_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn compact_payload(value: Value) -> Value {
    match value {
        Value::String(text) if text.len() > 1_200 => Value::String(truncate_chars(&text, 1_200)),
        Value::Array(items) => {
            Value::Array(items.into_iter().take(12).map(compact_payload).collect())
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, compact_payload(value)))
                .collect(),
        ),
        other => other,
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    format!("{}...", text.chars().take(max_chars).collect::<String>())
}

fn work_item_from_observed(item: lash::process::ObservedWorkItem) -> WorkItem {
    WorkItem {
        process: work_process_from_observed(item.process),
        events: item
            .events
            .into_iter()
            .map(work_event_from_observed)
            .collect(),
        kind: item.kind,
        label: item.label,
    }
}

fn work_process_from_observed(process: lash::process::ObservedProcess) -> WorkProcess {
    WorkProcess {
        process_id: process.process_id,
        graph_key: process.graph_key,
        lifecycle: process.lifecycle,
        status_label: process.status_label,
        terminal: process.terminal,
        error: process.error,
        created_at_ms: process.created_at_ms,
        updated_at_ms: process.updated_at_ms,
        input: compact_payload(serde_json::to_value(process.input).unwrap_or(Value::Null)),
        external_ref: process
            .external_ref
            .and_then(|value| serde_json::to_value(value).ok())
            .map(compact_payload),
        child_session_id: process.child_session_id,
        label: process.label,
    }
}

fn work_event_from_observed(event: lash::process::ObservedProcessEvent) -> WorkEvent {
    WorkEvent {
        sequence: event.sequence,
        event_type: event.event_type,
        occurred_at_ms: event.occurred_at_ms,
        payload: compact_payload(event.payload),
    }
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    message: String,
    retryable: bool,
    terminal: bool,
}

impl AppError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            retryable: false,
            terminal: true,
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
            retryable: false,
            terminal: true,
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
            retryable: false,
            terminal: true,
        }
    }

    fn internal(message: impl std::fmt::Display) -> Self {
        eprintln!("agent-workbench internal request failure: {message}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal server error".to_string(),
            retryable: false,
            terminal: false,
        }
    }

    fn session_open(error: lash::EmbedError) -> Self {
        if let Some((session_id, context)) = deleted_session_details(&error) {
            log_deleted_session_refusal(session_id, context);
            return Self::conflict(deleted_session_message(session_id));
        }
        Self::internal(error)
    }

    #[allow(dead_code, reason = "production authorizers use this denial constructor")]
    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
            retryable: false,
            terminal: true,
        }
    }

    fn gateway_timeout(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            message: message.into(),
            retryable: false,
            terminal: false,
        }
    }

    fn runtime(error: lash::EmbedError) -> Self {
        let retryable = error.is_retryable();
        let terminal = error.is_terminal();
        debug_assert!(
            !(retryable && terminal),
            "an embed error cannot be both retryable and terminal: {error}"
        );
        if let Some((session_id, context)) = deleted_session_details(&error) {
            log_deleted_session_refusal(session_id, context);
            return Self {
                status: StatusCode::CONFLICT,
                message: deleted_session_message(session_id),
                retryable,
                terminal,
            };
        }
        eprintln!("agent-workbench runtime request failure: {error}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            retryable,
            terminal,
            message: "internal server error".to_string(),
        }
    }
}

fn deleted_session_details(error: &lash::EmbedError) -> Option<(&str, Option<&str>)> {
    let (source, context) = match error {
        lash::EmbedError::Store(source) => (source, None),
        lash::EmbedError::Session(lash::SessionError::Store { context, source }) => {
            (source, Some(context.as_str()))
        }
        lash::EmbedError::Runtime(error) => {
            return error.deleted_session_id().map(|session_id| (session_id, None));
        }
        _ => return None,
    };
    match source {
        lash::persistence::StoreError::SessionDeleted { session_id } => {
            Some((session_id.as_str(), context))
        }
        _ => None,
    }
}

fn deleted_session_message(session_id: &str) -> String {
    lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted {
        session_id: session_id.to_string(),
    })
    .to_string()
}

fn log_deleted_session_refusal(session_id: &str, context: Option<&str>) {
    eprintln!(
        "agent-workbench session admission refusal: session_id={session_id:?} \
         tombstone_outcome=\"retired\" outcome=\"refused\" store_context={context:?}"
    );
}

impl std::fmt::Display for AppError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": self.message,
            })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod app_error_tests {
    use super::*;

    #[test]
    fn deleted_session_open_is_a_comprehensible_conflict() {
        let error = AppError::session_open(lash::EmbedError::Store(
            lash::persistence::StoreError::SessionDeleted {
                session_id: "retired-session".to_string(),
            },
        ));
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.message, deleted_session_message("retired-session"));
    }

    #[tokio::test]
    async fn wrapped_session_deletion_is_a_comprehensible_conflict_response() {
        let session_id = "retired-during-runtime-binding";
        let error = AppError::session_open(lash::EmbedError::Session(
            lash_core::SessionError::Store {
                context: format!("failed to bind session `{session_id}` to its store"),
                source: lash::persistence::StoreError::SessionDeleted {
                    session_id: session_id.to_string(),
                },
            },
        ));
        let response = error.into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("read conflict response");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("decode conflict response"),
            json!({
                "error": deleted_session_message(session_id),
            })
        );
    }
}
