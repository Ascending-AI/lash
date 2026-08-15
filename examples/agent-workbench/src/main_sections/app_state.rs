
impl AppState {
    /// Opens a session, asking for the ambient dialect and accepting the one
    /// already recorded.
    ///
    /// A dialect becomes durable at the session's first *commit*, not at open.
    /// An earlier version of this applied `LASH_RUNBOOK_DIALECT` only on the
    /// two call sites that create, and both of those open and drop without
    /// running a turn — so the pin evaporated with the handle and the first
    /// real turn, opening with no dialect and finding nothing recorded,
    /// committed `lashlang` permanently. A workbench told to serve TypeScript
    /// served Lashlang.
    ///
    /// So every open asks, and a session that already recorded a different
    /// dialect keeps its own: asking is what makes the pin land at the first
    /// commit, and accepting the recorded answer is what stops a store from an
    /// earlier row failing every route. Observably this is still create-only —
    /// the ambient value can only take effect on a session that has recorded
    /// nothing.
    ///
    /// Which dialect is asked for is per session, not per process: a session
    /// the operator created with a dialect asks for that one for the rest of
    /// its life (FIG-1306), and a session the roster does not know asks for the
    /// ambient `LASH_RUNBOOK_DIALECT`. Same mechanism, one source of the answer.
    fn session_builder(&self, session_id: impl Into<String>) -> lash::SessionBuilder {
        use lash::rlm::RlmSessionBuilderExt as _;

        let session_id = session_id.into();
        let dialect = self.requested_dialect(&session_id);
        let builder = self.core.session(session_id);
        match dialect {
            lash::rlm::RlmDialect::Lashlang => builder,
            lash::rlm::RlmDialect::Typescript => builder
                .rlm_dialect(lash::rlm::RlmDialect::Typescript)
                .expect("the typed TypeScript session option must serialize"),
        }
    }

    /// The dialect this session is opened with: its roster row's, or the
    /// ambient default for a session the roster never recorded.
    fn requested_dialect(&self, session_id: &str) -> lash::rlm::RlmDialect {
        self.sessions
            .dialect_for(session_id)
            .unwrap_or(self.rlm_dialect)
    }

    /// The dialect this session *recorded*, which is the one every label reads.
    ///
    /// A session that has committed nothing has recorded nothing, and reads as
    /// the ambient default; what it will be opened with is the honest answer
    /// for it, so that is what a fresh session's badge shows.
    async fn recorded_dialect(&self, session_id: &str) -> lash::rlm::RlmDialect {
        use lash::rlm::RlmSessionReadViewExt as _;

        let Ok(session) = self.open_session(session_id).await else {
            return self.requested_dialect(session_id);
        };
        let read_view = session.read_view();
        // Absence is Lashlang for a session that ran, and unknown for one that
        // has not committed yet — reading the default back for a session
        // created as TypeScript would badge it as the dialect it is about to
        // stop being. So absence defers to what the next open will ask for.
        let recorded = read_view
            .protocol_turn_options()
            .payload
            .get("dialect")
            .is_some()
            .then(|| read_view.rlm_dialect());
        drop(session);
        recorded.unwrap_or_else(|| self.requested_dialect(session_id))
    }

    /// Opens through [`Self::session_builder`], falling back to the recorded
    /// dialect when this session was pinned to a different one.
    ///
    /// The fallback is what keeps a carried-over store from failing every
    /// route; `runbooks/RULES.md` still requires a fresh data directory per
    /// parity row, for evidence purity rather than to avoid an error.
    async fn open_session(&self, session_id: &str) -> Result<lash::LashSession, lash::EmbedError> {
        match self.session_builder(session_id.to_string()).open().await {
            Ok(session) => Ok(session),
            Err(error) if is_dialect_pin_conflict(&error) => {
                self.core.session(session_id.to_string()).open().await
            }
            Err(error) => Err(error),
        }
    }

    fn current_session_id(&self) -> String {
        self.sessions.current()
    }

    fn selected_model(&self) -> ModelSelection {
        self.selected_model
            .lock_recover()
            .clone()
    }

    fn set_selected_model(&self, model: ModelSelection) {
        *self.selected_model.lock_recover() = model;
    }

