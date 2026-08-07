//! The bot's HTTP surface: the Events API request URL and a health endpoint.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use super::channel::ChannelBot;
use crate::wire::events::{ChallengeResponse, EventRequest, RETRY_NUM_HEADER, RETRY_REASON_HEADER};

/// Path the bot registers as its Events API request URL.
pub const EVENTS_PATH: &str = "/slack/events";

/// Build the bot's router.
pub fn router(bot: Arc<ChannelBot>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route(EVENTS_PATH, axum::routing::post(events))
        .with_state(bot)
}

/// Receive one Events API request.
///
/// The handler acknowledges and returns; handling happens on a spawned task.
/// This is not an optimisation — Slack requires a 2xx within three seconds and
/// retries otherwise, so a host that ran a model turn before acknowledging would
/// guarantee itself a storm of redeliveries for every slow answer. Acknowledging
/// first is what makes the ledger the thing that keeps the bot correct.
async fn events(State(bot): State<Arc<ChannelBot>>, headers: HeaderMap, body: String) -> Response {
    let request = match serde_json::from_str::<EventRequest>(&body) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("slack-clone-bot rejected an unparseable event: {error}");
            return (StatusCode::BAD_REQUEST, "unparseable event").into_response();
        }
    };
    match request {
        EventRequest::UrlVerification(handshake) => {
            // Answered inline: the whole point of the handshake is to prove this
            // endpoint is live right now.
            Json(ChallengeResponse {
                challenge: handshake.challenge,
            })
            .into_response()
        }
        EventRequest::EventCallback(envelope) => {
            let retry_num = headers
                .get(RETRY_NUM_HEADER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u32>().ok());
            if let Some(reason) = headers
                .get(RETRY_REASON_HEADER)
                .and_then(|value| value.to_str().ok())
            {
                eprintln!(
                    "slack-clone-bot redelivery of {} because {reason}",
                    envelope.event_id
                );
            }
            let event_id = envelope.event_id.clone();
            tokio::spawn(async move {
                match bot.ingest(*envelope, retry_num).await {
                    Ok(disposition) => {
                        println!("slack-clone-bot handled {event_id}: {disposition:?}");
                    }
                    Err(error) => {
                        // The ledger row stays unfinished, so boot recovery or a
                        // later redelivery picks the event back up.
                        eprintln!("slack-clone-bot failed to handle {event_id}: {error:#}");
                    }
                }
            });
            StatusCode::OK.into_response()
        }
    }
}

/// Liveness plus the identity the bot resolved, which is the first thing to
/// check when a bot is running but never answers.
async fn healthz(State(bot): State<Arc<ChannelBot>>) -> Json<serde_json::Value> {
    let identity = bot.identity();
    Json(json!({
        "service": "slack-clone-bot",
        "team_id": identity.team_id,
        "bot_id": identity.bot_id,
        "bot_user_id": identity.bot_user_id,
        "bot_handle": identity.handle,
        "events_path": EVENTS_PATH,
    }))
}
