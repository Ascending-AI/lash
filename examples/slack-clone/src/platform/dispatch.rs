//! Events API delivery: at-least-once, with Slack's retry contract.
//!
//! The bot's whole idempotence story is only worth reading if the sender really
//! does redeliver, so this dispatcher reproduces Slack's contract rather than
//! best-effort firing a `tokio::spawn` and hoping. Concretely: the queue is
//! durable, an attempt that does not return 2xx within the delivery timeout is
//! retried up to three more times, and every retry carries
//! `x-slack-retry-num` and `x-slack-retry-reason`.

use std::time::Duration;

use tokio::task::JoinHandle;

use super::apps::{self, MAX_RETRIES};
use super::state::PlatformState;
use crate::wire::events::{RETRY_NUM_HEADER, RETRY_REASON_HEADER};

/// Events claimed per dispatcher pass.
const BATCH: usize = 16;
/// Upper bound on how long the loop sleeps when idle, so a restart still drains
/// a backlog whose wake-up notification died with the previous process.
const IDLE_POLL: Duration = Duration::from_millis(500);

/// Start the delivery loop.
pub fn spawn(state: PlatformState) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match deliver_once(&state).await {
                Ok(0) => state.await_delivery_work(IDLE_POLL).await,
                Ok(_) => {}
                Err(error) => {
                    eprintln!("slack-clone-platform delivery pass failed: {error:#}");
                    tokio::time::sleep(IDLE_POLL).await;
                }
            }
        }
    })
}

/// Attempt every due delivery once. Returns how many were attempted.
pub async fn deliver_once(state: &PlatformState) -> anyhow::Result<usize> {
    let due = state
        .database()
        .call(|connection| apps::claim_due_events(connection, BATCH))
        .await?;
    let attempted = due.len();
    for row in due {
        // `attempts` counts completed attempts, so the retry number of the
        // attempt about to happen is `attempts`, and zero means "first delivery"
        // — which carries no retry headers at all, exactly as Slack does it.
        let retry_num = row.attempts;
        let outcome = post_event(
            state,
            &row.request_url,
            &row.payload_json,
            retry_num,
            row.last_reason.as_deref(),
        )
        .await;
        let id = row.id;
        match outcome {
            Ok(()) => {
                state
                    .database()
                    .call(move |connection| apps::mark_delivered(connection, id))
                    .await?;
            }
            Err(failure) => {
                let backoff = state
                    .config()
                    .retry_backoff
                    .saturating_mul(1u32 << retry_num.min(4))
                    .as_millis() as i64;
                let reason = failure.reason;
                let message = failure.detail.clone();
                let attempts = state
                    .database()
                    .call(move |connection| {
                        apps::mark_failed(connection, id, &message, reason, backoff)
                    })
                    .await?;
                let message = format!("{reason}: {failure}");
                if attempts > MAX_RETRIES {
                    eprintln!(
                        "slack-clone-platform abandoned event {} after {attempts} attempts: {message}",
                        row.event_id
                    );
                } else {
                    eprintln!(
                        "slack-clone-platform will retry event {} (attempt {attempts}): {message}",
                        row.event_id
                    );
                }
            }
        }
    }
    Ok(attempted)
}

/// A delivery failure, carrying the `x-slack-retry-reason` the next attempt
/// should advertise.
#[derive(Debug)]
struct DeliveryFailure {
    reason: &'static str,
    detail: String,
}

impl std::fmt::Display for DeliveryFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for DeliveryFailure {}

async fn post_event(
    state: &PlatformState,
    request_url: &str,
    payload_json: &str,
    retry_num: u32,
    previous_reason: Option<&str>,
) -> Result<(), DeliveryFailure> {
    let mut request = state
        .http()
        .post(request_url)
        .timeout(state.config().delivery_timeout)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(payload_json.to_string());
    if retry_num > 0 {
        request = request
            .header(RETRY_NUM_HEADER, retry_num.to_string())
            // Slack reports why the *previous* attempt failed, so the reason is
            // read back from the outbox rather than re-derived. The platform can
            // only produce three of Slack's six reasons; a bot written against
            // this example still handles the whole vocabulary.
            .header(
                RETRY_REASON_HEADER,
                previous_reason.unwrap_or("unknown_error"),
            );
    }
    let response = request.send().await.map_err(|error| {
        let reason = if error.is_timeout() {
            "http_timeout"
        } else {
            "connection_failed"
        };
        DeliveryFailure {
            reason,
            detail: error.to_string(),
        }
    })?;
    if response.status().is_success() {
        return Ok(());
    }
    Err(DeliveryFailure {
        reason: "http_error",
        detail: format!("HTTP {}", response.status()),
    })
}
