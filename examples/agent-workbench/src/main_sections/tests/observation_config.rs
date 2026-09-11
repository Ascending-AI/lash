use super::*;

async fn observation_get_preserves_config(path: &str) {
    let data_dir = tempfile::tempdir().unwrap();
    let state = recoverable_chat_test_state(data_dir.path(), 16).await;
    let session_id = state.current_session_id();
    let session = state.open_session(&session_id).await.unwrap();
    let peer_model = lash::ModelSpec::builder("peer-commanded-model")
        .context_window_tokens(8192)
        .build()
        .unwrap();
    session
        .admin()
        .config()
        .update(lash::SessionConfigPatch {
            model: Some(peer_model.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    drop(session);
    let store = state
        .session_store_factory
        .open_existing_store(&state_store_request(&state, &session_id))
        .await
        .unwrap()
        .unwrap();
    let before = store.load_session_head_meta().await.unwrap().unwrap();
    assert_eq!(before.config.model.id, peer_model.id);
    let app = Router::new()
        .route("/api/state", get(app_state))
        .route("/api/observations", get(session_observations))
        .route("/api/queued-work", get(list_queued_work))
        .route("/api/sessions", get(list_sessions))
        .route("/api/events", get(session_events))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let response = reqwest::Client::new()
        .get(format!("http://{address}{path}"))
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{path}: {}",
        response.status()
    );
    drop(response);
    let after = store.load_session_head_meta().await.unwrap().unwrap();
    server.abort();
    let _ = server.await;
    assert_eq!(
        after.head_revision, before.head_revision,
        "{path} wrote config"
    );
    assert_eq!(after.config, before.config, "{path} changed config");
}

#[tokio::test]
async fn state_get_preserves_config() {
    Box::pin(observation_get_preserves_config("/api/state")).await;
}
#[tokio::test]
async fn observations_get_preserves_config() {
    Box::pin(observation_get_preserves_config("/api/observations")).await;
}
#[tokio::test]
async fn queued_work_get_preserves_config() {
    Box::pin(observation_get_preserves_config("/api/queued-work")).await;
}

#[tokio::test]
async fn sessions_get_preserves_config() {
    Box::pin(observation_get_preserves_config("/api/sessions")).await;
}
#[tokio::test]
async fn events_get_preserves_config() {
    Box::pin(observation_get_preserves_config("/api/events")).await;
}
