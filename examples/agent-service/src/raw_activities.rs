use lash::TurnId;
use std::io::{self, Write};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::Response;
use lash::TurnInput;
use lash::remote::usage::RemoteTurnActivitySink;
use lash::rlm::RlmSendBuilderExt as _;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::remote_protocol::negotiate_remote;
use crate::routes::{
    TurnRefusal, answered_output, llm_profile_choice_for_chat_selection, mirror_committed_turn,
};
use crate::state::{AppError, AppResult, AppStateData};

#[derive(Debug, Deserialize)]
pub(crate) struct StreamRawActivitiesRequest {
    text: String,
}

/// Run one chat turn and expose its canonical remote activity projection as
/// newline-delimited JSON. This is a raw transport endpoint: unlike the
/// product message stream, it carries no app-owned rows or replay envelope.
/// The SQL transcript mirror is filled from committed rows after settlement.
pub(crate) async fn stream_raw_activities(
    State(state): State<AppStateData>,
    AxumPath(chat_id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<StreamRawActivitiesRequest>,
) -> AppResult<Response> {
    let (negotiated, accept_json) = negotiate_remote(&headers)?;
    let text = request.text.trim().to_string();
    if text.is_empty() {
        return Err(AppError::bad_request("message text is required"));
    }
    let llm_profile_selection = state
        .with_db({
            let chat_id = chat_id.clone();
            let text = text.clone();
            move |db| {
                db.require_chat(&chat_id)?;
                let llm_profile_selection = db.chat_llm_profile_selection(&chat_id)?;
                let board = db.chat_board(&chat_id)?;
                db.maybe_title_from_first_message(&chat_id, &text)?;
                db.insert_message_with_payload(
                    &chat_id,
                    "user",
                    &text,
                    Some(json!({ "board": board })),
                )?;
                Ok(llm_profile_selection)
            }
        })
        .await?;

    let turn_profile = llm_profile_choice_for_chat_selection(&llm_profile_selection);
    let session = state.open_session(&chat_id, turn_profile).await?;
    let turn_id = TurnId::prefixed("agent-service-raw-turn:", uuid::Uuid::new_v4());
    // Accepted before the response starts, so a refused acceptance is the
    // response's status: a retryable refusal answers 503.
    let turn = session
        .send(TurnInput::text(text))
        .id(turn_id.clone())
        .require_finish()?
        .await?;
    let (tx, rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let remote_events = Arc::new(RemoteTurnActivitySink::new(
        NdjsonChannelWriter::new(tx),
        0,
        negotiated,
    ));

    let task_turn_id = turn_id.clone();
    tokio::spawn(async move {
        match turn
            .outcome_into(remote_events.as_ref())
            .await
            .map_err(TurnRefusal::from)
            .and_then(answered_output)
        {
            Ok(_output) => {
                if let Err(error) =
                    mirror_committed_turn(&state, &chat_id, &session, &task_turn_id).await
                {
                    eprintln!("agent-service committed transcript mirror failed: {error}");
                }
            }
            Err(refusal) => eprintln!(
                "agent-service raw activity turn has no answer (retryable: {}): {}",
                refusal.retryable, refusal.message
            ),
        }
        let errors = remote_events.take_errors();
        if !errors.is_empty() {
            eprintln!(
                "agent-service raw activity stream write or encode failed: {}",
                errors.join("; ")
            );
        }
    });

    let mut response = crate::ndjson::ndjson_response(UnboundedReceiverStream::new(rx));
    let headers = response.headers_mut();
    headers.insert(
        "x-lash-turn-id",
        HeaderValue::from_str(turn_id.as_str())
            .map_err(|err| AppError::internal(format!("invalid turn-id header: {err}")))?,
    );
    headers.insert(
        "x-lash-protocol-accept",
        HeaderValue::from_str(&accept_json)
            .map_err(|err| AppError::internal(format!("invalid protocol-accept header: {err}")))?,
    );
    Ok(response)
}

/// Frames the remote sink's already-encoded NDJSON bytes back into the
/// stream items `ndjson_response` serves: one `Value` per line, reserialized
/// verbatim.
struct NdjsonChannelWriter {
    tx: mpsc::UnboundedSender<serde_json::Value>,
    pending: Vec<u8>,
}

impl NdjsonChannelWriter {
    fn new(tx: mpsc::UnboundedSender<serde_json::Value>) -> Self {
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
            let item =
                serde_json::from_slice::<serde_json::Value>(&line).map_err(io::Error::other)?;
            self.tx
                .send(item)
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
    use axum::http::{StatusCode, header};
    use lash::LashCore;
    use lash::direct::LlmOutputPart;
    use lash::provider::LlmResponse;
    use lash::remote::usage::RemoteTurnActivity;
    use lash::remote::usage::RemoteTurnEvent;
    use std::sync::Mutex;

    use super::*;
    use crate::db::AppDb;

    #[tokio::test]
    async fn raw_activity_route_streams_framed_activities_from_a_real_turn() {
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path();
        let double = crate::state::test_support::test_double().await;
        let state = raw_activity_test_state(data_dir, &double).await;
        let chat = state
            .with_db(|db| db.create_chat("raw activities", "scripted-model", None))
            .await
            .expect("create chat");
        let chat_id = chat.id;

        let response = stream_raw_activities(
            State(state.clone()),
            AxumPath(chat_id.clone()),
            crate::remote_protocol::test_remote_headers(),
            Json(StreamRawActivitiesRequest {
                text: "exercise raw activity transport".to_string(),
            }),
        )
        .await
        .expect("raw activity endpoint");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key("x-lash-protocol-accept"));
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
        let durable = state
            .core()
            .session(lash::SessionId::parse(&chat_id).expect("session id"))
            .durable()
            .await
            .expect("durable session");
        let canonical = durable.transcript().await.expect("canonical rows");
        let messages = state
            .with_db(move |db| db.list_messages(&chat_id))
            .await
            .expect("persisted messages");
        let records = messages
            .iter()
            .filter_map(|message| {
                message
                    .payload()
                    .and_then(|payload| payload.get("transcript"))
            })
            .map(|row| {
                serde_json::from_value::<lash::transcript::TranscriptRowRecord>(row.clone())
                    .expect("typed mirror")
            })
            .collect::<Vec<_>>();
        let expected = canonical
            .visible()
            .filter(|row| row.kind != lash::transcript::TranscriptRowKind::User)
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            records, expected,
            "raw transport settlement mirrors every canonical row"
        );
        assert_eq!(
            records
                .iter()
                .filter(|row| row.provenance.is_turn_reply)
                .map(|row| row.content.text.as_str())
                .collect::<Vec<_>>(),
            ["done through raw activities"],
            "the committed reply marker selects exactly one answer"
        );
    }

    async fn raw_activity_test_state(
        data_dir: &std::path::Path,
        double: &lash_restate_test::RestateTestBackend,
    ) -> AppStateData {
        let provider = lash::testing::TestProvider::builder()
            .kind("agent-service-raw-activity-script")
            .complete(|_request| async {
                let text = r#"<typescript>
finish("done through raw activities");
</typescript>"#;
                Ok(LlmResponse {
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
        let backend = double.lash_backend();
        let factory = crate::rlm_factory(&backend);
        let core = LashCore::rlm_builder(backend, factory)
            .serve_test_llm_profile(
                provider,
                lash::LlmProfileMetadata::builder("scripted-model")
                    .context_window_tokens(200_000)
                    .build()
                    .expect("model spec"),
            )
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "agent-service-raw-activity-test",
                "agent-service-raw-activity-test-boot",
            ))
            .expect("core");
        AppStateData::new(
            core,
            Arc::new(Mutex::new(
                AppDb::open(&data_dir.join("app.db")).expect("app db"),
            )),
            "scripted-model".to_string(),
            None,
            double.connection(),
        )
    }
}
