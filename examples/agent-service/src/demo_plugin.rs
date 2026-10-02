use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash::process::ProcessOriginator;
use lash::sync::MutexExt;
use lash::{
    plugins::{PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, SessionPlugin},
    tools::{
        PendingToolCall, PreparedToolCall, StaticToolExecute, StaticToolProvider,
        ToolAttemptOutcome, ToolBinding, ToolCall, ToolDefinition, ToolDefinitionBindingExt,
        ToolId, ToolOutcome, ToolPrepareContext,
    },
};
use serde_json::json;

use crate::board::{BoardState, board_snapshot};
use crate::db::AppDb;

const DEMO_PLUGIN_ID: &str = "demo_tic_tac_toe";

/// The demo's board plugin, installed once on the core: every chat session
/// and every process worker of the core runs it over the app's one database.
/// The board a session reads is its own chat's, named by the session id.
pub(crate) struct DemoPluginFactory {
    db: Arc<Mutex<AppDb>>,
}

impl DemoPluginFactory {
    pub(crate) fn new(db: Arc<Mutex<AppDb>>) -> Self {
        Self { db }
    }
}

impl PluginFactory for DemoPluginFactory {
    fn id(&self) -> &'static str {
        DEMO_PLUGIN_ID
    }

    fn declaration(&self) -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(self.id())
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(DemoSessionPlugin {
            db: Arc::clone(&self.db),
        }))
    }
}

struct DemoSessionPlugin {
    db: Arc<Mutex<AppDb>>,
}

impl SessionPlugin for DemoSessionPlugin {
    fn id(&self) -> &'static str {
        DEMO_PLUGIN_ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        reg.tools().provider(Arc::new(StaticToolProvider::new(
            demo_tool_definitions(),
            DemoTools {
                db: Arc::clone(&self.db),
            },
        )))?;
        Ok(())
    }
}

struct DemoTools {
    db: Arc<Mutex<AppDb>>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PreparedBoardCall {
    chat_id: lash::SessionId,
}

#[async_trait]
impl StaticToolExecute for DemoTools {
    async fn prepare_tool_call(
        &self,
        tool_id: &ToolId,
        pending: PendingToolCall,
        context: &ToolPrepareContext,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        let chat_id = match context.owner() {
            lash::RuntimeOwner::Session(session_id) => session_id,
            lash::RuntimeOwner::Process(_) => match context.process_originator() {
                Some(ProcessOriginator::Session { session_id, .. }) => session_id,
                _ => {
                    return Err(ToolOutcome::err_fmt(
                        "the board call has no originating chat",
                    ));
                }
            },
        };
        let payload = serde_json::to_value(PreparedBoardCall {
            chat_id: chat_id.clone(),
        })
        .map_err(ToolOutcome::err_fmt)?;
        Ok(PreparedToolCall::identity(tool_id.clone(), pending).with_prepared_payload(payload))
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        (async {
            let prepared = match call.context.decode_prepared_payload::<PreparedBoardCall>() {
                Ok(prepared) => prepared,
                Err(err) => return ToolOutcome::err_fmt(err),
            };
            let session_id = prepared.chat_id.as_str();
            match call.name() {
                "read_board" => match load_chat_board_for_tool(&self.db, session_id) {
                    Ok(board) => ToolOutcome::ok(board_snapshot(&board)),
                    Err(err) => ToolOutcome::err_fmt(err),
                },
                "play_move" => {
                    let Some(cell) = call.args.get("cell").and_then(|value| value.as_u64()) else {
                        return ToolOutcome::err_fmt("missing integer cell");
                    };
                    let output =
                        match apply_agent_move_for_tool(&self.db, session_id, cell as usize) {
                            Ok(output) => output,
                            Err(err) => return ToolOutcome::err_fmt(err),
                        };
                    if let Err(error) = record_board_context_for_tool(&self.db, session_id).await {
                        return ToolOutcome::err_fmt(error);
                    }
                    ToolOutcome::ok(output)
                }
                other => ToolOutcome::err_fmt(format!("unknown demo tool `{other}`")),
            }
        })
        .await
        .into()
    }
}

fn demo_tool_definitions() -> Vec<ToolDefinition> {
    vec![read_board_tool(), play_move_tool()]
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
fn read_board_tool() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:read_board",
        "read_board",
        "Read the app-owned Tic Tac Toe board. Returns the 0..8 index map, current marks by index, legal moves, winner, and whose turn it is.",
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        json!({ "type": "object" }),
    ).expect("valid declared tool schemas")
    .with_tool_binding(ToolBinding::new(["board"], "read"))
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
fn play_move_tool() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:play_move",
        "play_move",
        "Play one O move for the agent when it is O's turn. The move is a zero-based cell index: 0 top-left, 1 top-middle, 2 top-right, 3 middle-left, 4 center, 5 middle-right, 6 bottom-left, 7 bottom-middle, 8 bottom-right.",
        json!({
            "type": "object",
            "properties": { "cell": { "type": "integer", "minimum": 0, "maximum": 8 } },
            "required": ["cell"],
            "additionalProperties": false
        }),
        json!({ "type": "object" }),
    ).expect("valid declared tool schemas")
    .with_tool_binding(ToolBinding::new(["board"], "play"))
}

