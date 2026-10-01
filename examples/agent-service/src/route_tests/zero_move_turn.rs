use std::sync::{Arc, Mutex};

use axum::body::to_bytes;
use lash::direct::LlmOutputPart;
use lash::provider::LlmResponse;

use super::*;
use crate::board::BoardState;
use crate::db::AppDb;
use crate::state::test_support::{test_core_with_provider, test_state};

/// One X already on the board and O to move: the shape a board click
/// leaves behind, and the only shape that can wedge.
fn board_owing_a_move() -> BoardState {
    let mut cells = vec![None; 9];
    cells[0] = Some("X".to_string());
    BoardState {
        cells,
        turn: "O".to_string(),
    }
}

fn cell(text: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

/// A provider that answers from a script, one entry per provider call, and
/// records the debug form of every request it saw.
fn scripted_provider(
    kind: &'static str,
    script: Vec<String>,
    seen: Arc<Mutex<Vec<String>>>,
) -> lash::provider::ProviderHandle {
    let script = Arc::new(Mutex::new(script.into_iter()));
    lash::testing::TestProvider::builder()
        .kind(kind)
        .complete(move |request: lash::provider::LlmRequest| {
            let script = Arc::clone(&script);
            let seen = Arc::clone(&seen);
            async move {
                seen.lock_recover().push(format!("{request:?}"));
                let next = script.lock_recover().next();
                Ok(cell(next.as_deref().unwrap_or(
                    "<typescript>\nfinish(\"Your turn.\");\n</typescript>",
                )))
            }
        })
        .build()
        .into_handle()
}

async fn drive(state: &AppStateData, chat_id: &str, board: BoardState) -> Vec<serde_json::Value> {
    // Boxed for the same reason the replay test boxes: the handler future
    // is large enough to trip `clippy::large_futures` in a test frame.
    let response = Box::pin(send_message(
        State(state.clone()),
        AxumPath(chat_id.to_string()),
        test_remote_headers(),
        Json(SendMessageRequest {
            text: "I played X in the top left.".to_string(),
            board,
            model: None,
            model_variant: Default::default(),
        }),
    ))
    .await
    .expect("send message");
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    std::str::from_utf8(&body)
        .expect("utf8")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
        .collect()
}

fn system_messages(lines: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    lines
        .iter()
        .filter(|line| {
            line.get("type").and_then(serde_json::Value::as_str) == Some("message")
                && line
                    .pointer("/message/role")
                    .and_then(serde_json::Value::as_str)
                    == Some("system")
        })
        .collect()
}

/// FIG-3181, the wedge: the agent finishes twice without calling
/// `board.play`. The host must re-prompt once, then forfeit the move and
/// leave the board playable — `turn == "X"` is exactly the fact the UI's
/// `cell.disabled = ... || board.turn !== 'X' || ...` rule reads, so a
/// board that comes back X's is a board whose cells are clickable again.
#[tokio::test]
async fn a_turn_that_never_plays_leaves_the_board_playable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let data_dir = temp.path();
    let double = crate::state::test_support::test_double().await;
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let provider = scripted_provider(
        "agent-service-zero-move",
        vec![
            "<typescript>\nfinish(\"I already moved. Your turn.\");\n</typescript>".to_string(),
            "<typescript>\nfinish(\"I already moved. Your turn.\");\n</typescript>".to_string(),
        ],
        Arc::clone(&seen),
    );
    let core = test_core_with_provider(&double, provider).await;
    let state = test_state(
        &double,
        &core,
        AppDb::open(&data_dir.join("app.db")).expect("app db"),
    );
    let chat = state
        .with_db(|db| db.create_chat("wedged", "mock-model", None))
        .await
        .expect("create chat");

    let lines = drive(&state, &chat.id, board_owing_a_move()).await;

    let board = state
        .with_db({
            let chat_id = chat.id.clone();
            move |db| db.chat_board(&chat_id)
        })
        .await
        .expect("load board");
    assert_eq!(
        board.turn, "X",
        "a zero-move agent turn must hand the board back, not wedge it"
    );
    assert!(
        !crate::board::agent_owes_move(&board),
        "the board must owe nothing once the move is forfeited"
    );
    assert_eq!(
        board.cells,
        board_owing_a_move().cells,
        "no O may be invented on the agent's behalf"
    );

    let requests = seen.lock_recover().clone();
    assert_eq!(
        requests.len(),
        2,
        "the re-prompt is bounded at exactly one retry"
    );
    assert!(
        requests[1].contains("You finished your turn without playing"),
        "the retry must carry the explicit nudge"
    );

    let notices = system_messages(&lines);
    assert_eq!(notices.len(), 1, "one visible game error: {lines:#?}");
    assert_eq!(
        notices[0].pointer("/message/text"),
        Some(&json!(ZERO_MOVE_FORFEIT))
    );
    assert_eq!(
        notices[0].pointer("/message/payload/board/turn"),
        Some(&json!("X")),
        "the notice carries the yielded board so the UI re-enables the cells"
    );
}

