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
                    match apply_agent_move_for_tool(&self.db, session_id, cell as usize) {
                        Ok(output) => ToolOutcome::ok(output),
                        Err(err) => ToolOutcome::err_fmt(err),
                    }
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
    )
    .with_tool_binding(ToolBinding::new(["board"], "read"))
}

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
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use lash::RuntimeOwner;
    use lash::ToolCallId;
    use lash::process::ProcessOriginator;
    use lash::tools::{ToolId, ToolPrepareContext};

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
