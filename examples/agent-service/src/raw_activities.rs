use lash::sync::MutexExt;
use std::convert::Infallible;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use lash::rlm::RlmTurnBuilderExt as _;
use lash::{TurnActivityFanout, TurnActivitySink, TurnInput, TurnOutput};
use lash_remote_protocol::RemoteTurnActivitySink;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::routes::{
    ChannelTurnEvents, TurnPersistenceState, assistant_text_for_persistence,
    model_spec_for_chat_selection,
};
#[cfg(feature = "restate")]
use crate::state::AgentServiceDurability;
use crate::state::{AppError, AppResult, AppStateData};

#[derive(Debug, Deserialize)]
pub(crate) struct StreamRawActivitiesRequest {
    text: String,
}

/// Run one chat turn and expose its canonical remote activity projection as
/// newline-delimited JSON. This is a raw transport endpoint: unlike the
/// product message stream, it carries no app-owned rows or replay envelope.
/// In Restate mode, use the app-owned turn outbox keyed by `turn_id` that the
/// progress stream reads. The fanout invokes the remote sink before the
/// persistence sink, so clients must not read app rows as soon as they see an
/// activity.
pub(crate) async fn stream_raw_activities(
    State(state): State<AppStateData>,
    AxumPath(chat_id): AxumPath<String>,
    Json(request): Json<StreamRawActivitiesRequest>,
) -> AppResult<Response> {
    #[cfg(feature = "restate")]
    if state.durability() == AgentServiceDurability::Restate {
        return Err(AppError {
            status: StatusCode::NOT_IMPLEMENTED,
            message: "raw turn activities are unavailable in Restate mode; use the app-owned turn outbox keyed by turn_id that the progress stream reads"
                .to_string(),
        });
    }

    let text = request.text.trim().to_string();
    if text.is_empty() {
        return Err(AppError::bad_request("message text is required"));
    }
    let model_selection = state
        .with_db({
            let chat_id = chat_id.clone();
            let text = text.clone();
            move |db| {
                db.require_chat(&chat_id)?;
                let model_selection = db.chat_model_selection(&chat_id)?;
                let board = db.chat_board(&chat_id)?;
                db.maybe_title_from_first_message(&chat_id, &text)?;
                db.insert_message_with_payload(
                    &chat_id,
                    "user",
                    &text,
                    Some(json!({ "board": board })),
                )?;
                Ok(model_selection)
            }
        })
        .await?;

    let turn_model = model_spec_for_chat_selection(&model_selection)?;
    let session = state.open_session(&chat_id, turn_model).await?;
    let turn_id = format!("agent-service-raw-turn:{}", uuid::Uuid::new_v4());
    let turn = session
        .turn(TurnInput::text(text))
        .turn_id(turn_id.clone())
        .require_finish()?;
    let (tx, rx) = mpsc::unbounded_channel::<Result<Bytes, Infallible>>();
    let remote_events = Arc::new(RemoteTurnActivitySink::new(NdjsonChannelWriter::new(tx), 0));
    let turn_state = Arc::new(Mutex::new(TurnPersistenceState::default()));
    let persistence_events = Arc::new(ChannelTurnEvents::persistence(
        state.clone(),
        chat_id.clone(),
        Arc::clone(&turn_state),
    ));
    let events = TurnActivityFanout::new([
        Arc::clone(&remote_events) as Arc<dyn TurnActivitySink>,
        persistence_events as Arc<dyn TurnActivitySink>,
    ]);

    tokio::spawn(async move {
        match turn.stream_to(&events).await {
            Ok(result) => {
                let output = TurnOutput {
                    result,
                    activities: Vec::new(),
                };
                let assistant_text = assistant_text_for_persistence(
                    &output,
                    turn_state.lock_recover().assistant_prose(),
                );
                if let Err(error) = state
                    .with_db(move |db| db.insert_message(&chat_id, "assistant", &assistant_text))
                    .await
                {
                    eprintln!("agent-service raw activity persistence failed: {error}");
                }
            }
            Err(error) => eprintln!("agent-service raw activity turn failed: {error}"),
        }
        let errors = remote_events.take_errors();
        if !errors.is_empty() {
            eprintln!(
                "agent-service raw activity stream write failed: {}",
                errors.join("; ")
            );
        }
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-lash-turn-id", turn_id)
        .body(Body::from_stream(UnboundedReceiverStream::new(rx)))
        .expect("valid raw activity streaming response"))
}

struct NdjsonChannelWriter {
    tx: mpsc::UnboundedSender<Result<Bytes, Infallible>>,
    pending: Vec<u8>,
}

impl NdjsonChannelWriter {
    fn new(tx: mpsc::UnboundedSender<Result<Bytes, Infallible>>) -> Self {
        Self {
            tx,
            pending: Vec::new(),
        }
    }
}

impl Write for NdjsonChannelWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.tx.is_closed() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "HTTP stream closed",
            ));
        }
        self.pending.extend_from_slice(bytes);
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let remainder = self.pending.split_off(newline + 1);
            let line = std::mem::replace(&mut self.pending, remainder);
            self.tx
                .send(Ok(Bytes::from(line)))
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "HTTP stream closed"))?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use lash::LashCore;
    use lash::direct::LlmOutputPart;
    use lash::provider::LlmResponse;
    use lash_remote_protocol::{RemoteTurnActivity, RemoteTurnEvent};

    use super::*;
    use crate::db::AppDb;
    use crate::state::AgentServiceDurability;

    #[tokio::test]
    async fn raw_activity_route_streams_framed_activities_from_a_real_turn() {
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path();
        let state = raw_activity_test_state(data_dir, AgentServiceDurability::Local).await;
        let chat = state
            .with_db(|db| db.create_chat("raw activities", "scripted-model", None))
            .await
            .expect("create chat");
        let chat_id = chat.id;

        let response = stream_raw_activities(
            State(state.clone()),
            AxumPath(chat_id.clone()),
            Json(StreamRawActivitiesRequest {
                text: "exercise raw activity transport".to_string(),
            }),
        )
        .await
        .expect("raw activity endpoint");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/x-ndjson; charset=utf-8"
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        assert!(body.ends_with(b"\n"), "NDJSON must be newline-terminated");
        let lines = std::str::from_utf8(&body)
            .expect("UTF-8 NDJSON")
            .lines()
            .collect::<Vec<_>>();
        assert!(
            lines.len() >= 2,
            "real turn should emit at least two activities: {lines:#?}"
        );
        let activities = lines
            .iter()
            .enumerate()
            .map(|(sequence, line)| {
                let activity: RemoteTurnActivity =
                    serde_json::from_str(line).expect("one activity per NDJSON line");
                activity.validate().expect("valid remote activity");
                assert_eq!(activity.sequence, sequence as u64);
                activity
            })
            .collect::<Vec<_>>();
        assert_eq!(
            activities
                .iter()
                .filter(|activity| matches!(activity.event, RemoteTurnEvent::FinalValue { .. }))
                .count(),
            1,
            "complete raw stream must contain exactly one final_value: {activities:#?}"
        );
        assert!(
            matches!(
                activities.last().map(|activity| &activity.event),
                Some(RemoteTurnEvent::FinalValue { .. })
            ),
            "final_value must be the last raw activity: {activities:#?}"
        );
        let messages = state
            .with_db(move |db| db.list_messages(&chat_id))
            .await
            .expect("persisted messages");
        assert!(
            messages.iter().any(|message| message.kind == "code_block"),
            "fanout should also deliver the turn to the persistence sink: {messages:#?}"
        );
    }

    #[cfg(feature = "restate")]
    #[tokio::test]
    async fn raw_activity_route_rejects_restate_without_inserting_a_user_row() {
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path();
        let state = raw_activity_test_state(data_dir, AgentServiceDurability::Restate).await;
        let chat = state
            .with_db(|db| db.create_chat("raw activities", "scripted-model", None))
            .await
            .expect("create chat");
        let chat_id = chat.id;

        let error = stream_raw_activities(
            State(state.clone()),
            AxumPath(chat_id.clone()),
            Json(StreamRawActivitiesRequest {
                text: "must not be inserted".to_string(),
            }),
        )
        .await
        .expect_err("Restate raw activity route must return 501");
        assert_eq!(error.status, StatusCode::NOT_IMPLEMENTED);
        assert!(
            error
                .message
                .contains("app-owned turn outbox keyed by turn_id"),
            "501 should point to the Restate progress-stream alternative: {error}"
        );
        let messages = state
            .with_db(move |db| db.list_messages(&chat_id))
            .await
            .expect("persisted messages");
        assert!(
            messages.is_empty(),
            "Restate rejection must happen before inserting the user row: {messages:#?}"
        );
    }

    async fn raw_activity_test_state(
        data_dir: &std::path::Path,
        durability: AgentServiceDurability,
    ) -> AppStateData {
        let provider = lash::testing::TestProvider::builder()
            .kind("agent-service-raw-activity-script")
            .complete(|_request| async {
                let text = r#"<lashlang>
finish "done through raw activities"
</lashlang>"#;
                Ok(LlmResponse {
                    full_text: text.to_string(),
                    parts: vec![LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            })
            .build()
            .into_handle();
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::new(
                lash_protocol_rlm::ExecutionBound::instructions(1_000_000),
                lash_protocol_rlm::ExecutionBound::secs(30),
            ),
            Arc::new(
                lash_sqlite_store::Store::open(&data_dir.join("artifacts.db"))
                    .await
                    .expect("artifact store"),
            ),
        );
        let core = LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
            .provider(provider)
            .model(
                lash::ModelSpec::builder("scripted-model")
                    .context_window_tokens(200_000)
                    .build()
                    .expect("model spec"),
            )
            .store_factory(Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
                data_dir.join("lash-sessions"),
            )))
            .effect_host(Arc::new(
                lash::durability::InlineEffectHost::default()
                    .allow_process_lifetime_completion_keys(),
            ))
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .process_env_store(Arc::new(
                lash_sqlite_store::Store::open(&data_dir.join("process-env.db"))
                    .await
                    .expect("process env store"),
            ))
            .trigger_store(Arc::new(
                lash_sqlite_store::SqliteTriggerStore::open(&data_dir.join("triggers.db"))
                    .await
                    .expect("trigger store"),
            ))
            .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
                data_dir.join("attachments"),
            )))
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "agent-service-raw-activity-test",
                "agent-service-raw-activity-test-boot",
            ))
            .expect("core");
        let turn_work_driver = core.turn_work_driver();
        #[cfg(not(feature = "restate"))]
        let state = AppStateData::new(
            core,
            turn_work_driver,
            AppDb::open(&data_dir.join("app.db")).expect("app db"),
            "scripted-model".to_string(),
            None,
            durability,
        );
        #[cfg(feature = "restate")]
        let state = AppStateData::from_shared_db(
            core,
            turn_work_driver,
            Arc::new(Mutex::new(
                AppDb::open(&data_dir.join("app.db")).expect("app db"),
            )),
            lash::persistence::LeaseOwnerIdentity::opaque("agent-service-test", "test"),
            "scripted-model".to_string(),
            None,
            durability,
            None,
        );
        state
    }
}
