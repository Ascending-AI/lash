use super::*;
use crate::db::AppDb;
use lash::direct::LlmOutputPart;
use lash::provider::LlmResponse;

#[tokio::test]
async fn reconnect_after_host_restart_follows_the_accepted_turn_to_terminal() {
    use crate::state::test_support::{
        mock_llm_profile, test_core, test_core_with_provider, test_state,
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("restart.db");
    let double = crate::state::test_support::test_double().await;
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("agent-service-reconnect")
        .complete({
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let calls = Arc::clone(&calls);
            move |_| {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    started.notify_one();
                    release.notified().await;
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "<typescript>finish(\"survived restart\");</typescript>"
                                .to_string(),
                            response_meta: None,
                        }],
                        ..Default::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let core = test_core_with_provider(&double, provider).await;
    let state = test_state(&double, &core, AppDb::open(&db_path).expect("app db"));
    let chat = state
        .with_db(|db| db.create_chat("restart", "mock-model", None))
        .await
        .expect("chat");
    let session = state
        .open_session(&chat.id, mock_llm_profile())
        .await
        .expect("session");
    let turn_id = TurnId::fixture("restart-turn");
    let handle = session
        .send(TurnInput::text("continue across restart"))
        .id(turn_id.clone())
        .require_finish()
        .expect("finish")
        .await
        .expect("accepted");
    let input_id = handle.input_id().clone();
    tokio::time::timeout(std::time::Duration::from_secs(10), started.notified())
        .await
        .expect("engine started");
    drop(handle);
    drop(session);
    drop(state);
    drop(core);

    // Only the engine and durable stores survive. The new HTTP host has
    // neither the original handle nor its runtime session.
    let reopened_core = test_core(&double).await;
    let restarted = test_state(
        &double,
        &reopened_core,
        AppDb::open(&db_path).expect("reopen app db"),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listen");
    let addr = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        axum::serve(listener, crate::app_router(restarted))
            .await
            .expect("serve")
    });
    let client = reqwest::Client::new();
    let response = client
        .get(format!(
            "http://{addr}/api/chats/{}/turns/{turn_id}",
            chat.id
        ))
        .headers(test_remote_headers())
        .send()
        .await
        .expect("reconnect");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "an accepted turn must be followable by a fresh host"
    );
    release.notify_one();
    let body = tokio::time::timeout(std::time::Duration::from_secs(10), response.text())
        .await
        .expect("terminal deadline")
        .expect("body");
    let rows = body
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("row"))
        .collect::<Vec<_>>();
    let outcome = rows
        .iter()
        .find(|row| row["type"] == "outcome")
        .expect("durable outcome");
    assert_eq!(outcome["outcome"]["type"], "settled");
    assert_eq!(outcome["outcome"]["input_id"], input_id.to_string());
    assert_eq!(outcome["outcome"]["report"]["turn_id"], turn_id.as_str());
    assert!(
        body.contains("survived restart"),
        "the terminal carries the original answer: {body}"
    );
    assert_eq!(rows.last().expect("done")["type"], "done");
    let by_input = client
        .get(format!(
            "http://{addr}/api/chats/{}/inputs/{input_id}",
            chat.id
        ))
        .headers(test_remote_headers())
        .send()
        .await
        .expect("attach input");
    assert_eq!(by_input.status(), StatusCode::OK);
    let body = tokio::time::timeout(std::time::Duration::from_secs(10), by_input.text())
        .await
        .expect("settled deadline")
        .expect("body");
    assert!(body.contains("survived restart"));
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "reattachment never resubmits input"
    );
    let messages = AppDb::open(&db_path)
        .expect("db")
        .list_messages(&chat.id)
        .expect("messages");
    assert!(
        messages.is_empty(),
        "observers never write duplicate transcript rows"
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn chat_list_uses_the_lash_catalog_and_new_chats_are_catalogued() {
    use crate::state::test_support::{mock_llm_profile, test_core, test_state};

    let temp = tempfile::tempdir().expect("tempdir");
    let double = crate::state::test_support::test_double().await;
    let core = test_core(&double).await;
    let state = test_state(
        &double,
        &core,
        AppDb::open(&temp.path().join("catalog.db")).expect("db"),
    );
    let real = state
        .with_db(|db| db.create_chat("catalogued", "mock-model", None))
        .await
        .expect("chat");
    state
        .open_session(&real.id, mock_llm_profile())
        .await
        .expect("session");
    let phantom = state
        .with_db(|db| db.create_chat("app only", "mock-model", None))
        .await
        .expect("phantom");
    let created = create_chat(
        State(state.clone()),
        Json(CreateChatRequest {
            title: Some("empty chat".to_string()),
            model: None,
            model_variant: None,
        }),
    )
    .await
    .expect("create route")
    .0;
    let listed = list_chats(State(state.clone())).await.expect("list").0;
    let mut listed_ids = listed
        .iter()
        .map(|chat| chat.id.clone())
        .collect::<Vec<_>>();
    listed_ids.sort();
    let mut catalog_ids = core
        .sessions_filtered(lash::SessionListFilter {
            deleted: Some(false),
            ..Default::default()
        })
        .await
        .expect("catalog")
        .into_iter()
        .map(|session| session.session_id.to_string())
        .collect::<Vec<_>>();
    catalog_ids.sort();
    assert_eq!(
        listed_ids, catalog_ids,
        "app metadata must not invent catalog entries"
    );
    assert!(
        listed_ids.contains(&created.id),
        "an empty chat is created in Lash immediately"
    );
    assert!(!listed_ids.contains(&phantom.id));
    assert_eq!(
        listed
            .iter()
            .find(|chat| chat.id == real.id)
            .expect("real")
            .title,
        "catalogued"
    );
}
