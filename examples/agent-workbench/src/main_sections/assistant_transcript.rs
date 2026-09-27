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

/// Attempts at a reply commit that loses its head CAS. The reply is a
/// lane-less append made after the root settled, and the session's engine may
/// commit the session's next root (a queued input, a wake) first; the loser's
/// runtime then holds the committed head, so the append is made again.
const REPLY_COMMIT_ATTEMPTS: usize = 4;

/// Commit the reply the workbench renders as this turn's durable assistant
/// message. Only for turns `workbench_owns_committed_agent_reply` claims.
pub(crate) async fn commit_assistant_transcript(
    session: &lash::LashSession,
    turn_id: &TurnId,
    assistant_text: String,
    model: Option<&str>,
) -> Result<(), AppError> {
    let message_id = workbench_turn_assistant_message_id(turn_id);
    let mut attempt = 1;
    loop {
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
            Err(lash::EmbedError::Session(lash::SessionError::Store {
                source: lash::persistence::StoreError::HeadRevisionConflict { .. },
                ..
            })) if attempt < REPLY_COMMIT_ATTEMPTS => attempt += 1,
            Err(error) => return Err(AppError::runtime(error)),
        }
    }
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
