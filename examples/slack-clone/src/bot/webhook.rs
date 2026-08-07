//! The bot's HTTP surface: the Events API request URL and a health endpoint.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use super::channel::{self, ChannelBot, Disposition};
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
            //
            // Answered as JSON rather than as a plaintext echo. Slack accepts
            // either, and the platform's `verify_and_register` accepts either, so
            // the choice is only about which one a reader should copy: JSON is
            // self-describing and cannot be confused with an error page by a
            // proxy that rewrites content types.
            Json(ChallengeResponse {
                challenge: handshake.challenge,
            })
            .into_response()
        }
        EventRequest::EventCallback(envelope) => {
            // Verify before spawning. `ingest` checks the token too — that is the
            // seam the tests drive — but a forged request should not get a task,
            // a session open or a log line's worth of work out of the bot. 403
            // rather than 200 so a genuinely misconfigured sender learns, and
            // rather than a retryable 5xx so a forger gains nothing by repeating.
            if !bot.accepts_token(&envelope.token) {
                eprintln!(
                    "slack-clone-bot rejected event {} with a bad verification token",
                    envelope.event_id
                );
                return (StatusCode::FORBIDDEN, "bad verification token").into_response();
            }
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
                    Ok(Disposition::Deferred { event_id, .. }) => {
                        // The admission is fenced to a session-execution lease
                        // generation this boot cannot take yet — a redelivery
                        // landing inside the previous boot's lease TTL. Slack's own
                        // retries are bounded and would all fall inside that
                        // window, so the bot owns this retry.
                        println!("slack-clone-bot deferred {event_id}; retrying in the background");
                        if let Err(error) = bot
                            .retry_deferred(event_id.clone(), channel::DEFERRED_RETRY_DEADLINE)
                            .await
                        {
                            eprintln!(
                                "slack-clone-bot deferred retry of {event_id} failed: {error:#}"
                            );
                        }
                    }
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
