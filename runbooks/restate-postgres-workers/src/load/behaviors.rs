use super::*;
use crate::load::ReportedStatus;
use crate::load::behavior::{
    self, AuxiliaryEvidence, BehaviorReport, EditEvidence, FrameEvidence, HistoryEvidence,
    OccurrenceEvidence, PromotionEvidence,
};
use lash_restate::RestateControllerContext as _;

fn texts(session: &lash::LashSession) -> Vec<String> {
    session
        .read_view()
        .messages()
        .iter()
        .map(|message| {
            message
                .parts
                .iter()
                .map(|part| part.content().into_owned())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect()
}
fn frame(session: &lash::LashSession) -> String {
    session
        .read_view()
        .to_snapshot()
        .current_frame_node_id
        .map(|id| id.to_string())
        .unwrap_or_default()
}

async fn journal_read<T, F>(controller: &Controller<'_>, name: &str, future: F) -> HandlerResult<T>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Send + 'static,
    F: std::future::Future<Output = lash::Result<T>> + Send,
{
    let Json(result) = controller
        .context()
        .run_json_or_retry_send(name.into(), async move {
            match future.await {
                Ok(value) => Ok(Ok(value)),
                Err(error) if error.is_retryable() => Err(error.to_string()),
                Err(error) => Ok(Err(error.to_string())),
            }
        })
        .await?;
    result.map_err(terminal)
}

impl LoadWorker {
    async fn behavior_turn(
        &self,
        controller: &Controller<'_>,
        session: &lash::LashSession,
        run: &str,
        phase: &str,
    ) -> HandlerResult<InputOutcome> {
        let ctx = controller.context();
        let handle = session
            .send(TurnInput::text(format!(
                "{}{run}/{phase} {WORKLOAD_MARKER}{}",
                behavior::MARKER,
                self.load.sha256()
            )))
            .id(format!("load-{run}-behavior-{phase}"))
            .accept_restate(ctx)
            .await?;
        let outcome = input_outcome(handle.outcome_restate(ctx, RestateWait::new()).await?);
        if outcome.status != ReportedStatus::Answered {
            return Err(terminal(format!(
                "behavior {phase} did not answer: {}",
                outcome.outcome
            )));
        }
        Ok(outcome)
    }

    async fn external_event(
        &self,
        controller: &Controller<'_>,
        run: &str,
        suffix: &str,
    ) -> HandlerResult<OccurrenceEvidence> {
        let key = format!("{run}/behaviors/{suffix}");
        let source = json!({"schedule":format!("{run}/behaviors")});
        let receipt = self
            .core
            .triggers()
            .emit(
                lash::triggers::TriggerOccurrenceRequest::new(
                    behavior::EXTERNAL_SOURCE,
                    lash_core::facade_support::default_trigger_source_key(
                        behavior::EXTERNAL_SOURCE,
                        &source,
                    ),
                    json!({"schedule":format!("{run}/behaviors"),"tick":key}),
                    key.clone(),
                )
                .with_source(source),
                scoped(controller, &key, "emit")?,
            )
            .await
            .map_err(turn_handler_error)?;
        let ids = receipt.started_process_ids();
        let mut outputs = Vec::new();
        for id in &ids {
            let output = self
                .core
                .processes()
                .await_output(id)
                .await
                .map_err(turn_handler_error)?;
            let output = output.into_tool_output();
            if !output.is_success() {
                return Err(terminal(format!("external target {id} failed: {output:?}")));
            }
            outputs.push(output.into_value_for_projection());
        }
        Ok(OccurrenceEvidence {
            key,
            started: ids.iter().map(ToString::to_string).collect(),
            outputs,
        })
    }

    pub(super) async fn behaviors(
        &self,
        controller: &Controller<'_>,
        run: &str,
    ) -> HandlerResult<BehaviorReport> {
        let session_id = format!("load-{run}-behaviors");
        let session = create_or_open_session(&self.core, session_id.clone()).await?;
        let expected = behavior::prefill(&self.load, run).map_err(terminal_chain)?;
        let prefill = journal_read(controller, "load.prefill", async {
            if texts(&session).is_empty() {
                let messages = expected
                    .iter()
                    .enumerate()
                    .map(|(i, text)| {
                        lash_core::PluginMessage::text(
                            if i % 2 == 0 {
                                lash_core::MessageRole::User
                            } else {
                                lash_core::MessageRole::Assistant
                            },
                            text,
                        )
                    })
                    .collect();
                session.admin().state().append_messages(messages).await?;
            }
            let reopened = self.core.session(session_id.clone()).open().await?;
            Ok(HistoryEvidence {
                expected,
                reopened: texts(&reopened),
            })
        })
        .await?;
        self.behavior_turn(controller, &session, run, "seed")
            .await?;
        let admin = journal_read(controller, "load.admin-compaction", async {
            let before = frame(&self.core.session(session_id.clone()).open().await?);
            let applied = session
                .admin()
                .state()
                .compact_context(Some("load administrative compaction".into()))
                .await?;
            let reopened = self.core.session(session_id.clone()).open().await?;
            Ok(FrameEvidence {
                before,
                after: frame(&reopened),
                summary: texts(&reopened).first().cloned().unwrap_or_default(),
                applied,
            })
        })
        .await?;
        // Provider usage reaches half the 200,000-token context window; the
        // next admitted turn's pressure hook must open its own summary frame.
        self.behavior_turn(controller, &session, run, "pressure_usage")
            .await?;
        let before = journal_read(controller, "load.before-pressure", async {
            Ok(frame(&self.core.session(session_id.clone()).open().await?))
        })
        .await?;
        let outcome = self
            .behavior_turn(controller, &session, run, "pressure")
            .await?;
        let pressure = journal_read(controller, "load.pressure-frame", async {
            let reopened = self.core.session(session_id.clone()).open().await?;
            Ok(FrameEvidence {
                before,
                after: frame(&reopened),
                summary: texts(&reopened).first().cloned().unwrap_or_default(),
                applied: outcome.status == ReportedStatus::Answered,
            })
        })
        .await?;
        let auxiliary = self
            .behavior_turn(controller, &session, run, "auxiliary")
            .await?;
        let auxiliary = AuxiliaryEvidence {
            key: format!("{run}/behaviors/llm"),
            output: auxiliary.final_value["answer"]
                .as_str()
                .unwrap_or_default()
                .into(),
        };
        self.behavior_turn(controller, &session, run, "register")
            .await?;
        let external = self.external_event(controller, run, "event").await?;
        let process_id = external
            .started
            .first()
            .ok_or_else(|| terminal("external occurrence started no process"))?;
        let record = self
            .core
            .process_registry()
            .get_process(
                &process_id
                    .parse::<lash_core::ProcessId>()
                    .map_err(terminal)?,
            )
            .await
            .map_err(terminal)?
            .ok_or_else(|| terminal("promotion process record missing"))?;
        let lash_core::ProcessInput::Engine { kind, payload } = record.input.as_ref() else {
            return Err(terminal("promotion input is not Engine"));
        };
        let input: lash::process::LashlangProcessInput =
            serde_json::from_value(payload.clone()).map_err(terminal)?;
        let artifact = lashlang::LashlangArtifacts::of_backend(self.core.backend())
            .get_module_artifact(&input.module_ref)
            .await
            .map_err(terminal)?
            .ok_or_else(|| terminal("promotion module missing"))?;
        let promotion = PromotionEvidence {
            process_id: process_id.clone(),
            session_origin: matches!(&record.provenance.originator,lash_core::ProcessOriginator::Session {session_id:origin,..} if origin.as_str()==session_id),
            engine: kind.clone(),
            record_name: input.process_name,
            artifact_name: artifact
                .process_name_for_ref(&input.process_ref)
                .unwrap_or_default()
                .into(),
        };
        let edited = self
            .behavior_turn(controller, &session, run, "edit")
            .await?;
        let revision = edited.final_value["revision"].as_u64().unwrap_or_default();
        let listed = session
            .admin()
            .triggers()
            .by_source_type(behavior::EXTERNAL_SOURCE)
            .await
            .map_err(turn_handler_error)?;
        let listed_revision = listed
            .iter()
            .find(|entry| entry.subscription_key == "load-external")
            .map_or(0, |entry| entry.revision);
        let after_edit = self.external_event(controller, run, "edit").await?;
        self.behavior_turn(controller, &session, run, "delete")
            .await?;
        let after_delete = self.external_event(controller, run, "after-delete").await?;
        let edit = EditEvidence {
            revision,
            listed_revision,
            started: after_edit.started,
            outputs: after_edit.outputs,
            after_delete: after_delete.started,
        };
        Ok(BehaviorReport {
            prefill,
            admin,
            pressure,
            auxiliary,
            external,
            edit,
            promotion,
        })
    }
}
