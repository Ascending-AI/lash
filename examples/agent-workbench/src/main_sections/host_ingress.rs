//! What the workbench does with its own administrative authority: cancel a
//! process, reclaim a deleted session's finished work, and emit the
//! occurrences of its host trigger sources (the button and the mail world).
//!
//! Each act runs under a context the core's session administration mints for
//! one runtime operation, the same authority any host acting on external
//! ingress (a webhook, an operator control) uses. The engine then executes
//! what the act recorded: a cancel request is the process's mail, and an
//! occurrence's deliveries start their processes, which wake their sessions.
use super::*;
use lash::ProcessId;
use lash::SessionId;

impl AppState {
    /// A context for one runtime operation of this host, `operation`.
    async fn host_operation(
        &self,
        operation: String,
    ) -> Result<lash::runtime::ActorContext, AppError> {
        self.core
            .session_administration()
            .await
            .effect_host()
            .scoped(lash::runtime::AdmittedScope::runtime_operation(operation))
            // Audited: constructing this scope only validates the local runtime-operation id.
            .map_err(AppError::internal)
    }

    /// Request the cancellation of `process_id` on behalf of `session_id`.
    pub(crate) async fn cancel_process(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        operation_id: &str,
    ) -> Result<lash::process::ProcessCancelReceipt, AppError> {
        let operation = self
            .host_operation(format!("workbench-process-cancel:{operation_id}"))
            .await?;
        let receipt = self
            .core
            .processes()
            .cancel(process_id, operation)
            .await
            // Audited: process cancellation uses the global process registry and never consults a session tombstone.
            .map_err(AppError::internal)?;
        self.trace_for_session(
            session_id,
            "process.cancel_requested",
            json!({
                "operation_id": operation_id,
                "process_id": process_id,
                "receipt": receipt,
            }),
        );
        Ok(receipt)
    }

    /// Reclaim the terminal process rows `session_id` originated, as the
    /// retention half of deleting that session (FIG-989).
    ///
    /// Process rows are runtime-global and record their creating session only
    /// as provenance, so the session's close detaches them rather than
    /// deleting them. The bound is identity, not age: the originating session
    /// is gone and its awaits with it, so every one of its terminal rows is
    /// eligible once the tombstone commits. Live processes are untouched: the
    /// lever deletes only terminal rows. The workbench folds no process change
    /// feed into a store of its own, so no projection watermark guards it.
    pub(crate) async fn prune_processes_originated_by(
        &self,
        session_id: &SessionId,
    ) -> Result<lash::process::ProcessPruneReport, lash::EmbedError> {
        self.core
            .processes()
            .prune(
                u64::MAX,
                Some(&lash::process::ProcessListFilter {
                    status: lash::process::ProcessStatusFilter::Any,
                    originator: Some(lash::process::ProcessOriginatorFilter::session(
                        session_id.clone(),
                    )),
                    ..lash::process::ProcessListFilter::default()
                }),
                lash::process::ProjectionWatermark::NoProjector,
            )
            .await
        // Audited: process retention reads and writes the global registry and never consults a session tombstone.
    }

    /// Emit one press of `button` in `session_id` and publish the press as
    /// one row (FIG-5036).
    pub(crate) async fn emit_button_press(
        &self,
        session_id: &SessionId,
        button: ButtonChoice,
        pressed_at: String,
        operation_id: &str,
    ) -> Result<lash::triggers::TriggerEmitReport, AppError> {
        let payload = json!({
            "pressed_at": pressed_at,
            "button": button.as_str(),
            "message": format!("user pressed the {} button", button.lower()),
        });
        let report = self
            .emit_host_occurrence(
                session_id,
                HostTriggerSource {
                    resource_type: BUTTON_TRIGGER_RESOURCE,
                    alias: BUTTON_TRIGGER_ALIAS,
                    event: BUTTON_TRIGGER_EVENT,
                    source_type: BUTTON_TRIGGER_SOURCE_TYPE,
                },
                payload,
                format!("workbench-button-trigger:{operation_id}"),
            )
            .await?;
        self.trace_for_session(
            session_id,
            "button_trigger.trigger_occurrence",
            json!({
                "button": button,
                "occurrence_id": report.occurrence_id,
                "started_process_ids": report.started_process_ids(),
                "deliveries": trigger_delivery_trace(&report),
            }),
        );
        self.push_trigger_occurrence_for_session(
            session_id,
            format!("{} pressed", button.lower()),
            &report,
            pressed_at,
        );
        self.publish_trigger_dispatch_done(session_id, operation_id);
        Ok(report)
    }

