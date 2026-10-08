//! A read route never writes the session it reads: after a peer commanded a
//! model change, serving `path` leaves the session's recorded config and
//! head revision as they were.

use super::*;

async fn observation_get_preserves_config(path: &str) {
    let workbench = Workbench::silent().await;
    let state = workbench.state.clone();
    let session_id = state.current_session_id();
    let session = state
        .create_or_open_session(&session_id, "test")
        .await
        .expect("open the session");
    let config = session.admin().config();
    let outcome = config
        .apply(
            lash::config::ConfigWrite::new(
                "peer-commanded-model",
                config.revision().await.expect("read the config revision"),
            ),
            lash::config::ConfigTransaction::of(lash::config::SetLlmProfile {
                model: lash::LlmProfileKey::new("peer-commanded-model"),
            }),
        )
        .await
        .expect("a peer commands a model change");
    assert!(
        matches!(
            outcome,
            lash::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{outcome:?}"
    );
    drop(config);
    drop(session);
    let store = Arc::clone(&state.session_store_factory);
    let before = store
        .load_session_head_meta(&session_id)
        .await
        .expect("read the head")
        .expect("the session has a head");
    assert_eq!(
        before
            .config
            .model
            .as_ref()
            .map(|model| model.key().as_str()),
        Some("peer-commanded-model")
    );

    let app = Router::new()
        .route("/api/state", get(app_state))
        .route(
            "/api/observations",
            get(|state, query, headers| {
                session_observations_with_shutdown(state, query, headers, None)
            }),
        )
        .route("/api/queued-work", get(list_queued_work))
        .route("/api/sessions", get(list_sessions))
        .route(
            "/api/events",
            get(|state, query| session_events_with_shutdown(state, query, None)),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local port");
    let address = listener.local_addr().expect("the bound address");
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let mut request = reqwest::Client::new().get(format!("http://{address}{path}"));
    if path == "/api/observations" {
        request = request.headers(remote_hello_headers());
    }
    let response = request.send().await.expect("the route answers");
    assert!(
        response.status().is_success(),
        "{path}: {}",
        response.status()
    );
    drop(response);
    let after = store
        .load_session_head_meta(&session_id)
        .await
        .expect("read the head")
        .expect("the session has a head");
    server.abort();
    let _ = server.await;
    assert_eq!(
        after.head_revision, before.head_revision,
        "{path} wrote the session"
    );
    assert_eq!(after.config, before.config, "{path} changed the config");
    workbench.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_get_preserves_config() {
    observation_get_preserves_config("/api/state").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observations_get_preserves_config() {
    observation_get_preserves_config("/api/observations").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_work_get_preserves_config() {
    observation_get_preserves_config("/api/queued-work").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_get_preserves_config() {
    observation_get_preserves_config("/api/sessions").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_get_preserves_config() {
    observation_get_preserves_config("/api/events").await;
}
