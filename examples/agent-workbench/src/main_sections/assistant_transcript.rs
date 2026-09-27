//! The reply the workbench commits as a turn's durable assistant message.

use super::*;
use lash::TurnId;

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
pub(crate) fn workbench_owns_committed_agent_reply(output: &TurnReport) -> bool {
    output.assistant_message().is_none()
}

/// Commit the reply the workbench renders as this turn's durable assistant
/// message. Only for turns `workbench_owns_committed_agent_reply` claims.
///
/// The commit is a lane-less append made after the root settled, and the
/// session's engine may commit the session's next root (a queued input, a
/// wake) between the append's read of the session and its head CAS. The loss
/// is a typed head-revision conflict; retrying on the same open keeps
/// committing against whatever head the losing runtime last refreshed, so
/// the conflict is answered by reloading the session — `reopen` opens it
/// again, reading the head the conflict named and pacing the retry past the
/// drive still holding the lane — before the append is made again
/// (FIG-3925).
///
/// The retry is bounded by [`SESSION_OPEN_MAX_ATTEMPTS`] conflicting appends
/// paced by the same jittered backoff as the workbench's other contention
/// retry — and by nothing else. A wall-clock budget measures how slow the
/// host is, not how contended the session is: every millisecond spent parked
/// behind a competing commit or waiting out a busy store would spend the
/// allowance before a single retry ran, so only lost head CASes count
/// (FIG-3936). An already-committed reply id skips the append, so a retry
/// that lands after its own commit is still idempotent. Only the typed
/// conflict retries; every other error stays terminal.
#[expect(
    clippy::expect_used,
    reason = "every break out of the retry loop follows a conflicting attempt that just \
              recorded last_conflict; reaching a second attempt requires one"
)]
pub(crate) async fn commit_assistant_transcript<Reopen, ReopenFuture, Trace>(
    session: &lash::LashSession,
    turn_id: &TurnId,
    assistant_text: String,
    model: Option<&str>,
    mut reopen: Reopen,
    mut trace: Trace,
) -> Result<(), AppError>
where
    Reopen: FnMut() -> ReopenFuture,
    ReopenFuture: std::future::Future<Output = Result<lash::LashSession, lash::EmbedError>>,
    Trace: FnMut(&str, Value),
{
    let message_id = workbench_turn_assistant_message_id(turn_id);
    let started = tokio::time::Instant::now();
    let mut reloaded: Option<lash::LashSession> = None;
    let mut last_conflict = None;
    for attempt in 1..=SESSION_OPEN_MAX_ATTEMPTS {
        let session = reloaded.as_ref().unwrap_or(session);
        let already_committed = session
            .read_view()
            .messages()
            .iter()
            .any(|message| message.id == message_id);
        if already_committed {
            return Ok(());
        }
        let message = assistant_transcript_message(&message_id, assistant_text.clone(), model);
        match session.admin().state().append_messages(vec![message]).await {
            Ok(()) => return Ok(()),
            Err(error) if reply_commit_is_head_conflict(&error) => {
                trace(
                    "turn.reply_commit.head_conflict",
                    json!({
                        "turn_id": turn_id,
                        "attempt": attempt,
                        "attempt_cap": SESSION_OPEN_MAX_ATTEMPTS,
                        "elapsed_ms": started.elapsed().as_millis(),
                        "outcome": "reloading",
                    }),
                );
                last_conflict = Some(error);
                if attempt == SESSION_OPEN_MAX_ATTEMPTS {
                    break;
                }
                reloaded = Some(reopen().await.map_err(AppError::session_open)?);
                tokio::time::sleep(contention_retry_delay(attempt)).await;
            }
            Err(error) => return Err(AppError::runtime(error)),
        }
    }
    trace(
        "turn.reply_commit.retry_exhausted",
        json!({
            "turn_id": turn_id,
            "attempt_cap": SESSION_OPEN_MAX_ATTEMPTS,
            "elapsed_ms": started.elapsed().as_millis(),
            "outcome": "temporarily_unavailable",
        }),
    );
    // A contention retry that runs out of attempts is the session briefly
    // busy, not a turn failure: the follower refollows the root and the reply
    // commit is made again against the reloaded head.
    Err(AppError::retryable_internal(last_conflict.expect(
        "a reply-commit retry budget exhausts only after typed contention",
    )))
}

/// Whether the reply commit's append lost its head CAS to a competing
/// session commit. The conflict is the one outcome a reload-and-retry can
/// repair; anything else must stay terminal.
fn reply_commit_is_head_conflict(error: &lash::EmbedError) -> bool {
    matches!(
        error,
        lash::EmbedError::Session(lash::SessionError::Store {
            source: lash::persistence::StoreError::HeadRevisionConflict { .. },
            ..
        })
    )
}

/// The durable assistant message a reply commit appends.
fn assistant_transcript_message(
    message_id: &str,
    assistant_text: String,
    model: Option<&str>,
) -> lash::plugins::PluginMessage {
    let mut message = lash::plugins::PluginMessage::text(
        lash::messages::MessageRole::Assistant,
        assistant_text.clone(),
    )
    .with_id(message_id.to_string());
    if let Some((turn, model)) = replay_route_committed_reply(&assistant_text, model) {
        // The deterministic replay-route fixture must cross the same durable
        // transcript seam as a resumed production turn. Keep its visible text
        // ordinary, while retaining provider-owned replay state in a hidden
        // reasoning part for the next request's route filter to inspect.
        message.parts = vec![
            lash::messages::Part::text(format!("{message_id}.p0"), assistant_text, None),
            lash::messages::Part::reasoning(
                format!("{message_id}.p1"),
                format!("FIG-1374 portable reasoning {turn}"),
                Some(lash::direct::ProviderReasoningReplay {
                    signature: Some(format!("FIG1374-OPAQUE-REPLAY-{turn}")),
                    origin: Some(lash::direct::ProviderRouteIdentity::new(
                        "workbench-dev-failure",
                        "workbench-dev-failure",
                        model,
                    )),
                    ..Default::default()
                }),
            ),
        ];
    }
    message
}

pub(crate) fn replay_route_committed_reply<'a>(
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