    /// Emit the `mail.received` occurrence of `delivery` in `session_id` and
    /// publish it as one row.
    pub(crate) async fn emit_mail_received(
        &self,
        session_id: &SessionId,
        delivery: &mail::MailDelivery,
        operation_id: &str,
    ) -> Result<lash::triggers::TriggerEmitReport, AppError> {
        let payload = json!({
            "account": delivery.account,
            "title": delivery.title,
            "text": delivery.text,
        });
        let report = self
            .emit_host_occurrence(
                session_id,
                HostTriggerSource {
                    resource_type: MAIL_EVENT_RESOURCE,
                    alias: MAIL_EVENT_ALIAS,
                    event: MAIL_EVENT_EVENT,
                    source_type: MAIL_RECEIVED_SOURCE_TYPE,
                },
                payload,
                format!("workbench-mail-trigger:{operation_id}"),
            )
            .await?;
        self.trace_for_session(
            session_id,
            "mail_received.trigger_occurrence",
            json!({
                "account": delivery.account,
                "title": delivery.title,
                "occurrence_id": report.occurrence_id,
                "started_process_ids": report.started_process_ids(),
                "deliveries": trigger_delivery_trace(&report),
            }),
        );
        self.push_trigger_occurrence_for_session(
            session_id,
            format!("mail to inbox.{}: {}", delivery.account, delivery.title),
            &report,
            Utc::now().to_rfc3339(),
        );
        self.publish_trigger_dispatch_done(session_id, operation_id);
        Ok(report)
    }

    async fn emit_host_occurrence(
        &self,
        session_id: &SessionId,
        source: HostTriggerSource,
        payload: Value,
        idempotency_key: String,
    ) -> Result<lash::triggers::TriggerEmitReport, AppError> {
        let source_key = lash::triggers::empty_trigger_source_key(source.source_type)
            // Audited: an empty source key over a constant source type has no session identity.
            .map_err(AppError::internal)?;
        self.trace_for_session(
            session_id,
            "trigger.emit",
            json!({
                "resource_type": source.resource_type,
                "alias": source.alias,
                "event": source.event,
                "source_type": source.source_type,
                "source_key": source_key,
                "payload": payload.clone(),
            }),
        );
        // A delivery wakes the session with a run its engine starts on its
        // own; follow it so the page sees it.
        turns::watch_session_runs(self, session_id).await;
        let operation = self
            .host_operation(format!("trigger:{idempotency_key}"))
            .await?;
        self.core
            .triggers()
            .emit(
                lash::triggers::TriggerOccurrenceRequest::new(
                    source.source_type,
                    source_key,
                    payload,
                    idempotency_key,
                )
                .with_source(json!({}))
                .for_session(session_id),
                operation,
            )
            .await
            // Audited: trigger delivery consumes per-subscription failures into its report, so emission cannot return a typed session tombstone.
            .map_err(AppError::internal)
    }

    /// Publish the one row a host trigger occurrence shows (FIG-5036).
    ///
    /// The occurrence id is the row's id, so a repeated emission republishes
    /// the same row instead of adding a second one. The row is stamped `at`
    /// the moment the occurrence happened (a button's press), not when it was
    /// published, so the page places it where it happened.
    pub(crate) fn push_trigger_occurrence_for_session(
        &self,
        session_id: &SessionId,
        label: impl Into<String>,
        report: &lash::triggers::TriggerEmitReport,
        at: impl Into<String>,
    ) -> ChatMessage {
        self.push_prepared_message_for_session(
            session_id,
            ChatMessage {
                id: report.occurrence_id.clone(),
                role: "event".into(),
                text: label.into(),
                at: at.into(),
                attachments: Vec::new(),
                provenance: Some(ChatMessageProvenance::TriggerOccurrence {
                    occurrence_id: report.occurrence_id.clone(),
                    process_ids: report.started_process_ids(),
                }),
                client_nonce: None,
            },
        )
    }

    /// Clear the page's busy state when this dispatch owns it; a foreground
    /// turn's busy state survives a mid-turn occurrence.
    fn publish_trigger_dispatch_done(&self, session_id: &SessionId, operation_id: &str) {
        if self.active_turns.for_session(session_id).is_none() {
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
}

/// One of the workbench's host trigger sources, as its trace names it.
struct HostTriggerSource {
    resource_type: &'static str,
    alias: &'static str,
    event: &'static str,
    source_type: &'static str,
}

/// Every delivery a trigger occurrence produced, with the outcome and, for a
/// refusal, the typed code and reason: `started_process_ids()` leaves out a
/// delivery that failed to start.
pub(crate) fn trigger_delivery_trace(report: &lash::triggers::TriggerEmitReport) -> Value {
    json!(
        report
            .deliveries
            .iter()
            .map(|delivery| {
                let (outcome, code, reason) = match &delivery.outcome {
                    lash::triggers::TriggerDeliveryEmitOutcome::Started { .. } => {
                        ("started", None, None)
                    }
                    lash::triggers::TriggerDeliveryEmitOutcome::Failed { code, reason, .. } => {
                        ("failed", Some(code), Some(reason))
                    }
                };
                json!({
                    "subscription_id": delivery.subscription_id,
                    "process_id": delivery.process_id(),
                    "outcome": outcome,
                    "code": code,
                    "reason": reason,
                })
            })
            .collect::<Vec<_>>()
    )
}