    /// The settings panel for one session, labelled with the dialect that
    /// session *recorded* rather than the one this process is configured with —
    /// the two differ exactly when the label matters (FIG-1306, ADR 0063).
    fn settings_for_session(
        &self,
        session_id: String,
        rlm_dialect: lash::rlm::RlmDialect,
    ) -> Settings {
        let selected_model = self.selected_model();
        Settings {
            model: selected_model.model,
            model_variant: selected_model.model_variant,
            web_configured: self.web_configured,
            model_variants: vec!["", "low", "medium", "high"],
            session_name: self
                .sessions
                .entry(&session_id)
                .map(|entry| entry.name)
                .unwrap_or_else(|| session_id.clone()),
            rlm_dialect: rlm_dialect.language_id(),
            session_id,
        }
    }

    #[cfg(test)]
    fn messages_snapshot(&self) -> Vec<ChatMessage> {
        self.messages.lock_recover().clone()
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
                outcome: TurnDoneOutcome::Completed,
            },
        );
    }

    /// Report a turn that never reached an outcome of its own: retire the rows
    /// the workbench published for it, render one failure row, and close it out
    /// as failed.
    ///
    /// Order is the contract (FIG-1000). The retirement runs first so no viewer
    /// can read the failure and still find the retired row behind it, and the
    /// `Failed` outcome runs last so a viewer that already rendered those rows
    /// knows to re-derive from the authoritative snapshot instead of keeping a
    /// phantom whose commit was refused.
    fn publish_turn_failed(&self, session_id: &str, turn_id: &str) {
        let retired = self.event_tx.retire_turn_rows(session_id, turn_id);
        if !retired.is_empty() {
            self.messages
                .lock_recover()
                .retain(|message| !retired.contains(&message.id));
        }
        self.push_message_with_id_for_session(
            session_id,
            format!("turn:{turn_id}:failed"),
            "event",
            PUBLIC_TURN_FAILURE_MESSAGE,
        );
        self.publish_for_session_identified(
            session_id,
            format!("turn:{turn_id}:done"),
            StreamItem::Done {
                turn_id: Some(turn_id.to_string()),
                outcome: TurnDoneOutcome::Failed,
            },
        );
    }

    fn publish_trigger_dispatch_done(&self, session_id: &str, operation_id: &str) {
        if self.active_turns.for_session(session_id).is_empty() {
            self.publish_for_session_identified(
                session_id,
                format!("operation:{operation_id}:done"),
                StreamItem::Done {
                    turn_id: None,
                    outcome: TurnDoneOutcome::Completed,
                },
            );
        }
    }

    #[cfg(test)]
    fn track_turn(&self, session_id: &str, turn_id: &str) {
        self.active_turns.insert(session_id, turn_id);
    }

    fn track_turn_prompt(
        &self,
        session_id: &str,
        turn_id: &str,
        prompt: String,
        attachment_id: Option<String>,
    ) {
        self.active_turns
            .insert_with_prompt(session_id, turn_id, Some(prompt), attachment_id);
    }

    /// Delete `session_id`, reclaim the finished work it left behind, and report
    /// what the reclamation removed.
    ///
    /// Both halves are the workbench's session-deletion contract, so they live
    /// in one place: the runtime delete retires the session, and the retention
    /// lever below reclaims the globally-owned process rows the delete
    /// deliberately only detaches.
    async fn delete_session_and_reclaim_processes(
        &self,
        session_id: &str,
        scoped_effect_controller: lash::runtime::ScopedEffectController<'_>,
    ) -> Result<lash::process::ProcessPruneReport, AppError> {
        let report = self
            .core
            .delete_session(session_id, scoped_effect_controller)
            .await
            // Audited: delete_session lowers component and factory failures to non-tombstone EmbedError variants.
            .map_err(AppError::internal)?;
        let retention = self.prune_processes_originated_by(session_id).await?;
        self.trace_for_session(
            session_id,
            "reset.restate.session_deleted",
            json!({
                "session_id": session_id,
                "report": report,
                "process_retention": {
                    "pruned_processes": retention.pruned_processes,
                    "pruned_events": retention.pruned_events,
                    "pruned_trigger_deliveries": retention.pruned_trigger_deliveries,
                },
            }),
        );
        Ok(retention)
    }

    /// Reclaim the terminal process rows `session_id` originated, as the
    /// retention half of deleting that session.
    ///
    /// Deleting a session deliberately detaches rather than deletes its process
    /// state: rows in the process registry are runtime-global and record the
    /// creating session only as provenance, so the delete discards the wakes
    /// aimed at the session and drops its observer edges while leaving the rows
    /// themselves. Reclaiming them is this separate host lever, and a host that
    /// never pulls it accumulates every deleted session's finished work in the
    /// runtime-wide registry the work rail reads.
    ///
    /// Two choices this workbench has to make explicitly:
    ///
    /// * No age bound. Retention normally protects rows a late
    ///   `await_terminal` could still replay, and age is the host's proxy for
    ///   "nobody is coming back for this". Here the bound is identity, not age:
    ///   the originating session is gone, its awaits were revoked before the
    ///   delete, and its observer edges no longer exist, so every one of its
    ///   terminal rows is eligible the moment the delete commits. A `now`
    ///   cutoff would express the same set while also needing a wall-clock read
    ///   inside a durable workflow handler.
    /// * [`ProjectionWatermark::NoProjector`](lash::process::ProjectionWatermark::NoProjector).
    ///   The workbench installs a best-effort
    ///   [`ProcessEventSink`](lash::process::ProcessEventSink) that pushes
    ///   events straight to the UI stream for freshness (ADR 0017), and reads
    ///   the registry live for every rail render. It never folds the process
    ///   change feed into a store of its own, so it holds no acknowledged
    ///   cursor and has no unprojected history for a watermark to protect.
    ///
    /// Live processes are untouched by construction: the lever only ever
    /// deletes terminal rows, and processes this session started that are still
    /// running deliberately outlive it as global work. This is one reclamation
    /// at delete time, not a standing sweep, so work that was live at the delete
    /// and terminates afterwards stays observable on the rail — the property the
    /// `workbench-process-lifecycle` runbook judges.
    async fn prune_processes_originated_by(
        &self,
        session_id: &str,
    ) -> Result<lash::process::ProcessPruneReport, AppError> {
        self.core
            .processes()
            .prune(
                u64::MAX,
                Some(&lash::process::ProcessListFilter {
                    status: lash::process::ProcessStatusFilter::Any,
                    originator_id: Some(session_id.to_string()),
                    ..lash::process::ProcessListFilter::default()
                }),
                lash::process::ProjectionWatermark::NoProjector,
            )
            .await
            // Audited: process retention reads and writes the global registry and never consults a session tombstone.
            .map_err(AppError::internal)
    }

    /// Fan out exact-address cooperative cancellation to the active turns the
    /// UI submitted for `session_id`.
    async fn cancel_turns_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<TurnCancelReceipt>, AppError> {
        let active = self.active_turns.for_session(session_id);
        let session = self
            .open_session(session_id)
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
                // Audited: revoked turn-cancel gates become an UnknownOrRevoked outcome; remaining failures are untyped control errors.
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
                    Err(err)
                        if err.code
                            == lash::runtime::RuntimeErrorCode::TurnTerminalAwaitTimeout =>
                    {
                        (None, Some(err))
                    }
                    // Audited: terminal attachment lowers Restate transport and revocation failures to RuntimeError without a tombstone cause.
                    Err(err) => return Err(AppError::internal(err.to_string())),
                }
            } else {
                (None, None)
            };
            let routing_retained = if terminal.is_none()
                && terminal_error
                    .as_ref()
                    .is_some_and(|err| {
                        err.code == lash::runtime::RuntimeErrorCode::TurnTerminalAwaitTimeout
                    })
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
                StreamItem::Done {
                    turn_id: None,
                    outcome: TurnDoneOutcome::Completed,
                },
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
        self.push_message_with_id_and_attachments_for_session(
            session_id,
            id,
            role,
            text,
            Vec::new(),
        )
    }

    fn push_message_with_id_and_attachments_for_session(
        &self,
        session_id: &str,
        id: impl Into<String>,
        role: impl Into<String>,
        text: impl Into<String>,
        attachments: Vec<ChatAttachment>,
    ) -> ChatMessage {
        let message = ChatMessage {
            id: id.into(),
            role: role.into(),
            text: text.into(),
            at: Utc::now().to_rfc3339(),
            attachments,
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
                .lock_recover()
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

/// One row of the workbench's durable session roster.
///
/// `dialect` is the dialect the session was *created with*, which is what every
/// later open has to ask for again — the pin only becomes durable at the
/// session's first commit, so a roster that forgot it would let the ambient
/// default overwrite an operator's choice on the very first turn. What a
/// session actually *recorded* is read back from its own read view, never from
/// this row (FIG-1306).
#[derive(Clone, Debug, Deserialize, Serialize)]
struct WorkbenchSessionEntry {
    session_id: String,
    /// The operator's name for this session, or the id when they gave none.
    name: String,
    dialect: lash::rlm::RlmDialect,
    created_at_ms: i64,
    last_active_ms: i64,
}

/// The sessions this workbench knows about, and which one is current.
///
/// Two durable files, because they answer two questions and the first is
/// load-bearing for every driver in the battery: `session-id` stays exactly
/// what it was — the plain-text id a query-less `/api/` call resolves to, which
/// the runbooks read and write directly — and `sessions.json` beside it is the
/// roster the session list renders, one row per session with the dialect it was
/// created with.
///
/// A session the roster does not know still resolves: it is served on the
/// ambient `LASH_RUNBOOK_DIALECT`, which is how every pre-roster deployment and
/// every ad-hoc `?session_id=` tab reads.
#[derive(Clone, Debug)]
struct WorkbenchSessions {
    current: Arc<Mutex<String>>,
    path: Option<Arc<PathBuf>>,
    roster: Arc<Mutex<BTreeMap<String, WorkbenchSessionEntry>>>,
    roster_path: Option<Arc<PathBuf>>,
}

impl WorkbenchSessions {
    #[cfg(test)]
    fn fresh() -> Self {
        Self {
            current: Arc::new(Mutex::new(new_session_id())),
            path: None,
            roster: Arc::new(Mutex::new(BTreeMap::new())),
            roster_path: None,
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
        let roster_path = path.with_file_name(SESSION_ROSTER_FILE_NAME);
        let roster = match std::fs::read(&roster_path) {
            Ok(bytes) => serde_json::from_slice::<Vec<WorkbenchSessionEntry>>(&bytes)
                .with_context(|| format!("decode workbench sessions `{}`", roster_path.display()))?
                .into_iter()
                .map(|entry| (entry.session_id.clone(), entry))
                .collect(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("read workbench sessions `{}`", roster_path.display())
                });
            }
        };
        let ids = Self {
            current: Arc::new(Mutex::new(current)),
            path: Some(Arc::new(path)),
            roster: Arc::new(Mutex::new(roster)),
            roster_path: Some(Arc::new(roster_path)),
        };
        ids.persist();
        Ok(ids)
    }

    fn current(&self) -> String {
        self.current.lock_recover().clone()
    }

    fn rotate(&self) -> (String, String) {
        let mut current = self.current.lock_recover();
        let old = current.clone();
        let new = new_session_id();
        *current = new.clone();
        drop(current);
        self.persist();
        // A reset replaces the session behind the same roster slot, so the new
        // id inherits the retired one's name and dialect: an operator who
        // created a TypeScript session and pressed reset is still in one.
        let carried = self.roster.lock_recover().get(&old).cloned();
        if let Some(carried) = carried {
            self.record(new.clone(), carried.name, carried.dialect);
            self.forget(&old);
        }
        (old, new)
    }

    /// Add a session to the roster, or refresh the row of one already there.
    fn record(
        &self,
        session_id: String,
        name: String,
        dialect: lash::rlm::RlmDialect,
    ) -> WorkbenchSessionEntry {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut roster = self.roster.lock_recover();
        let entry = roster
            .entry(session_id.clone())
            .and_modify(|entry| {
                entry.name = name.clone();
                entry.last_active_ms = now_ms;
            })
            .or_insert(WorkbenchSessionEntry {
                session_id,
                name,
                dialect,
                created_at_ms: now_ms,
                last_active_ms: now_ms,
            })
            .clone();
        self.persist_roster(&roster);
        entry
    }

    /// Register a session the roster has not seen, keeping any row it has.
    ///
    /// This is how the boot session joins the roster: its dialect is the
    /// ambient one, and a row that already exists wins, because that row is
    /// what the session's durable pin was created from.
    fn ensure(&self, session_id: &str, dialect: lash::rlm::RlmDialect) {
        if self.roster.lock_recover().contains_key(session_id) {
            return;
        }
        self.record(session_id.to_string(), session_id.to_string(), dialect);
    }

    fn forget(&self, session_id: &str) {
        let mut roster = self.roster.lock_recover();
        if roster.remove(session_id).is_some() {
            self.persist_roster(&roster);
        }
    }

    fn touch(&self, session_id: &str) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut roster = self.roster.lock_recover();
        let Some(entry) = roster.get_mut(session_id) else {
            return;
        };
        entry.last_active_ms = now_ms;
        self.persist_roster(&roster);
    }

    /// A row for a session the roster never recorded, so the selector can show
    /// it without the read side writing to the roster.
    fn unrostered_entry(
        &self,
        session_id: String,
        dialect: lash::rlm::RlmDialect,
    ) -> WorkbenchSessionEntry {
        WorkbenchSessionEntry {
            name: session_id.clone(),
            session_id,
            dialect,
            created_at_ms: 0,
            last_active_ms: 0,
        }
    }

    fn entry(&self, session_id: &str) -> Option<WorkbenchSessionEntry> {
        self.roster.lock_recover().get(session_id).cloned()
    }

    /// The dialect this session must be opened with, if the roster knows it.
    fn dialect_for(&self, session_id: &str) -> Option<lash::rlm::RlmDialect> {
        self.entry(session_id).map(|entry| entry.dialect)
    }

    /// The roster, oldest first, which is the order the selector renders.
    fn list(&self) -> Vec<WorkbenchSessionEntry> {
        let mut entries = self
            .roster
            .lock_recover()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        entries
    }

    /// Make a rostered session the one a query-less API call resolves to.
    ///
    /// Selection is durable for the same reason the boot id is: a reload, a
    /// restart, and the drivers that read `<data-dir>/session-id` must all
    /// agree on which session the workbench is serving.
    fn select(&self, session_id: &str) -> Option<WorkbenchSessionEntry> {
        let entry = self.entry(session_id)?;
        *self.current.lock_recover() = session_id.to_string();
        self.persist();
        self.touch(session_id);
        Some(entry)
    }

    fn persist_roster(&self, roster: &BTreeMap<String, WorkbenchSessionEntry>) {
        let Some(path) = self.roster_path.as_deref() else {
            return;
        };
        let entries = roster.values().cloned().collect::<Vec<_>>();
        let encoded =
            serde_json::to_vec_pretty(&entries).expect("workbench session roster serializes");
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, encoded)
            .unwrap_or_else(|err| panic!("write session roster `{}`: {err}", temporary.display()));
        std::fs::rename(&temporary, path).unwrap_or_else(|err| {
            panic!(
                "replace session roster `{}` from `{}`: {err}",
                path.display(),
                temporary.display()
            )
        });
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
    selected_model: &ModelSelection,
    model: Option<&str>,
    model_variant: Option<&str>,
) -> Result<lash::ModelSpec, AppError> {
    let model = model
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(selected_model.model.as_str())
        .to_string();
    let model_variant = model_variant_for_request(selected_model, model_variant);
    lash::ModelSpec::builder(model)
        .variant(model_variant
            .map(lash::provider::ReasoningSelection::Effort)
            .unwrap_or_default())
        .context_window_tokens(workbench_context_window_tokens())
        .build()
        .map(with_workbench_model_capability)
        .map_err(|error| AppError::bad_request(error.to_string()))
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
    lash::ModelSpec::builder(selection.model)
        .variant(selection
            .model_variant
            .map(lash::provider::ReasoningSelection::Effort)
            .unwrap_or_default())
        .context_window_tokens(workbench_context_window_tokens())
        .build()
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
        // Audited: session configuration updates only resident state and its current implementation is infallible.
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

/// Whether the reply this turn produced is the workbench's to commit.
///
/// The runtime owns a turn's assistant output. A turn that finishes *as* an
/// assistant message has already had that text committed once — by the protocol
/// during the turn, or by the turn boundary materializing the terminal output —
/// so a workbench copy on top of it would put the same reply in the durable
/// transcript twice. That is what a background wake turn did: a queued turn runs
/// without `require_finish`, so a prose-only reply terminates naturally into
/// `TurnFinish::AssistantMessage` (FIG-984). The regime is the termination, not
/// the path: any turn ending in bare prose reaches it.
///
/// A turn that finishes with a terminal *value* is not an assistant message.
/// `require_finish` — which the send path applies — forces the answer through
/// `finish`, and the runtime deliberately keeps that value out of the
/// conversation. The reply the workbench renders is then the workbench's own to
/// commit, so resume and `/api/state` still read it from durable truth.
fn workbench_owns_committed_agent_reply(output: &TurnResult) -> bool {
    output.assistant_message().is_none()
}

/// Commit the reply the workbench renders as this turn's durable assistant
/// message. Only for turns `workbench_owns_committed_agent_reply` claims.
pub(crate) async fn commit_assistant_transcript(
    session: &lash::LashSession,
    turn_id: &str,
    assistant_text: String,
    model: Option<&str>,
) -> Result<(), AppError> {
    let message_id = workbench_turn_assistant_message_id(turn_id);
    let already_committed = session
        .read_view()
        .messages()
        .iter()
        .any(|message| message.id == message_id);
    if already_committed {
        return Ok(());
    }
    let mut message = lash::plugins::PluginMessage::text(
        lash::messages::MessageRole::Assistant,
        assistant_text.clone(),
    )
    .with_id(message_id.clone());
    if let Some((turn, model)) = replay_route_committed_reply(&assistant_text, model) {
        // The deterministic replay-route fixture must cross the same durable
        // transcript seam as a resumed production turn. Keep its visible text
        // ordinary, while retaining provider-owned replay state in a hidden
        // reasoning part for the next request's route filter to inspect.
        message.content.clear();
        message.parts = vec![
            lash_core::Part::text(format!("{message_id}.p0"), assistant_text, None),
            lash_core::Part::reasoning(
                format!("{message_id}.p1"),
                format!("FIG-1374 portable reasoning {turn}"),
                Some(lash_core::llm::types::ProviderReasoningReplay {
                    signature: Some(format!("FIG1374-OPAQUE-REPLAY-{turn}")),
                    origin: Some(lash_core::ProviderRouteIdentity::new(
                        "workbench-dev-failure",
                        "workbench-dev-failure",
                        model,
                    )),
                    ..Default::default()
                }),
            ),
        ];
    }
    session
        .admin()
        .state()
        .append_messages(vec![message])
        .await
        .map_err(AppError::runtime)
}

fn replay_route_committed_reply<'a>(
    assistant_text: &str,
    model: Option<&'a str>,
) -> Option<(usize, &'a str)> {
    let model = model.filter(|model| model.starts_with("dev/replay-route-"))?;
    let turn = assistant_text
        .strip_prefix("FIG-1374 replay-route response ")?
        .parse()
        .ok()?;
    Some((turn, model))
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
        lash::EmbedError::Plugin(lash::plugins::PluginError::RuntimeEffectController(error)) => {
            return match error.cause.as_ref() {
                Some(lash::runtime::RuntimeErrorCause::SessionDeleted { session_id }) => {
                    Some((session_id.as_str(), Some(error.code.as_str())))
                }
                None => None,
            };
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
            lash::SessionError::Store {
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

/// Whether opening a session failed because it already recorded a different
/// dialect, as opposed to failing for any other reason.
///
/// Matched on the message because the pin lives in the protocol plugin and
/// surfaces as a protocol error. A wrong answer here can only make a genuinely
/// broken open retry once without the dialect and fail again.
fn is_dialect_pin_conflict(error: &lash::EmbedError) -> bool {
    error.to_string().contains("RLM dialect is durably pinned")
}