fn load_chat_board_for_tool(db: &Arc<Mutex<AppDb>>, chat_id: &str) -> Result<BoardState, String> {
    let mut db = db.lock_recover();
    db.chat_board(chat_id).map_err(|err| err.to_string())
}

fn apply_agent_move_for_tool(
    db: &Arc<Mutex<AppDb>>,
    chat_id: &str,
    cell: usize,
) -> Result<serde_json::Value, String> {
    let mut db = db.lock_recover();
    db.apply_agent_move(chat_id, cell)
        .map_err(|err| err.to_string())
}

async fn record_board_context_for_tool(
    db: &Arc<Mutex<AppDb>>,
    chat_id: &str,
) -> crate::state::AppResult<()> {
    let (core, board) = {
        let mut db = db.lock_recover();
        let core = db.context_core.upgrade().ok_or_else(|| {
            crate::state::AppError::internal("the board context host is unavailable")
        })?;
        (core, db.chat_board(chat_id)?)
    };
    let session = core
        .session(lash::SessionId::parse(chat_id)?)
        .enqueue_only()
        .open()
        .await?;
    let config = session.admin().config();
    let revision = config.revision().await?;
    // The move runs inside its root, so submission must not wait for the
    // command lane. The root keeps its render; the command precedes later work.
    config
        .submit(
            lash::config::ConfigWrite::new(
                format!("board-context:{}", uuid::Uuid::new_v4()),
                revision,
            ),
            lash::config::ConfigTransaction::of(lash::rlm::SetRlmPromptContext {
                context: vec![crate::board::board_prompt(&board)],
            }),
        )
        .await?;
    drop(session);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash::RuntimeOwner;
    use lash::ToolCallId;
    use lash::process::ProcessOriginator;
    use lash::tools::{ToolId, ToolPrepareContext};

    #[tokio::test]
    async fn a_non_user_turn_records_fresh_board_context_and_replays_it() {
        use axum::Json;
        use axum::extract::{Path as AxumPath, State};
        use lash::persistence::QueuedWorkStore as _;

        let temp = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Mutex::new(
            AppDb::open(&temp.path().join("app.db")).expect("app db"),
        ));
        let chat = db
            .lock_recover()
            .create_chat("context law", "mock-model", None)
            .expect("chat");
        let session_id = lash::SessionId::fixture(chat.id.clone());
        let first = BoardState {
            cells: vec![None; 9],
            turn: "O".to_string(),
        };
        let mut moved = first.clone();
        moved.cells[4] = Some("O".to_string());
        moved.turn = "X".to_string();
        let requests = Arc::new(Mutex::new(Vec::<lash::provider::LlmRequest>::new()));
        let double = crate::state::test_support::test_double().await;
        let provider = lash::testing::TestProvider::builder()
            .kind("board-context-law")
            .complete({
                let requests = Arc::clone(&requests);
                let double = double.clone();
                let db = Arc::clone(&db);
                let chat_id = chat.id.clone();
                move |request| {
                    let mut seen = requests.lock_recover();
                    seen.push(request);
                    let answer = if seen.len() == 1 {
                        "<typescript>await board.play({ cell: 4 }); finish(\"moved\");</typescript>"
                    } else {
                        if seen.len() == 2 {
                            db.lock_recover()
                                .upsert_chat_board(&chat_id, &crate::board::default_board())
                                .expect("change live data before replay");
                            double.crash_turn_drive(
                                lash_restate_test::CrashPoint::BeforeRunResult { name: None },
                            );
                        }
                        "<typescript>finish(\"observed\");</typescript>"
                    };
                    async move {
                        Ok(lash::provider::LlmResponse {
                            parts: vec![lash::direct::LlmOutputPart::Text {
                                text: answer.to_string(),
                                response_meta: None,
                            }],
                            ..Default::default()
                        })
                    }
                }
            })
            .build()
            .into_handle();
        let core = crate::state::test_support::test_core_with_board(&double, provider, &db).await;
        let state = crate::state::AppStateData::new(
            core,
            Arc::clone(&db),
            "mock-model".to_string(),
            None,
            double.connection(),
        );
        let response = crate::routes::send_message(
            State(state.clone()),
            AxumPath(chat.id.clone()),
            crate::remote_protocol::test_remote_headers(),
            Json(
                serde_json::from_value(
                    json!({ "text": "play", "board": first, "model": null, "model_variant": null }),
                )
                .expect("send request"),
            ),
        )
        .await
        .expect("user board update");
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("user turn ends");
        double.settle_session_drive(&session_id).await;
        assert_eq!(
            db.lock_recover().chat_board(&chat.id).expect("board"),
            moved,
            "the tool mutated the board"
        );
        let crashes = double.server().stats().crashes;
        let process_id = lash::ProcessId::fixture("board-context-process");
        let wake = lash::process::ProcessWakeDelivery {
            version: lash::formats::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
            target_session_id: session_id.clone(),
            process_id: process_id.clone(),
            sequence: 1,
            event_type: "process.wake".to_string(),
            process_caused_by: None,
            authority: Default::default(),
            input: "observe the board".to_string(),
            created_at_ms: 1,
        };
        double
            .stores()
            .session_store_factory()
            .enqueue_queued_work(
                lash::persistence::QueuedWorkBatchDraft::new(
                    session_id.clone(),
                    lash::persistence::DeliveryPolicy::EarliestSafeBoundary,
                    lash::persistence::QueuedWorkPayload::process_wake(wake),
                )
                .with_source_key(lash::process::process_wake_source_key(&process_id, 1))
                .with_process_wake_source(process_id, 1),
            )
            .await
            .expect("enqueue process wake");
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            double.attach_drive(
                &session_id,
                lash::restate::DriveRequestId::new("board-context-drive"),
            ),
        )
        .await
        .expect("drive answers")
        .expect("drive wake");
        assert_eq!(
            double.server().stats().crashes,
            crashes + 1,
            "one root attempt crashed"
        );
        let seen = requests.lock_recover();
        assert_eq!(seen.len(), 3, "user call, non-user call, and redrive");
        assert!(
            seen[0]
                .instructions
                .as_deref()
                .unwrap_or_default()
                .contains(&crate::board::board_prompt(&first)),
            "user board update must set recorded prompt context"
        );
        assert!(
            seen[1]
                .instructions
                .as_deref()
                .unwrap_or_default()
                .contains(&crate::board::board_prompt(&moved)),
            "non-user turn must render the tool's fresh board context"
        );
        assert_eq!(
            seen[1].instructions, seen[2].instructions,
            "redrive must reuse its recorded render after live data changed"
        );
        assert!(
            !seen[0]
                .messages
                .iter()
                .any(|message| format!("{message:?}").contains("## Tic Tac Toe Board")),
            "board context belongs to recorded config"
        );
    }

    fn pending() -> PendingToolCall {
        PendingToolCall {
            call_id: ToolCallId::fixture("board-read"),
            provider_call_id: None,
            tool_name: "read_board".to_string(),
            args: json!({}),
            replay: None,
        }
    }

    #[tokio::test]
    async fn a_process_board_call_is_bound_to_its_originating_chat() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tools = DemoTools {
            db: Arc::new(Mutex::new(
                AppDb::open(&temp.path().join("app.db")).expect("app db"),
            )),
        };
        let context = ToolPrepareContext::for_testing(
            RuntimeOwner::Process(lash::ProcessId::fixture("process-without-a-session")),
            Arc::new(lash::testing::MockSessionManager::default()),
            Some(ProcessOriginator::Session {
                session_id: "originating-chat".into(),
                agent_frame_id: None,
            }),
        );
        let call = tools
            .prepare_tool_call(&ToolId::new("tool:read_board"), pending(), &context)
            .await
            .expect("prepare board read");
        assert_eq!(
            call.prepared_payload,
            json!({"chat_id": "originating-chat"})
        );
    }

    #[tokio::test]
    async fn a_host_process_has_no_implicit_chat_board() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tools = DemoTools {
            db: Arc::new(Mutex::new(
                AppDb::open(&temp.path().join("app.db")).expect("app db"),
            )),
        };
        let context = ToolPrepareContext::for_testing(
            RuntimeOwner::Process(lash::ProcessId::fixture("host-process")),
            Arc::new(lash::testing::MockSessionManager::default()),
            Some(ProcessOriginator::host()),
        );
        assert!(
            tools
                .prepare_tool_call(&ToolId::new("tool:read_board"), pending(), &context)
                .await
                .is_err()
        );
    }
}
