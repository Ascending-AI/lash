//! Two sessions of one workbench run side by side and share nothing: each
//! transcript holds only its own turn.

use super::*;
use lash::SessionId;

/// The text of every user row `session_id` renders.
async fn user_rows(state: &AppState, session_id: &SessionId) -> Vec<String> {
    state_rows(
        &read_state(state, Some(session_id))
            .await
            .expect("project the session"),
    )
    .into_iter()
    .filter(|(role, _)| role == "user")
    .map(|(_, text)| text)
    .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_isolate_transcripts() {
    let workbench = Workbench::replying("<typescript>\nfinish(\"isolated\");\n</typescript>").await;
    let state = &workbench.state;
    let session_a = state.current_session_id();
    let Json(created) = create_session(
        State(state.clone()),
        Json(SessionCreateRequest {
            name: Some("session b".to_string()),
        }),
    )
    .await
    .expect("create session B");
    let session_b = created.session_id;

    let (turn_a, turn_b) = tokio::join!(
        send_text(state, Some(&session_a), "isolation-marker-A"),
        send_text(state, Some(&session_b), "isolation-marker-B"),
    );
    let turn_a = started_turn_id(&turn_a.expect("session A's send"));
    let turn_b = started_turn_id(&turn_b.expect("session B's send"));
    wait_for_turn_released(state, &session_a, &turn_a, Duration::from_secs(30)).await;
    wait_for_turn_released(state, &session_b, &turn_b, Duration::from_secs(30)).await;
    assert_eq!(
        user_rows(state, &session_a).await,
        vec!["isolation-marker-A".to_string()]
    );
    assert_eq!(
        user_rows(state, &session_b).await,
        vec!["isolation-marker-B".to_string()]
    );

    workbench.shutdown().await;
}
