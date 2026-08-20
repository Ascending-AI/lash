//! Compiled sources for the Rust snippets on `docs/streaming.html`.

use std::sync::Arc;

use lash::observe::{SessionObservationEvent, SessionObservationEventPayload};
use lash::persistence::SessionReadView;
use lash::{LashSession, TurnActivity, TurnActivitySink, TurnInput};

async fn render_activity(_activity: TurnActivity) -> anyhow::Result<()> {
    Ok(())
}

async fn replace_from_read_view(_view: &SessionReadView) -> anyhow::Result<()> {
    Ok(())
}

async fn append_committed_view(_view: &SessionReadView) -> anyhow::Result<()> {
    Ok(())
}

async fn refetch_pending_turn_inputs(_ids: &[String]) -> anyhow::Result<()> {
    Ok(())
}

async fn refetch_queued_work_summaries(_ids: &[String]) -> anyhow::Result<()> {
    Ok(())
}

async fn refetch_process_summaries(_process_ids: &[String]) -> anyhow::Result<()> {
    Ok(())
}

async fn persist_cursor(_cursor: &lash::observe::SessionCursor) -> anyhow::Result<()> {
    Ok(())
}

async fn persist_remote_cursor(
    _cursor: &lash::remote::observations::RemoteSessionCursor,
) -> anyhow::Result<()> {
    Ok(())
}

async fn update_frame(_frame_id: &str) -> anyhow::Result<()> {
    Ok(())
}

async fn replace_from_remote_observation(
    _observation: &lash::remote::observations::RemoteSessionObservation,
) -> anyhow::Result<()> {
    Ok(())
}

async fn send_remote_session_line(_line: String) -> anyhow::Result<()> {
    Ok(())
}

// docs:start:turn-local-stream
async fn stream_one_turn(
    session: &LashSession,
    sink: &dyn TurnActivitySink,
) -> lash::Result<lash::TurnReport> {
    session
        .turn(TurnInput::text("Summarize the incident."))
        .stream_to(sink)
        .await
}

async fn pull_one_turn(session: &LashSession) -> anyhow::Result<lash::TurnReport> {
    use futures_util::StreamExt as _;

    let mut stream = session
        .turn(TurnInput::text("Summarize the incident."))
        .stream()?;

    while let Some(activity) = stream.next().await {
        render_activity(activity?).await?;
    }

    Ok(stream.finish().await?)
}
// docs:end:turn-local-stream

// docs:start:session-reconnect
use futures_util::StreamExt as _;
use lash::observe::SessionObservationStreamItem;

async fn reconnect_session(
    session: &LashSession,
    stored_cursor: Option<lash::observe::SessionCursor>,
) -> anyhow::Result<lash::observe::SessionCursor> {
    let observable = session.observe();

    let mut cursor = match stored_cursor {
        Some(cursor) => cursor,
        None => {
            let observation = observable.current_observation();
            replace_from_read_view(&observation.read_view).await?;
            observation.cursor
        }
    };

    persist_cursor(&cursor).await?;

    let mut live = observable.subscribe_and_recover(cursor.clone());
    while let Some(item) = live.next().await {
        match item? {
            SessionObservationStreamItem::Event(event) => {
                cursor = event.cursor.clone();
                fold_session_event(event).await?;
            }
            SessionObservationStreamItem::Gap { observation, gap } => {
                replace_from_read_view(&observation.read_view).await?;
                cursor = gap.latest_cursor;
            }
        }
        persist_cursor(&cursor).await?;
    }

    Ok(cursor)
}
// docs:end:session-reconnect

async fn fold_session_event(event: Arc<SessionObservationEvent>) -> anyhow::Result<()> {
    // docs:start:fold-session-event
    match &event.payload {
        SessionObservationEventPayload::TurnActivity(activity) => {
            render_activity(activity.clone()).await?;
        }
        SessionObservationEventPayload::Committed { read_view } => {
            append_committed_view(read_view).await?;
        }
        SessionObservationEventPayload::ResidentChanged { read_view } => {
            replace_from_read_view(read_view).await?;
        }
        SessionObservationEventPayload::AgentFrameSwitched { frame_id } => {
            update_frame(frame_id).await?;
        }
        SessionObservationEventPayload::QueueChanged { batch_ids, .. } => {
            // Queue events cover both pending user input ids and non-user
            // queued-work batch ids; refresh the surfaces separately.
            refetch_pending_turn_inputs(batch_ids).await?;
            refetch_queued_work_summaries(batch_ids).await?;
        }
        SessionObservationEventPayload::ProcessChanged { process_ids, .. } => {
            refetch_process_summaries(process_ids).await?;
        }
    }
    // docs:end:fold-session-event
    Ok(())
}

// docs:start:remote-ndjson-sink
use lash_remote_protocol::RemoteTurnActivitySink;

// agent-service exercises this sink live at POST /api/chats/{chat_id}/activities.
async fn stream_turn_as_ndjson(session: &LashSession) -> anyhow::Result<Vec<u8>> {
    let sink = RemoteTurnActivitySink::new(Vec::<u8>::new(), 0);

    session
        .turn(TurnInput::text("Summarize the incident."))
        .stream_to(&sink)
        .await?;

    let errors = sink.take_errors();
    if !errors.is_empty() {
        anyhow::bail!(
            "remote stream write or encode failed: {}",
            errors.join("; ")
        );
    }

    sink.into_inner()
        .map_err(|_| anyhow::anyhow!("remote stream writer still borrowed"))
}
// docs:end:remote-ndjson-sink

// docs:start:remote-session-event
use lash::observe::RemoteSessionObservationStreamItem;

async fn stream_remote_session_observations(
    session: &LashSession,
    stored_cursor: Option<lash::remote::observations::RemoteSessionCursor>,
) -> anyhow::Result<lash::remote::observations::RemoteSessionCursor> {
    let observable = session.observe();
    let mut cursor = match stored_cursor {
        Some(cursor) => cursor,
        None => {
            let observation = observable.current_remote_observation();
            replace_from_remote_observation(&observation).await?;
            lash::remote::observations::RemoteSessionCursor::new(observation.cursor)
        }
    };

    persist_remote_cursor(&cursor).await?;

    let mut live = observable.subscribe_and_recover_remote(cursor.clone())?;
    while let Some(item) = live.next().await {
        match item? {
            RemoteSessionObservationStreamItem::Event(event) => {
                cursor = lash::remote::observations::RemoteSessionCursor::new(event.cursor.clone());
                send_remote_session_line(serde_json::to_string(&event)?).await?;
            }
            RemoteSessionObservationStreamItem::Gap { observation, gap } => {
                cursor =
                    lash::remote::observations::RemoteSessionCursor::new(gap.latest_cursor.clone());
                replace_from_remote_observation(&observation).await?;
                send_remote_session_line(serde_json::to_string(&gap)?).await?;
            }
        }
        persist_remote_cursor(&cursor).await?;
    }

    Ok(cursor)
}
// docs:end:remote-session-event
