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

/// The external occurrence `{run}/behaviors/{suffix}`: its recorded emission,
/// then the terminal output of each process it started, awaited inside one
/// recorded step. A replay after a started process is pruned returns the
/// outputs the first attempt awaited instead of asking the registry again.
pub(super) async fn external_event(
    controller: &Controller<'_>,
    core: &lash::LashCore,
    run: &str,
    suffix: &str,
) -> HandlerResult<OccurrenceEvidence> {
    let key = format!("{run}/behaviors/{suffix}");
    let source = json!({"schedule":format!("{run}/behaviors")});
    let receipt = core
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
    let outputs = journal_read(controller, "load.external-outputs", async {
        let mut outputs = Vec::new();
        for id in &ids {
            let output = core.processes().await_output(id).await?.into_tool_output();
            if !output.is_success() {
                return Ok(Err(format!("external target {id} failed: {output:?}")));
            }
            outputs.push(output.into_value_for_projection());
        }
        Ok(Ok(outputs))
    })
    .await?
    .map_err(terminal)?;
    Ok(OccurrenceEvidence {
        key,
        started: ids.iter().map(ToString::to_string).collect(),
        outputs,
    })
}

/// The promotion readback of the process the external occurrence started:
/// its registry record and the module artifact its engine input names, read
/// with every refusal inside one recorded step. A replay after the process
/// is pruned, or its module released, returns the original evidence.
pub(super) async fn promotion_evidence(
    controller: &Controller<'_>,
    core: &lash::LashCore,
    session_id: &str,
    external: &OccurrenceEvidence,
) -> HandlerResult<PromotionEvidence> {
    let process_id = external
        .started
        .first()
        .ok_or_else(|| terminal("external occurrence started no process"))?
        .clone();
    journal_read(controller, "load.promotion", async move {
        let id = match process_id.parse::<lash_core::ProcessId>() {
            Ok(id) => id,
            Err(error) => return Ok(Err(error.to_string())),
        };
        let Some(record) = core.process_registry().get_process(&id).await? else {
            return Ok(Err("promotion process record missing".into()));
        };
        let lash_core::ProcessInput::Engine { kind, payload } = record.input.as_ref() else {
            return Ok(Err("promotion input is not Engine".into()));
        };
        let input: lash::process::LashlangProcessInput =
            match serde_json::from_value(payload.clone()) {
                Ok(input) => input,
                Err(error) => return Ok(Err(error.to_string())),
            };
        let Some(artifact) = lashlang::LashlangArtifacts::of_backend(core.backend())
            .get_module_artifact(&input.module_ref)
            .await
            .map_err(lash_core::PluginError::from)?
        else {
            return Ok(Err("promotion module missing".into()));
        };
        Ok(Ok(behavior::promotion(
            process_id,
            matches!(&record.provenance.originator,lash_core::ProcessOriginator::Session {session_id:origin,..} if origin.as_str()==session_id),
            kind.clone(),
            &record.identity,
            &input,
            &artifact,
        )))
    })
    .await?
    .map_err(terminal)
}

/// The revision the session's `load-external` subscription lists, read with
/// the session's acquisition inside one recorded step. A replay after the
/// session is deleted or the subscription advances returns the original
/// revision.
pub(super) async fn listed_trigger_revision(
    controller: &Controller<'_>,
    core: &lash::LashCore,
    session_id: &str,
) -> HandlerResult<u64> {
    journal_read(controller, "load.trigger-revision", async {
        let listed = core
            .session(session_id.to_string())
            .open()
            .await?
            .admin()
            .triggers()
            .by_source_type(behavior::EXTERNAL_SOURCE)
            .await?;
        Ok(listed
            .iter()
            .find(|entry| entry.subscription_key == "load-external")
            .map_or(0, |entry| entry.revision))
    })
    .await
}

impl LoadWorker {
    async fn behavior_turn(
        &self,
        controller: &Controller<'_>,
        session: &lash::DurableSession,
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

    pub(super) async fn behaviors(
        &self,
        controller: &Controller<'_>,
        run: &str,
    ) -> HandlerResult<BehaviorReport> {
        let session_id = format!("load-{run}-behaviors");
        let session =
            journaled_session(controller.context(), &self.core, session_id.clone()).await?;
        let expected = behavior::prefill(&self.load, run).map_err(terminal_chain)?;
        let prefill = journal_read(controller, "load.prefill", async {
            // The live session is opened only inside journaled steps: a
            // replay reads their answers back and opens nothing.
            let session = self.core.session(session_id.clone()).open().await?;
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
            let session = self.core.session(session_id.clone()).open().await?;
            let before = frame(&session);
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
        let external = external_event(controller, &self.core, run, "event").await?;
        let promotion = promotion_evidence(controller, &self.core, &session_id, &external).await?;
        let edited = self
            .behavior_turn(controller, &session, run, "edit")
            .await?;
        let revision = edited.final_value["revision"].as_u64().unwrap_or_default();
        let listed_revision = listed_trigger_revision(controller, &self.core, &session_id).await?;
        let after_edit = external_event(controller, &self.core, run, "edit").await?;
        self.behavior_turn(controller, &session, run, "delete")
            .await?;
        let after_delete = external_event(controller, &self.core, run, "after-delete").await?;
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

#[cfg(test)]
#[path = "behaviors_replay_tests.rs"]
mod replay_tests;
