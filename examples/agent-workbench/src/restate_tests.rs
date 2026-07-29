use super::{classified_embed_handler_error, cron_occurrence_key};
use crate::AppError;

#[test]
fn cron_occurrence_key_is_unique_per_tick() {
    let job = "session:source:cron.Schedule:sha256:abc";
    let first = cron_occurrence_key(job, "2026-06-09T22:30:30+00:00");
    let second = cron_occurrence_key(job, "2026-06-09T22:31:00+00:00");
    // Two ticks of one job must not collide: a constant key makes the
    // second tick fail its trigger emit with an idempotency conflict
    // before re-arming, killing the schedule after exactly one fire.
    assert_ne!(first, second);
    // A retried tick (same journaled fired_at) must dedupe.
    assert_eq!(first, cron_occurrence_key(job, "2026-06-09T22:30:30+00:00"));
    // Distinct jobs never collide on the same tick time.
    assert_ne!(
        first,
        cron_occurrence_key("other-job", "2026-06-09T22:30:30+00:00")
    );
}

#[test]
fn cron_sync_classifies_permanent_errors_terminal_and_unknown_errors_retryable() {
    let deleted = classified_embed_handler_error(lash::EmbedError::Store(
        lash::persistence::StoreError::SessionDeleted {
            session_id: "retired-session".to_string(),
        },
    ));
    let deleted_rendered =
        <restate_sdk::errors::HandlerError as AsRef<dyn std::error::Error>>::as_ref(&deleted)
            .to_string();
    assert!(
        deleted_rendered.starts_with("Terminal error"),
        "SessionDeleted must terminate cron reconciliation, got {deleted_rendered}"
    );

    let unknown = classified_embed_handler_error(lash::EmbedError::Store(
        lash::persistence::StoreError::Backend("temporary outage".to_string()),
    ));
    let unknown_rendered =
        <restate_sdk::errors::HandlerError as AsRef<dyn std::error::Error>>::as_ref(&unknown)
            .to_string();
    assert!(
        !unknown_rendered.starts_with("Terminal error"),
        "ambiguous store failures must remain retryable, got {unknown_rendered}"
    );
}

#[test]
fn runtime_shape_uses_the_shared_terminal_classifier() {
    let error = AppError::runtime(lash::EmbedError::Session(lash::SessionError::Store {
        context: "failed to bind retired session".to_string(),
        source: lash::persistence::StoreError::SessionDeleted {
            session_id: "retired-session".to_string(),
        },
    }));

    assert!(error.terminal);
    assert!(!error.retryable);
    assert_eq!(
        error.message,
        crate::deleted_session_message("retired-session")
    );
}