/// The bound is a bound in both directions: a turn that does play spends no
/// retry and raises no game error.
#[tokio::test]
async fn a_turn_that_plays_spends_no_retry() {
    let temp = tempfile::tempdir().expect("tempdir");
    let data_dir = temp.path();
    let double = crate::state::test_support::test_double().await;
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let provider = scripted_provider(
        "agent-service-one-move",
        vec![
            "<typescript>\nawait board.play({ cell: 4 });\nfinish(\"I took the center. Your turn.\");\n</typescript>"
                .to_string(),
        ],
        Arc::clone(&seen),
    );
    let db = Arc::new(Mutex::new(
        AppDb::open(&data_dir.join("app.db")).expect("app db"),
    ));
    let core = crate::state::test_support::test_core_with_board(&double, provider, &db).await;
    let state = AppStateData::new(
        core,
        db,
        "mock-model".to_string(),
        None,
        double.connection(),
    );
    let chat = state
        .with_db(|db| db.create_chat("live", "mock-model", None))
        .await
        .expect("create chat");

    let lines = drive(&state, &chat.id, board_owing_a_move()).await;

    let board = state
        .with_db({
            let chat_id = chat.id.clone();
            move |db| db.chat_board(&chat_id)
        })
        .await
        .expect("load board");
    assert_eq!(board.turn, "X");
    assert_eq!(
        board.cells[4],
        Some("O".to_string()),
        "the agent's own move stands: {board:?}"
    );
    assert_eq!(
        seen.lock_recover().len(),
        1,
        "a turn that played must not be re-prompted"
    );
    assert!(
        system_messages(&lines).is_empty(),
        "no game error on a healthy turn: {lines:#?}"
    );
}

/// The recovery is host policy, not durability plumbing: it lives in one
/// helper the send route calls, so this drives that helper directly with a
/// turn runner that never plays.
#[tokio::test]
async fn the_zero_move_policy_is_one_shared_bounded_loop() {
    let temp = tempfile::tempdir().expect("tempdir");
    let data_dir = temp.path();
    let double = crate::state::test_support::test_double().await;
    let core = crate::state::test_support::test_core(&double).await;
    let state = test_state(
        &double,
        &core,
        AppDb::open(&data_dir.join("app.db")).expect("app db"),
    );
    let chat = state
        .with_db(|db| db.create_chat("shared", "mock-model", None))
        .await
        .expect("create chat");
    state
        .with_db({
            let chat_id = chat.id.clone();
            move |db| db.upsert_chat_board(&chat_id, &board_owing_a_move())
        })
        .await
        .expect("seed board");

    drop(
        state
            .open_session(
                &chat.id,
                ModelChoice {
                    key: "mock-model".into(),
                    reasoning: Default::default(),
                },
            )
            .await
            .expect("create the session the fake turn runner uses"),
    );

    let inputs = Arc::new(Mutex::new(Vec::<String>::new()));
    let emitted = Arc::new(Mutex::new(Vec::<StreamItem>::new()));
    let outcome = run_turn_with_zero_move_recovery(
        &state,
        &chat.id,
        "I played X in the top left.".to_string(),
        TurnId::from("shared-turn-1".to_string()),
        || TurnId::from("shared-turn-2".to_string()),
        |turn_input, _turn_id| {
            let inputs = Arc::clone(&inputs);
            async move {
                // A turn that plays nothing: the board is left untouched.
                inputs.lock_recover().push(turn_input);
                Ok(TurnAttempt::Completed)
            }
        },
        |item| {
            let emitted = Arc::clone(&emitted);
            async move {
                emitted.lock_recover().push(item);
            }
        },
    )
    .await
    .expect("recovery loop");

    assert!(matches!(outcome, TurnAttempt::Completed));
    let inputs = inputs.lock_recover().clone();
    assert_eq!(inputs.len(), 2, "exactly one re-prompt: {inputs:?}");
    assert_eq!(inputs[1], ZERO_MOVE_NUDGE, "the retry carries the nudge");

    let board = state
        .with_db({
            let chat_id = chat.id.clone();
            move |db| db.chat_board(&chat_id)
        })
        .await
        .expect("board");
    assert_eq!(board.turn, "X", "the board is handed back: {board:?}");
    assert!(!agent_owes_move(&board));
    assert_eq!(
        board.cells[4], None,
        "no move is invented on the agent's behalf: {board:?}"
    );

    let emitted = emitted.lock_recover().clone();
    let notices: Vec<&StreamItem> = emitted
        .iter()
        .filter(
            |item| matches!(item, StreamItem::Message { message } if message.role() == "system"),
        )
        .collect();
    assert_eq!(notices.len(), 1, "one forfeit notice: {emitted:#?}");
    let StreamItem::Message { message } = notices[0] else {
        unreachable!("filtered to messages");
    };
    assert_eq!(message.text(), ZERO_MOVE_FORFEIT);
}
