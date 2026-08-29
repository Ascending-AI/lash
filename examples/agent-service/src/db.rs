use std::path::Path;

use axum::http::StatusCode;
use lash::TurnEvent;
use lash_remote_protocol::RemoteTurnEvent;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::json;

use crate::board::{BoardState, apply_agent_move, default_board};
use crate::state::{AppError, AppResult};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ChatSummary {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) model: String,
    pub(crate) model_variant: Option<String>,
    pub(crate) model_label: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ChatMessage {
    pub(crate) id: i64,
    pub(crate) chat_id: String,
    pub(crate) kind: String,
    pub(crate) role: String,
    pub(crate) text: String,
    pub(crate) payload: Option<serde_json::Value>,
    pub(crate) created_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ChatBranchPoint {
    pub(crate) node_id: String,
    pub(crate) source_chat_id: String,
    pub(crate) message_count: usize,
    pub(crate) created_at: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ChatModelSelection {
    pub(crate) model: String,
    pub(crate) model_variant: Option<String>,
}

#[cfg(feature = "restate")]
#[derive(Debug)]
pub(crate) struct TurnOutboxEvent {
    pub(crate) id: i64,
    pub(crate) item_json: String,
    pub(crate) is_done: bool,
}

pub(crate) struct AppDb {
    conn: Connection,
}

impl AppDb {
    pub(crate) fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS chats (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                model TEXT NOT NULL,
                model_variant TEXT,
                fork_pending INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
                kind TEXT NOT NULL DEFAULT 'message',
                role TEXT NOT NULL,
                text TEXT NOT NULL,
                payload TEXT,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_chat_id_id
                ON messages(chat_id, id);
            CREATE TABLE IF NOT EXISTS chat_boards (
                chat_id TEXT PRIMARY KEY REFERENCES chats(id) ON DELETE CASCADE,
                board_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS code_block_tool_calls (
                code_block_message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
                tool_call_message_id INTEGER REFERENCES messages(id) ON DELETE SET NULL,
                call_id TEXT NOT NULL,
                PRIMARY KEY (code_block_message_id, call_id)
            );
            CREATE INDEX IF NOT EXISTS idx_code_block_tool_calls_tool_message
                ON code_block_tool_calls(tool_call_message_id);
            CREATE TABLE IF NOT EXISTS turn_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                turn_id TEXT NOT NULL,
                item_json TEXT NOT NULL,
                is_done INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_turn_events_turn_id_id
                ON turn_events(turn_id, id);
            CREATE TABLE IF NOT EXISTS chat_branch_points (
                source_chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
                node_id TEXT NOT NULL,
                message_id INTEGER NOT NULL,
                board_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY (source_chat_id, node_id)
            );
            CREATE INDEX IF NOT EXISTS idx_chat_branch_points_source_created
                ON chat_branch_points(source_chat_id, created_at DESC);
            ",
        )?;
        add_column_if_missing(&conn, "chats", "model_variant", "TEXT")?;
        add_column_if_missing(&conn, "chats", "fork_pending", "INTEGER NOT NULL DEFAULT 0")?;
        add_column_if_missing(&conn, "messages", "kind", "TEXT NOT NULL DEFAULT 'message'")?;
        add_column_if_missing(&conn, "messages", "payload", "TEXT")?;
        let legacy_model_label: Option<String> = conn
            .query_row(
                "SELECT model FROM chats
                 WHERE model_variant IS NULL
                   AND model GLOB '* (*)'
                   AND substr(model, -1) = ')'
                 LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(model) = legacy_model_label {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "legacy chat model label `{model}` requires separate model and model_variant columns"
                    ),
                )),
            ));
        }
        Ok(Self { conn })
    }

    pub(crate) fn insert_turn_event<T: Serialize>(
        &mut self,
        turn_id: &str,
        item: &T,
        is_done: bool,
    ) -> AppResult<()> {
        let item_json =
            serde_json::to_string(item).map_err(|err| AppError::internal(err.to_string()))?;
        self.conn.execute(
            "INSERT INTO turn_events (turn_id, item_json, is_done, created_at)
             VALUES (?1, ?2, ?3, datetime('now'))",
            params![turn_id, item_json, is_done as i64],
        )?;
        Ok(())
    }

    #[cfg(feature = "restate")]
    pub(crate) fn list_turn_events_after(
        &mut self,
        turn_id: &str,
        last_id: i64,
    ) -> AppResult<Vec<TurnOutboxEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, item_json, is_done
             FROM turn_events
             WHERE turn_id = ?1 AND id > ?2
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![turn_id, last_id], |row| {
            Ok(TurnOutboxEvent {
                id: row.get(0)?,
                item_json: row.get(1)?,
                is_done: row.get::<_, i64>(2)? != 0,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(AppError::from)
    }

    pub(crate) fn list_chats(&mut self) -> AppResult<Vec<ChatSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, created_at, updated_at, model, model_variant
             FROM chats WHERE fork_pending = 0 ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], chat_summary_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(AppError::from)
    }

    pub(crate) fn create_chat(
        &mut self,
        title: &str,
        model: &str,
        model_variant: Option<&str>,
    ) -> AppResult<ChatSummary> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now();
        self.conn.execute(
            "INSERT INTO chats (id, title, created_at, updated_at, model, model_variant)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, title, now, now, model, model_variant],
        )?;
        Ok(self.chat(&id)?)
    }

    pub(crate) fn save_branch_point(
        &mut self,
        source_chat_id: &str,
        node_id: &str,
    ) -> AppResult<ChatBranchPoint> {
        self.require_chat(source_chat_id)?;
        let message_id = self.conn.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM messages WHERE chat_id = ?1",
            params![source_chat_id],
            |row| row.get::<_, i64>(0),
        )?;
        let board_json = serde_json::to_string(&self.chat_board(source_chat_id)?)
            .map_err(|err| AppError::internal(err.to_string()))?;
        let created_at = now();
        self.conn.execute(
            "INSERT INTO chat_branch_points
             (node_id, source_chat_id, message_id, board_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(source_chat_id, node_id) DO UPDATE SET
                message_id = excluded.message_id,
                board_json = excluded.board_json,
                created_at = excluded.created_at",
            params![node_id, source_chat_id, message_id, board_json, created_at],
        )?;
        self.branch_point(source_chat_id, node_id)
    }

    pub(crate) fn list_branch_points(
        &mut self,
        source_chat_id: &str,
    ) -> AppResult<Vec<ChatBranchPoint>> {
        self.require_chat(source_chat_id)?;
        let mut stmt = self.conn.prepare(
            "SELECT point.node_id, point.source_chat_id,
                    COUNT(message.id), point.created_at
             FROM chat_branch_points point
             LEFT JOIN messages message
               ON message.chat_id = point.source_chat_id
              AND message.id <= point.message_id
             WHERE point.source_chat_id = ?1
             GROUP BY point.node_id, point.source_chat_id, point.created_at
             ORDER BY point.created_at DESC",
        )?;
        let points = stmt.query_map(params![source_chat_id], branch_point_from_row)?;
        points
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(AppError::from)
    }

    pub(crate) fn prepare_chat_fork(
        &mut self,
        source_chat_id: &str,
        node_id: &str,
        target_chat_id: &str,
    ) -> AppResult<()> {
        let (message_id, board_json) = self
            .conn
            .query_row(
                "SELECT message_id, board_json
                 FROM chat_branch_points
                 WHERE source_chat_id = ?1 AND node_id = ?2",
                params![source_chat_id, node_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| AppError {
                status: StatusCode::NOT_FOUND,
                message: format!("branch point `{node_id}` not found"),
            })?;
        let source = self.chat(source_chat_id)?;
        let title = format!("{} · branch", source.title);
        let created_at = now();
        let transaction = self.conn.transaction()?;
        transaction.execute(
            "INSERT INTO chats
             (id, title, created_at, updated_at, model, model_variant, fork_pending)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5, 1)",
            params![
                target_chat_id,
                title,
                created_at,
                source.model,
                source.model_variant
            ],
        )?;
        transaction.execute(
            "INSERT INTO messages (chat_id, kind, role, text, payload, created_at)
             SELECT ?1, kind, role, text, payload, created_at
             FROM messages
             WHERE chat_id = ?2 AND id <= ?3
             ORDER BY id",
            params![target_chat_id, source_chat_id, message_id],
        )?;
        transaction.execute(
            "INSERT INTO chat_boards (chat_id, board_json, updated_at)
             VALUES (?1, ?2, ?3)",
            params![target_chat_id, board_json, created_at],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn finish_chat_fork(&mut self, target_chat_id: &str) -> AppResult<ChatSummary> {
        let changed = self.conn.execute(
            "UPDATE chats SET fork_pending = 0 WHERE id = ?1 AND fork_pending = 1",
            params![target_chat_id],
        )?;
        if changed != 1 {
            return Err(AppError::internal(format!(
                "pending branch chat `{target_chat_id}` was not found"
            )));
        }
        Ok(self.chat(target_chat_id)?)
    }

    pub(crate) fn pending_chat_forks(&mut self) -> AppResult<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM chats WHERE fork_pending = 1 ORDER BY id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(AppError::from)
    }

    pub(crate) fn delete_chat(&mut self, chat_id: &str) -> AppResult<()> {
        self.conn
            .execute("DELETE FROM chats WHERE id = ?1", params![chat_id])?;
        Ok(())
    }

    pub(crate) fn require_chat(&mut self, chat_id: &str) -> AppResult<()> {
        if self.chat(chat_id).optional()?.is_some() {
            return Ok(());
        }
        Err(AppError {
            status: StatusCode::NOT_FOUND,
            message: format!("chat `{chat_id}` not found"),
        })
    }

    fn chat(&mut self, chat_id: &str) -> rusqlite::Result<ChatSummary> {
        self.conn.query_row(
            "SELECT id, title, created_at, updated_at, model, model_variant
             FROM chats WHERE id = ?1 AND fork_pending = 0",
            params![chat_id],
            chat_summary_from_row,
        )
    }

    pub(crate) fn update_chat_model(
        &mut self,
        chat_id: &str,
        model: &str,
        model_variant: Option<&str>,
    ) -> AppResult<ChatSummary> {
        let changed = self.conn.execute(
            "UPDATE chats SET model = ?1, model_variant = ?2, updated_at = ?3 WHERE id = ?4",
            params![model, model_variant, now(), chat_id],
        )?;
        if changed == 0 {
            return Err(AppError {
                status: StatusCode::NOT_FOUND,
                message: format!("chat `{chat_id}` not found"),
            });
        }
        Ok(self.chat(chat_id)?)
    }

    pub(crate) fn chat_model_selection(&mut self, chat_id: &str) -> AppResult<ChatModelSelection> {
        self.conn
            .query_row(
                "SELECT model, model_variant FROM chats WHERE id = ?1",
                params![chat_id],
                |row| {
                    Ok(ChatModelSelection {
                        model: row.get(0)?,
                        model_variant: row.get(1)?,
                    })
                },
            )
            .map_err(AppError::from)
    }

    pub(crate) fn maybe_title_from_first_message(
        &mut self,
        chat_id: &str,
        text: &str,
    ) -> AppResult<()> {
        let title: String = self.conn.query_row(
            "SELECT title FROM chats WHERE id = ?1",
            params![chat_id],
            |row| row.get(0),
        )?;
        if title != "New chat" {
            return Ok(());
        }
        let message_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE chat_id = ?1",
            params![chat_id],
            |row| row.get(0),
        )?;
        if message_count == 0 {
            self.conn.execute(
                "UPDATE chats SET title = ?1, updated_at = ?2 WHERE id = ?3",
                params![compact_title(text), now(), chat_id],
            )?;
        }
        Ok(())
    }

    pub(crate) fn upsert_chat_board(&mut self, chat_id: &str, board: &BoardState) -> AppResult<()> {
        let board_json =
            serde_json::to_string(board).map_err(|err| AppError::internal(err.to_string()))?;
        self.conn.execute(
            "INSERT INTO chat_boards (chat_id, board_json, updated_at)
             VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(chat_id) DO UPDATE SET
                board_json = excluded.board_json,
                updated_at = excluded.updated_at",
            params![chat_id, board_json],
        )?;
        Ok(())
    }

    pub(crate) fn chat_board(&mut self, chat_id: &str) -> AppResult<BoardState> {
        let board_json: Option<String> = self
            .conn
            .query_row(
                "SELECT board_json FROM chat_boards WHERE chat_id = ?1",
                params![chat_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(board_json) = board_json else {
            return Ok(default_board());
        };
        serde_json::from_str(&board_json).map_err(|err| AppError::internal(err.to_string()))
    }

    pub(crate) fn apply_agent_move(
        &mut self,
        chat_id: &str,
        cell: usize,
    ) -> AppResult<serde_json::Value> {
        let board = self.chat_board(chat_id)?;
        let output = apply_agent_move(&board, cell);
        if !output
            .get("accepted")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            let reason = output
                .get("reason")
                .and_then(|value| value.as_str())
                .unwrap_or("agent move was rejected");
            return Err(AppError::bad_request(reason));
        }
        let next_board_value = output
            .get("board")
            .ok_or_else(|| AppError::internal("accepted move missing board"))?;
        let next_board: BoardState = serde_json::from_value(next_board_value.clone())
            .map_err(|err| AppError::internal(err.to_string()))?;
        self.upsert_chat_board(chat_id, &next_board)?;
        Ok(output)
    }

    pub(crate) fn insert_message(
        &mut self,
        chat_id: &str,
        role: &str,
        text: &str,
    ) -> AppResult<ChatMessage> {
        self.insert_message_with_payload(chat_id, role, text, None)
    }

    pub(crate) fn insert_message_with_payload(
        &mut self,
        chat_id: &str,
        role: &str,
        text: &str,
        payload: Option<serde_json::Value>,
    ) -> AppResult<ChatMessage> {
        let created_at = now();
        let payload_text = payload.as_ref().map(serde_json::Value::to_string);
        self.conn.execute(
            "INSERT INTO messages (chat_id, kind, role, text, payload, created_at)
             VALUES (?1, 'message', ?2, ?3, ?4, ?5)",
            params![chat_id, role, text, payload_text, created_at],
        )?;
        self.conn.execute(
            "UPDATE chats SET updated_at = ?1 WHERE id = ?2",
            params![now(), chat_id],
        )?;
        let id = self.conn.last_insert_rowid();
        Ok(ChatMessage {
            id,
            chat_id: chat_id.to_string(),
            kind: "message".to_string(),
            role: role.to_string(),
            text: text.to_string(),
            payload,
            created_at,
        })
    }

    pub(crate) fn insert_reasoning(&mut self, chat_id: &str, text: &str) -> AppResult<ChatMessage> {
        let created_at = now();
        self.conn.execute(
            "INSERT INTO messages (chat_id, kind, role, text, payload, created_at)
             VALUES (?1, 'reasoning', 'assistant', ?2, NULL, ?3)",
            params![chat_id, text, created_at],
        )?;
        self.conn.execute(
            "UPDATE chats SET updated_at = ?1 WHERE id = ?2",
            params![now(), chat_id],
        )?;
        let id = self.conn.last_insert_rowid();
        Ok(ChatMessage {
            id,
            chat_id: chat_id.to_string(),
            kind: "reasoning".to_string(),
            role: "assistant".to_string(),
            text: text.to_string(),
            payload: None,
            created_at,
        })
    }

    pub(crate) fn update_reasoning(&mut self, id: i64, text: &str) -> AppResult<ChatMessage> {
        self.conn.execute(
            "UPDATE messages SET text = ?1 WHERE id = ?2 AND kind = 'reasoning'",
            params![text, id],
        )?;
        self.message_by_id(id)
    }

    pub(crate) fn insert_tool_call(
        &mut self,
        chat_id: &str,
        event: TurnEvent,
    ) -> AppResult<ChatMessage> {
        let payload = tool_payload(event)?;
        let created_at = now();
        let tool_name = payload["name"].as_str().unwrap_or("tool").to_string();
        self.conn.execute(
            "INSERT INTO messages (chat_id, kind, role, text, payload, created_at)
             VALUES (?1, 'tool_call', 'tool', ?2, ?3, ?4)",
            params![chat_id, tool_name, payload.to_string(), created_at],
        )?;
        self.conn.execute(
            "UPDATE chats SET updated_at = ?1 WHERE id = ?2",
            params![now(), chat_id],
        )?;
        let id = self.conn.last_insert_rowid();
        Ok(ChatMessage {
            id,
            chat_id: chat_id.to_string(),
            kind: "tool_call".to_string(),
            role: "tool".to_string(),
            text: tool_name,
            payload: Some(payload),
            created_at,
        })
    }

    pub(crate) fn update_tool_call(&mut self, id: i64, event: TurnEvent) -> AppResult<ChatMessage> {
        let payload = tool_payload(event)?;
        let tool_name = payload["name"].as_str().unwrap_or("tool").to_string();
        self.conn.execute(
            "UPDATE messages SET text = ?1, payload = ?2
             WHERE id = ?3 AND kind = 'tool_call'",
            params![tool_name, payload.to_string(), id],
        )?;
        self.message_by_id(id)
    }

    pub(crate) fn insert_code_block(
        &mut self,
        chat_id: &str,
        event: TurnEvent,
        code: Option<String>,
    ) -> AppResult<ChatMessage> {
        let (payload, tool_call_ids) = match event {
            TurnEvent::CodeBlockStarted {
                language,
                code,
                graph_key,
            } => (
                json!({
                    "phase": "started",
                    "language": language,
                    "code": code,
                    "graph_key": graph_key,
                }),
                Vec::new(),
            ),
            TurnEvent::CodeBlockCompleted {
                language,
                output,
                error,
                success,
                duration_ms,
                tool_call_ids,
                graph_key,
            } => (
                json!({
                    "phase": "completed",
                    "language": language,
                    "output": output,
                    "error": error,
                    "success": success,
                    "duration_ms": duration_ms,
                    "tool_call_ids": tool_call_ids,
                    "code": code,
                    "graph_key": graph_key,
                }),
                tool_call_ids,
            ),
            _ => return Err(AppError::internal("expected code-block event")),
        };
        let created_at = now();
        let language = payload["language"].as_str().unwrap_or("code").to_string();
        self.conn.execute(
            "INSERT INTO messages (chat_id, kind, role, text, payload, created_at)
             VALUES (?1, 'code_block', 'assistant', ?2, ?3, ?4)",
            params![chat_id, language, payload.to_string(), created_at],
        )?;
        self.conn.execute(
            "UPDATE chats SET updated_at = ?1 WHERE id = ?2",
            params![now(), chat_id],
        )?;
        let id = self.conn.last_insert_rowid();
        self.replace_code_block_tool_links(chat_id, id, &tool_call_ids)?;
        Ok(ChatMessage {
            id,
            chat_id: chat_id.to_string(),
            kind: "code_block".to_string(),
            role: "assistant".to_string(),
            text: language,
            payload: Some(payload),
            created_at,
        })
    }

    pub(crate) fn update_code_block(
        &mut self,
        id: i64,
        event: TurnEvent,
        code: Option<String>,
    ) -> AppResult<ChatMessage> {
        let (payload, tool_call_ids) = match event {
            TurnEvent::CodeBlockCompleted {
                language,
                output,
                error,
                success,
                duration_ms,
                tool_call_ids,
                graph_key,
            } => (
                json!({
                    "phase": "completed",
                    "language": language,
                    "output": output,
                    "error": error,
                    "success": success,
                    "duration_ms": duration_ms,
                    "tool_call_ids": tool_call_ids,
                    "code": code,
                    "graph_key": graph_key,
                }),
                tool_call_ids,
            ),
            _ => return Err(AppError::internal("expected code-block event")),
        };
        let language = payload["language"].as_str().unwrap_or("code").to_string();
        self.conn.execute(
            "UPDATE messages SET text = ?1, payload = ?2
             WHERE id = ?3 AND kind = 'code_block'",
            params![language, payload.to_string(), id],
        )?;
        let chat_id = self
            .conn
            .query_row(
                "SELECT chat_id FROM messages WHERE id = ?1 AND kind = 'code_block'",
                params![id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| AppError::internal("code block message not found"))?;
        self.replace_code_block_tool_links(&chat_id, id, &tool_call_ids)?;
        self.message_by_id(id)
    }

    pub(crate) fn list_messages(&mut self, chat_id: &str) -> AppResult<Vec<ChatMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, chat_id, kind, role, text, payload, created_at
             FROM messages WHERE chat_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![chat_id], chat_message_from_row)?;
        let mut messages = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(AppError::from)?;
        drop(stmt);
        self.hydrate_code_block_tool_links(&mut messages)?;
        Ok(messages)
    }

    fn message_by_id(&mut self, id: i64) -> AppResult<ChatMessage> {
        self.conn
            .query_row(
                "SELECT id, chat_id, kind, role, text, payload, created_at
                 FROM messages WHERE id = ?1",
                params![id],
                chat_message_from_row,
            )
            .map_err(AppError::from)
    }

    fn replace_code_block_tool_links(
        &mut self,
        chat_id: &str,
        code_block_message_id: i64,
        tool_call_ids: &[String],
    ) -> AppResult<()> {
        self.conn.execute(
            "DELETE FROM code_block_tool_calls WHERE code_block_message_id = ?1",
            params![code_block_message_id],
        )?;
        for call_id in tool_call_ids {
            let tool_message_id = self
                .find_tool_call_message_id(chat_id, call_id)
                .map_err(AppError::from)?;
            self.conn.execute(
                "INSERT OR REPLACE INTO code_block_tool_calls
                 (code_block_message_id, tool_call_message_id, call_id)
                 VALUES (?1, ?2, ?3)",
                params![code_block_message_id, tool_message_id, call_id],
            )?;
        }
        Ok(())
    }

    fn find_tool_call_message_id(
        &mut self,
        chat_id: &str,
        call_id: &str,
    ) -> rusqlite::Result<Option<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, payload FROM messages
             WHERE chat_id = ?1 AND kind = 'tool_call'
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![chat_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (id, payload) = row?;
            let Some(payload) = payload else {
                continue;
            };
            let Ok(payload) = serde_json::from_str::<serde_json::Value>(&payload) else {
                continue;
            };
            if payload.get("call_id").and_then(|value| value.as_str()) == Some(call_id) {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    fn hydrate_code_block_tool_links(&mut self, messages: &mut [ChatMessage]) -> AppResult<()> {
        for message in messages.iter_mut() {
            if message.kind != "code_block" {
                continue;
            }
            let Some(payload) = message.payload.as_mut() else {
                continue;
            };
            let mut stmt = self.conn.prepare(
                "SELECT call_id FROM code_block_tool_calls
                 WHERE code_block_message_id = ?1
                 ORDER BY rowid ASC",
            )?;
            let call_ids = stmt
                .query_map(params![message.id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if !call_ids.is_empty()
                && let Some(object) = payload.as_object_mut()
            {
                object.insert("tool_call_ids".to_string(), json!(call_ids));
            }
        }
        Ok(())
    }

    fn branch_point(&mut self, source_chat_id: &str, node_id: &str) -> AppResult<ChatBranchPoint> {
        self.conn
            .query_row(
                "SELECT point.node_id, point.source_chat_id,
                        COUNT(message.id), point.created_at
                 FROM chat_branch_points point
                 LEFT JOIN messages message
                   ON message.chat_id = point.source_chat_id
                  AND message.id <= point.message_id
                 WHERE point.source_chat_id = ?1 AND point.node_id = ?2
                 GROUP BY point.node_id, point.source_chat_id, point.created_at",
                params![source_chat_id, node_id],
                branch_point_from_row,
            )
            .map_err(AppError::from)
    }
}

fn tool_payload(event: TurnEvent) -> AppResult<serde_json::Value> {
    let event =
        RemoteTurnEvent::try_from(event).map_err(|error| AppError::internal(error.to_string()))?;
    match event {
        RemoteTurnEvent::ToolCallStarted {
            call_id,
            name,
            args,
            ..
        } => Ok(json!({
            "phase": "started",
            "call_id": call_id,
            "name": name,
            "args": args,
        })),
        RemoteTurnEvent::ToolCallCompleted {
            call_id,
            name,
            args,
            output,
            duration_ms,
            ..
        } => Ok(json!({
            "phase": "completed",
            "call_id": call_id,
            "name": name,
            "args": args,
            "output": output,
            "duration_ms": duration_ms,
        })),
        _ => Err(AppError::internal("expected tool-call event")),
    }
}

fn chat_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChatSummary> {
    let model: String = row.get(4)?;
    let model_variant: Option<String> = row.get(5)?;
    let model_label = model_label(&model, model_variant.as_deref());
    Ok(ChatSummary {
        id: row.get(0)?,
        title: row.get(1)?,
        created_at: row.get(2)?,
        updated_at: row.get(3)?,
        model,
        model_variant,
        model_label,
    })
}

fn chat_message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChatMessage> {
    let kind: String = row.get(2)?;
    let payload: Option<String> = row.get(5)?;
    let mut payload = payload
        .map(|value| {
            serde_json::from_str(&value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()?;
    if kind == "tool_call"
        && let Some(payload) = payload.as_mut()
    {
        normalize_legacy_tool_result(payload);
    }
    Ok(ChatMessage {
        id: row.get(0)?,
        chat_id: row.get(1)?,
        kind,
        role: row.get(3)?,
        text: row.get(4)?,
        payload,
        created_at: row.get(6)?,
    })
}

fn normalize_legacy_tool_result(payload: &mut serde_json::Value) {
    let Some(result) = payload.pointer_mut("/output/outcome/payload") else {
        return;
    };
    let serde_json::Value::Object(wrapper) = result else {
        return;
    };
    if wrapper.len() != 2
        || wrapper.get("$lash_tool_value").and_then(|tag| tag.as_str()) != Some("untrusted_json")
    {
        return;
    }
    let Some(value) = wrapper.get("value").cloned() else {
        return;
    };
    *result = value;
}

fn branch_point_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChatBranchPoint> {
    Ok(ChatBranchPoint {
        node_id: row.get(0)?,
        source_chat_id: row.get(1)?,
        message_count: row.get::<_, i64>(2)? as usize,
        created_at: row.get(3)?,
    })
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for existing in columns {
        if existing? == column {
            return Ok(());
        }
    }
    conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
        [],
    )?;
    Ok(())
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn model_label(model: &str, model_variant: Option<&str>) -> String {
    match model_variant
        .map(str::trim)
        .filter(|variant| !variant.is_empty())
    {
        Some(variant) => format!("{model} ({variant})"),
        None => model.to_string(),
    }
}

fn compact_title(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title = text.chars().take(54).collect::<String>();
    if text.chars().count() > 54 {
        title.push_str("...");
    }
    if title.is_empty() {
        "New chat".to_string()
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    #[test]
    fn replayed_tool_result_matches_live_payload_shape() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut db = AppDb::open(&temp.path().join("app.db")).expect("open db");
        let chat = db
            .create_chat("tool replay", "mock-model", None)
            .expect("create chat");
        db.insert_tool_call(
            &chat.id,
            TurnEvent::ToolCallCompleted {
                call_id: Some("move-1".to_string()),
                name: "play_move".to_string(),
                args: json!({ "cell": 4 }),
                output: lash::tools::ToolCallOutput::success(json!({
                    "accepted": true,
                    "move": { "cell": 4 },
                    "board": {
                        "cells": [null, null, null, null, "O", null, null, null, null],
                        "turn": "X"
                    }
                })),
                duration_ms: 7,
                graph_key: None,
                parent_call_id: None,
            },
        )
        .expect("persist completed tool call");

        let messages = db.list_messages(&chat.id).expect("replay transcript");
        let replayed_result =
            &messages[0].payload.as_ref().expect("tool payload")["output"]["outcome"]["payload"];

        assert_eq!(
            replayed_result,
            &json!({
                "accepted": true,
                "move": { "cell": 4 },
                "board": {
                    "cells": [null, null, null, null, "O", null, null, null, null],
                    "turn": "X"
                }
            }),
            "persisted replay must expose the same bare tool result as the live stream"
        );
    }

    #[test]
    fn legacy_wrapped_tool_result_replays_as_live_payload_shape() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut db = AppDb::open(&temp.path().join("app.db")).expect("open db");
        let chat = db
            .create_chat("legacy tool replay", "mock-model", None)
            .expect("create chat");
        let legacy_payload = json!({
            "phase": "completed",
            "call_id": "move-legacy",
            "name": "play_move",
            "args": { "cell": 4 },
            "output": {
                "outcome": {
                    "status": "success",
                    "payload": {
                        "$lash_tool_value": "untrusted_json",
                        "value": { "accepted": true, "move": { "cell": 4 } }
                    }
                }
            },
            "duration_ms": 7
        });
        db.conn
            .execute(
                "INSERT INTO messages (chat_id, kind, role, text, payload, created_at)
                 VALUES (?1, 'tool_call', 'tool', 'play_move', ?2, ?3)",
                params![&chat.id, legacy_payload.to_string(), now()],
            )
            .expect("insert legacy tool row");

        let messages = db.list_messages(&chat.id).expect("replay transcript");

        assert_eq!(
            messages[0].payload.as_ref().expect("tool payload")["output"]["outcome"]["payload"],
            json!({ "accepted": true, "move": { "cell": 4 } })
        );
    }

    #[test]
    fn apply_agent_move_errors_when_not_agent_turn() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut db = AppDb::open(&temp.path().join("app.db")).expect("open db");
        let chat = db
            .create_chat("game", "mock-model", None)
            .expect("create chat");
        let board = BoardState {
            cells: vec![None; 9],
            turn: "X".to_string(),
        };
        db.upsert_chat_board(&chat.id, &board).expect("seed board");

        let err = db
            .apply_agent_move(&chat.id, 0)
            .expect_err("agent move should fail while it is X's turn");

        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.message, "It is not O's turn.");
        let board = db.chat_board(&chat.id).expect("load board");
        assert_eq!(board.turn, "X");
        assert!(board.cells.iter().all(Option::is_none));
    }

    #[test]
    fn branch_point_clones_only_the_pinned_product_projection() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut db = AppDb::open(&temp.path().join("app.db")).expect("open db");
        let source = db
            .create_chat("source", "mock-model", Some("low"))
            .expect("create source");
        db.insert_message(&source.id, "user", "before")
            .expect("insert message before pin");
        let pinned_board = BoardState {
            cells: vec![
                Some("X".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ],
            turn: "O".to_string(),
        };
        db.upsert_chat_board(&source.id, &pinned_board)
            .expect("save pinned board");
        let point = db
            .save_branch_point(&source.id, "node-pinned")
            .expect("save point");
        assert_eq!(point.message_count, 1);

        db.insert_message(&source.id, "assistant", "after")
            .expect("insert later message");
        db.upsert_chat_board(&source.id, &default_board())
            .expect("advance board");
        db.prepare_chat_fork(&source.id, "node-pinned", "branch")
            .expect("fork product projection");
        assert!(
            db.list_chats()
                .expect("list while branch is pending")
                .iter()
                .all(|chat| chat.id != "branch"),
            "pending product projection must not be externally visible"
        );
        let branch = db.finish_chat_fork("branch").expect("publish branch");

        assert_eq!(branch.id, "branch");
        assert_eq!(branch.model, "mock-model");
        assert_eq!(branch.model_variant.as_deref(), Some("low"));
        let messages = db.list_messages("branch").expect("branch messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "before");
        assert_eq!(db.chat_board("branch").expect("branch board"), pinned_board);

        let sibling = db
            .create_chat("sibling", "mock-model", None)
            .expect("create sibling");
        db.insert_message(&sibling.id, "user", "sibling projection")
            .expect("insert sibling message");
        db.save_branch_point(&sibling.id, "node-pinned")
            .expect("the same shared node can be pinned by another chat");
        assert_eq!(
            db.list_branch_points(&source.id)
                .expect("source point remains scoped")[0]
                .message_count,
            1,
            "a sibling chat must not overwrite the source projection"
        );
    }

    #[test]
    fn list_messages_rejects_malformed_payload() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut db = AppDb::open(&temp.path().join("app.db")).expect("open db");
        let chat = db
            .create_chat("malformed payload", "mock-model", None)
            .expect("create chat");
        db.conn
            .execute(
                "INSERT INTO messages (chat_id, role, text, payload, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![&chat.id, "assistant", "stored", "not-json", now()],
            )
            .expect("insert malformed payload");

        let error = db
            .list_messages(&chat.id)
            .expect_err("malformed stored payload must be rejected");
        assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            error.message.contains("Conversion error from type Text"),
            "unexpected decode error: {}",
            error.message
        );
    }

    #[test]
    fn open_rejects_legacy_model_label_without_rewriting_it() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("app.db");
        let db = AppDb::open(&path).expect("open db");
        db.conn
            .execute(
                "INSERT INTO chats
                 (id, title, created_at, updated_at, model, model_variant)
                 VALUES (?1, ?2, ?3, ?3, ?4, NULL)",
                params!["legacy", "Legacy", now(), "mock-model (low)"],
            )
            .expect("insert legacy model label");
        drop(db);

        let error = match AppDb::open(&path) {
            Ok(_) => panic!("legacy model label must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("legacy chat model label"));

        let conn = Connection::open(&path).expect("reopen raw db");
        let row = conn
            .query_row(
                "SELECT model, model_variant FROM chats WHERE id = 'legacy'",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .expect("read legacy row");
        assert_eq!(row, ("mock-model (low)".to_string(), None));
    }
}
