use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash::sync::MutexExt;
use lash::{
    PluginBinding,
    plugins::{PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, SessionPlugin},
    prompt::PromptContribution,
    tools::{
        LashlangToolBinding, StaticToolExecute, StaticToolProvider, ToolCall, ToolDefinition,
        ToolDefinitionLashlangExt, ToolResult,
    },
};
use serde_json::json;

use crate::board::{BoardState, board_prompt, board_snapshot};
use crate::db::AppDb;

#[derive(Clone, Debug)]
pub(crate) struct DemoPlugin;

#[derive(Clone)]
pub(crate) struct DemoPluginConfig {
    pub(crate) db: Arc<Mutex<AppDb>>,
}

impl PluginBinding for DemoPlugin {
    const ID: &'static str = "demo_tic_tac_toe";
    type SessionConfig = DemoPluginConfig;
    type Input = ();

    fn factory(config: &Self::SessionConfig) -> Arc<dyn PluginFactory> {
        Arc::new(DemoPluginFactory {
            db: Arc::clone(&config.db),
        })
    }
}

struct DemoPluginFactory {
    db: Arc<Mutex<AppDb>>,
}

impl PluginFactory for DemoPluginFactory {
    fn id(&self) -> &'static str {
        DemoPlugin::ID
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
        DemoPlugin::ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        let db = Arc::clone(&self.db);
        reg.prompt().contribute(Arc::new(move |ctx| {
            let db = Arc::clone(&db);
            Box::pin(async move {
                let board = load_chat_board_for_plugin(&db, &ctx.session_id)?;
                let context = board_prompt(&board);
                Ok(vec![PromptContribution::environment(
                    "Tic Tac Toe Board",
                    context,
                )])
            })
        }));
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

#[async_trait]
impl StaticToolExecute for DemoTools {
    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        match call.name {
            "read_board" => match load_chat_board_for_tool(&self.db, call.context.session_id()) {
                Ok(board) => ToolResult::ok(board_snapshot(&board)),
                Err(err) => ToolResult::err_fmt(err),
            },
            "play_move" => {
                let Some(cell) = call.args.get("cell").and_then(|value| value.as_u64()) else {
                    return ToolResult::err_fmt("missing integer cell");
                };
                match apply_agent_move_for_tool(&self.db, call.context.session_id(), cell as usize)
                {
                    Ok(output) => ToolResult::ok(output),
                    Err(err) => ToolResult::err_fmt(err),
                }
            }
            other => ToolResult::err_fmt(format!("unknown demo tool `{other}`")),
        }
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
    .with_lashlang_binding(LashlangToolBinding::new(["board"], "read"))
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
    .with_lashlang_binding(LashlangToolBinding::new(["board"], "play"))
}

fn load_chat_board_for_plugin(
    db: &Arc<Mutex<AppDb>>,
    chat_id: &str,
) -> Result<BoardState, PluginError> {
    let mut db = db.lock_recover();
    db.chat_board(chat_id)
        .map_err(|err| PluginError::Session(err.to_string()))
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
