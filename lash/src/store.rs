use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

/// SQLite-backed store for archive (pruned message content) and tasks.
/// Single `Mutex<Connection>` serializes all access (same as the old `Mutex<HashMap>`).
pub struct Store {
    conn: Mutex<Connection>,
}

// ─── Schema ───

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS archive (
    hash    TEXT PRIMARY KEY,
    content TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS tasks (
    id          TEXT PRIMARY KEY,
    subject     TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'pending',
    priority    TEXT NOT NULL DEFAULT 'medium',
    active_form TEXT NOT NULL DEFAULT '',
    owner       TEXT NOT NULL DEFAULT '',
    metadata    TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS task_deps (
    blocker_id  TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    blocked_id  TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    PRIMARY KEY (blocker_id, blocked_id)
);

CREATE TABLE IF NOT EXISTS counters (
    name  TEXT PRIMARY KEY,
    value INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO counters (name, value) VALUES ('task_id', 0);

CREATE TABLE IF NOT EXISTS agents (
    agent_id            TEXT PRIMARY KEY,
    parent_id           TEXT,
    status              TEXT NOT NULL DEFAULT 'active',
    messages            TEXT NOT NULL DEFAULT '[]',
    iteration           INTEGER NOT NULL DEFAULT 0,
    config_json         TEXT NOT NULL DEFAULT '{}',
    repl_snapshot       BLOB,
    input_tokens        INTEGER NOT NULL DEFAULT 0,
    output_tokens       INTEGER NOT NULL DEFAULT 0,
    cached_input_tokens INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS history_turns (
    agent_id         TEXT NOT NULL,
    turn_index       INTEGER NOT NULL,
    user_message     TEXT NOT NULL DEFAULT '',
    prose            TEXT NOT NULL DEFAULT '',
    code             TEXT NOT NULL DEFAULT '',
    output           TEXT NOT NULL DEFAULT '',
    error            TEXT,
    tool_calls_json  TEXT NOT NULL DEFAULT '[]',
    files_read_json  TEXT NOT NULL DEFAULT '[]',
    files_written_json TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (agent_id, turn_index)
);
CREATE INDEX IF NOT EXISTS idx_history_turns_agent_turn
ON history_turns(agent_id, turn_index);

CREATE TABLE IF NOT EXISTS mem_state (
    agent_id      TEXT PRIMARY KEY,
    current_turn  INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS mem_entries (
    agent_id      TEXT NOT NULL,
    key           TEXT NOT NULL,
    description   TEXT NOT NULL DEFAULT '',
    value         TEXT NOT NULL DEFAULT '',
    turn          INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (agent_id, key)
);
CREATE INDEX IF NOT EXISTS idx_mem_entries_agent_turn
ON mem_entries(agent_id, turn);
";

fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         PRAGMA cache_size = -2000;",
    )
}

// ─── TaskEntry (public, used by tasks.rs tool) ───

#[derive(Clone, Debug, serde::Serialize)]
pub struct TaskEntry {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: String,
    pub priority: String,
    pub active_form: String,
    pub owner: String,
    pub blocks: Vec<String>,
    pub blocked_by: Vec<String>,
    pub metadata: serde_json::Value,
}

/// Persisted agent state for snapshot/resume.
#[derive(Clone, Debug)]
pub struct AgentState {
    pub agent_id: String,
    pub parent_id: Option<String>,
    pub status: String,
    pub messages_json: String,
    pub iteration: i64,
    pub config_json: String,
    pub repl_snapshot: Option<Vec<u8>>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HistoryTurnRecord {
    pub index: i64,
    pub user_message: String,
    pub prose: String,
    pub code: String,
    pub output: String,
    pub error: Option<String>,
    pub tool_calls: Vec<serde_json::Value>,
    pub files_read: Vec<String>,
    pub files_written: Vec<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MemRecord {
    pub key: String,
    pub description: String,
    pub value: String,
    pub turn: i64,
}

type DependencyMap = HashMap<String, Vec<String>>;
type ActiveBlockedSet = HashSet<String>;
type DependencyMaps = (DependencyMap, DependencyMap, ActiveBlockedSet);

impl Store {
    /// Open (or create) a SQLite database at `path`.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        apply_pragmas(&conn)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory database (for sub-agents / tests).
    pub fn memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        apply_pragmas(&conn)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // ─── Archive operations ───

    /// Store content, return its 12-char hex SHA-256 hash.
    pub fn store_archive(&self, content: &str) -> String {
        let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
        let short = hash[..12].to_string();
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT OR IGNORE INTO archive (hash, content) VALUES (?1, ?2)",
            params![short, content],
        );
        short
    }

    /// Retrieve archived content by short hash.
    pub fn get_archive(&self, hash: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT content FROM archive WHERE hash = ?1",
            params![hash],
            |row| row.get(0),
        )
        .ok()
    }

    // ─── Task operations ───

    /// Atomically increment the task counter and return the next hex ID.
    pub fn next_task_id(&self) -> String {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE counters SET value = value + 1 WHERE name = 'task_id'",
            [],
        )
        .unwrap();
        let val: i64 = conn
            .query_row(
                "SELECT value FROM counters WHERE name = 'task_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        format!("{:04x}", val)
    }

    /// Create a new task. Returns the created TaskEntry.
    pub fn create_task(
        &self,
        id: &str,
        subject: &str,
        description: &str,
        priority: &str,
        active_form: &str,
        metadata: &serde_json::Value,
    ) -> TaskEntry {
        let meta_str = serde_json::to_string(metadata).unwrap_or_else(|_| "{}".into());
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tasks (id, subject, description, status, priority, active_form, metadata)
             VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?6)",
            params![id, subject, description, priority, active_form, meta_str],
        )
        .unwrap();
        drop(conn);
        self.get_task(id).unwrap()
    }

    /// Get a single task by ID, including its dependency edges.
    pub fn get_task(&self, id: &str) -> Option<TaskEntry> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, subject, description, status, priority, active_form, owner, metadata
                 FROM tasks WHERE id = ?1",
            )
            .ok()?;
        let entry = stmt
            .query_row(params![id], |row| {
                Ok(TaskEntryRow {
                    id: row.get(0)?,
                    subject: row.get(1)?,
                    description: row.get(2)?,
                    status: row.get(3)?,
                    priority: row.get(4)?,
                    active_form: row.get(5)?,
                    owner: row.get(6)?,
                    metadata_str: row.get(7)?,
                })
            })
            .ok()?;

        let blocks = self.get_blocks_locked(&conn, &entry.id);
        let blocked_by = self.get_blocked_by_locked(&conn, &entry.id);

        Some(TaskEntry {
            id: entry.id,
            subject: entry.subject,
            description: entry.description,
            status: entry.status,
            priority: entry.priority,
            active_form: entry.active_form,
            owner: entry.owner,
            blocks,
            blocked_by,
            metadata: serde_json::from_str(&entry.metadata_str).unwrap_or_default(),
        })
    }

    /// List tasks with optional status and blocked filters.
    pub fn list_tasks(
        &self,
        status_filter: Option<&str>,
        blocked_filter: Option<bool>,
    ) -> Vec<TaskEntry> {
        let conn = self.conn.lock().unwrap();
        let rows: Vec<TaskEntryRow> = if let Some(status) = status_filter {
            let mut stmt = match conn.prepare(
                "SELECT id, subject, description, status, priority, active_form, owner, metadata
                 FROM tasks WHERE status = ?1 ORDER BY id",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            stmt.query_map(params![status], |row| {
                Ok(TaskEntryRow {
                    id: row.get(0)?,
                    subject: row.get(1)?,
                    description: row.get(2)?,
                    status: row.get(3)?,
                    priority: row.get(4)?,
                    active_form: row.get(5)?,
                    owner: row.get(6)?,
                    metadata_str: row.get(7)?,
                })
            })
            .ok()
            .map(|iter| iter.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        } else {
            let mut stmt = match conn.prepare(
                "SELECT id, subject, description, status, priority, active_form, owner, metadata
                 FROM tasks ORDER BY id",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            stmt.query_map([], |row| {
                Ok(TaskEntryRow {
                    id: row.get(0)?,
                    subject: row.get(1)?,
                    description: row.get(2)?,
                    status: row.get(3)?,
                    priority: row.get(4)?,
                    active_form: row.get(5)?,
                    owner: row.get(6)?,
                    metadata_str: row.get(7)?,
                })
            })
            .ok()
            .map(|iter| iter.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        };

        let (blocks_map, blocked_by_map, actively_blocked) =
            self.load_dependency_maps_locked(&conn);

        let mut result = Vec::new();
        for entry in rows {
            let blocks = blocks_map.get(&entry.id).cloned().unwrap_or_default();
            let blocked_by = blocked_by_map.get(&entry.id).cloned().unwrap_or_default();
            let is_blocked = actively_blocked.contains(&entry.id);

            if let Some(want_blocked) = blocked_filter
                && want_blocked != is_blocked
            {
                continue;
            }

            result.push(TaskEntry {
                id: entry.id,
                subject: entry.subject,
                description: entry.description,
                status: entry.status,
                priority: entry.priority,
                active_form: entry.active_form,
                owner: entry.owner,
                blocks,
                blocked_by,
                metadata: serde_json::from_str(&entry.metadata_str).unwrap_or_default(),
            });
        }
        result
    }

    /// Update fields on a task. Only non-None fields are changed.
    #[allow(clippy::too_many_arguments)]
    pub fn update_task(
        &self,
        id: &str,
        subject: Option<&str>,
        description: Option<&str>,
        status: Option<&str>,
        priority: Option<&str>,
        active_form: Option<&str>,
        metadata_merge: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Option<TaskEntry> {
        let conn = self.conn.lock().unwrap();

        // Check exists
        let exists: bool = conn
            .query_row("SELECT 1 FROM tasks WHERE id = ?1", params![id], |_| Ok(()))
            .is_ok();
        if !exists {
            return None;
        }

        if let Some(v) = subject {
            conn.execute(
                "UPDATE tasks SET subject = ?1 WHERE id = ?2",
                params![v, id],
            )
            .unwrap();
        }
        if let Some(v) = description {
            conn.execute(
                "UPDATE tasks SET description = ?1 WHERE id = ?2",
                params![v, id],
            )
            .unwrap();
        }
        if let Some(v) = status {
            conn.execute("UPDATE tasks SET status = ?1 WHERE id = ?2", params![v, id])
                .unwrap();
            // Auto-clear owner when status changes to pending/completed/cancelled
            if matches!(v, "pending" | "completed" | "cancelled") {
                conn.execute("UPDATE tasks SET owner = '' WHERE id = ?1", params![id])
                    .unwrap();
            }
        }
        if let Some(v) = priority {
            conn.execute(
                "UPDATE tasks SET priority = ?1 WHERE id = ?2",
                params![v, id],
            )
            .unwrap();
        }
        if let Some(v) = active_form {
            conn.execute(
                "UPDATE tasks SET active_form = ?1 WHERE id = ?2",
                params![v, id],
            )
            .unwrap();
        }
        if let Some(merge) = metadata_merge {
            let current_str: String = conn
                .query_row(
                    "SELECT metadata FROM tasks WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .unwrap_or_else(|_| "{}".into());
            let mut current: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&current_str).unwrap_or_default();
            for (k, v) in merge {
                if v.is_null() {
                    current.remove(k);
                } else {
                    current.insert(k.clone(), v.clone());
                }
            }
            let new_str = serde_json::to_string(&current).unwrap_or_else(|_| "{}".into());
            conn.execute(
                "UPDATE tasks SET metadata = ?1 WHERE id = ?2",
                params![new_str, id],
            )
            .unwrap();
        }

        drop(conn);
        self.get_task(id)
    }

    /// Atomically claim a task: set owner and flip status to in_progress.
    /// If `id` is None, auto-picks the next claimable task (highest priority, unblocked, unclaimed pending).
    /// Returns Ok(TaskEntry) on success, Err(message) on failure.
    pub fn claim_task(&self, id: Option<&str>, owner: &str) -> Result<TaskEntry, String> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| format!("failed to start claim transaction: {e}"))?;

        let resolved_id = match id {
            Some(i) => i.to_string(),
            None => {
                // Auto-pick under an immediate transaction so selection+claim are atomic.
                let picked: Option<String> = tx
                    .query_row(
                        "SELECT t.id
                         FROM tasks t
                         WHERE t.status = 'pending'
                           AND t.owner = ''
                           AND NOT EXISTS (
                               SELECT 1
                               FROM task_deps d
                               JOIN tasks b ON b.id = d.blocker_id
                               WHERE d.blocked_id = t.id
                                 AND b.status NOT IN ('completed', 'cancelled')
                           )
                         ORDER BY
                           CASE t.priority
                               WHEN 'high' THEN 0
                               WHEN 'medium' THEN 1
                               WHEN 'low' THEN 2
                               ELSE 3
                           END,
                           t.id
                         LIMIT 1",
                        [],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|e| format!("failed to select claimable task: {e}"))?;
                match picked {
                    Some(v) => v,
                    None => return Err("no claimable tasks".into()),
                }
            }
        };

        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT status, owner FROM tasks WHERE id = ?1",
                params![&resolved_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|e| format!("failed to inspect task state: {e}"))?;
        let (status, current_owner) = match row {
            Some(v) => v,
            None => return Err(format!("task not found: {}", resolved_id)),
        };

        if status == "completed" || status == "cancelled" {
            return Err(format!("task is {}, cannot claim", status));
        }
        if !current_owner.is_empty() && current_owner != owner {
            return Err(format!("already claimed by {}", current_owner));
        }

        let blockers: Vec<String> = {
            let mut stmt = tx
                .prepare(
                    "SELECT d.blocker_id
                     FROM task_deps d
                     JOIN tasks t ON t.id = d.blocker_id
                     WHERE d.blocked_id = ?1
                       AND t.status NOT IN ('completed', 'cancelled')",
                )
                .map_err(|e| format!("failed to inspect blockers: {e}"))?;
            stmt.query_map(params![&resolved_id], |row| row.get(0))
                .map_err(|e| format!("failed to inspect blockers: {e}"))?
                .filter_map(|r| r.ok())
                .collect()
        };
        if !blockers.is_empty() {
            return Err(format!("blocked by: {}", blockers.join(", ")));
        }

        let changed = tx
            .execute(
                "UPDATE tasks
                 SET owner = ?1, status = 'in_progress'
                 WHERE id = ?2
                   AND status NOT IN ('completed', 'cancelled')
                   AND (owner = '' OR owner = ?1)",
                params![owner, &resolved_id],
            )
            .map_err(|e| format!("failed to claim task: {e}"))?;
        if changed == 0 {
            return Err(format!("failed to claim task: {}", resolved_id));
        }

        tx.commit()
            .map_err(|e| format!("failed to commit claim: {e}"))?;
        drop(conn);
        self.get_task(&resolved_id)
            .ok_or_else(|| format!("task not found: {}", resolved_id))
    }

    /// Delete a task and clean up its dependency edges.
    pub fn delete_task(&self, id: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        // CASCADE on task_deps handles edge cleanup
        let changed = conn
            .execute("DELETE FROM tasks WHERE id = ?1", params![id])
            .unwrap_or(0);
        changed > 0
    }

    /// Add a dependency edge: `blocker_id` blocks `blocked_id`.
    pub fn add_dep(&self, blocker_id: &str, blocked_id: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT OR IGNORE INTO task_deps (blocker_id, blocked_id) VALUES (?1, ?2)",
            params![blocker_id, blocked_id],
        );
    }

    /// Remove a dependency edge.
    pub fn remove_dep(&self, blocker_id: &str, blocked_id: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "DELETE FROM task_deps WHERE blocker_id = ?1 AND blocked_id = ?2",
            params![blocker_id, blocked_id],
        );
    }

    /// Get a task summary (counts, high-priority, blocked).
    pub fn task_summary(&self) -> String {
        let conn = self.conn.lock().unwrap();

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
            .unwrap_or(0);
        if total == 0 {
            return "No tasks.".into();
        }

        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE status = 'pending'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let in_progress: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE status = 'in_progress'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let completed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE status = 'completed'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let cancelled: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE status = 'cancelled'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let mut lines = vec![format!(
            "Tasks: {} total  |  {} pending  {} in_progress  {} completed  {} cancelled",
            total, pending, in_progress, completed, cancelled
        )];

        // High priority (non-completed, non-cancelled)
        let mut stmt = conn
            .prepare("SELECT id, subject, status FROM tasks WHERE priority = 'high' AND status NOT IN ('completed', 'cancelled') ORDER BY id")
            .unwrap();
        let high: Vec<(String, String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        if !high.is_empty() {
            lines.push(String::new());
            lines.push("High priority:".into());
            for (id, subject, status) in &high {
                let is_blocked = self.is_blocked_locked(&conn, id);
                let symbol = match status.as_str() {
                    "in_progress" => "~",
                    _ if is_blocked => "!",
                    _ => "+",
                };
                lines.push(format!("  {} {} '{}'", symbol, id, subject));
            }
        }

        // Blocked tasks
        let mut stmt = conn
            .prepare("SELECT id, subject FROM tasks WHERE status NOT IN ('completed', 'cancelled') ORDER BY id")
            .unwrap();
        let active: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        let mut blocked_lines = Vec::new();
        for (id, subject) in &active {
            let blocked_by = self.get_blocked_by_locked(&conn, id);
            // Filter to only non-completed/non-cancelled blockers
            let active_blockers: Vec<&String> = blocked_by
                .iter()
                .filter(|bid| {
                    conn.query_row(
                        "SELECT status FROM tasks WHERE id = ?1",
                        params![bid],
                        |row| row.get::<_, String>(0),
                    )
                    .map(|s| s != "completed" && s != "cancelled")
                    .unwrap_or(false)
                })
                .collect();
            if !active_blockers.is_empty() {
                let blocker_ids: Vec<&str> = active_blockers.iter().map(|s| s.as_str()).collect();
                blocked_lines.push(format!(
                    "  {} '{}'  blocked by: {}",
                    id,
                    subject,
                    blocker_ids.join(", ")
                ));
            }
        }
        if !blocked_lines.is_empty() {
            lines.push(String::new());
            lines.push("Blocked:".into());
            lines.extend(blocked_lines);
        }

        lines.join("\n")
    }

    // ─── Agent state operations (snapshot/resume) ───

    /// Save or update an agent's state in the store.
    #[allow(clippy::too_many_arguments)]
    pub fn save_agent_state(
        &self,
        agent_id: &str,
        parent_id: Option<&str>,
        status: &str,
        messages_json: &str,
        iteration: i64,
        config_json: &str,
        repl_snapshot: Option<&[u8]>,
        input_tokens: i64,
        output_tokens: i64,
        cached_input_tokens: i64,
    ) {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO agents (agent_id, parent_id, status, messages, iteration, config_json, repl_snapshot, input_tokens, output_tokens, cached_input_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![agent_id, parent_id, status, messages_json, iteration, config_json, repl_snapshot, input_tokens, output_tokens, cached_input_tokens],
        )
        .unwrap();
    }

    /// Load an agent's state by ID.
    pub fn load_agent_state(&self, agent_id: &str) -> Option<AgentState> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT agent_id, parent_id, status, messages, iteration, config_json, repl_snapshot, input_tokens, output_tokens, cached_input_tokens
             FROM agents WHERE agent_id = ?1",
            params![agent_id],
            |row| {
                Ok(AgentState {
                    agent_id: row.get(0)?,
                    parent_id: row.get(1)?,
                    status: row.get(2)?,
                    messages_json: row.get(3)?,
                    iteration: row.get(4)?,
                    config_json: row.get(5)?,
                    repl_snapshot: row.get(6)?,
                    input_tokens: row.get(7)?,
                    output_tokens: row.get(8)?,
                    cached_input_tokens: row.get(9)?,
                })
            },
        )
        .ok()
    }

    /// List all active agents, optionally filtered by parent_id.
    pub fn list_active_agents(&self, parent_id: Option<&str>) -> Vec<AgentState> {
        let conn = self.conn.lock().unwrap();
        let mut results = Vec::new();
        let query = match parent_id {
            Some(_) => {
                "SELECT agent_id, parent_id, status, messages, iteration, config_json, repl_snapshot, input_tokens, output_tokens, cached_input_tokens
                 FROM agents WHERE status = 'active' AND parent_id = ?1 ORDER BY agent_id"
            }
            None => {
                "SELECT agent_id, parent_id, status, messages, iteration, config_json, repl_snapshot, input_tokens, output_tokens, cached_input_tokens
                 FROM agents WHERE status = 'active' AND parent_id IS NULL ORDER BY agent_id"
            }
        };
        let mut stmt = match conn.prepare(query) {
            Ok(s) => s,
            Err(_) => return results,
        };
        let row_mapper = |row: &rusqlite::Row| -> rusqlite::Result<AgentState> {
            Ok(AgentState {
                agent_id: row.get(0)?,
                parent_id: row.get(1)?,
                status: row.get(2)?,
                messages_json: row.get(3)?,
                iteration: row.get(4)?,
                config_json: row.get(5)?,
                repl_snapshot: row.get(6)?,
                input_tokens: row.get(7)?,
                output_tokens: row.get(8)?,
                cached_input_tokens: row.get(9)?,
            })
        };
        let iter = if let Some(pid) = parent_id {
            stmt.query_map(params![pid], row_mapper)
        } else {
            stmt.query_map([], row_mapper)
        };
        if let Ok(rows) = iter {
            for row in rows.flatten() {
                results.push(row);
            }
        }
        results
    }

    /// Mark an agent as done.
    pub fn mark_agent_done(&self, agent_id: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE agents SET status = 'done' WHERE agent_id = ?1",
            params![agent_id],
        );
    }

    // ─── History operations ───

    pub fn history_add_turn(&self, agent_id: &str, turn: &serde_json::Value) {
        let conn = self.conn.lock().unwrap();
        Self::history_add_turn_locked(&conn, agent_id, turn);
    }

    pub fn history_export(&self, agent_id: &str) -> Vec<HistoryTurnRecord> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT turn_index, user_message, prose, code, output, error, tool_calls_json, files_read_json, files_written_json
             FROM history_turns
             WHERE agent_id = ?1
             ORDER BY turn_index",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map(params![agent_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
            ))
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        rows.filter_map(|r| r.ok())
            .map(
                |(
                    index,
                    user_message,
                    prose,
                    code,
                    output,
                    error,
                    tool_calls_json,
                    files_read_json,
                    files_written_json,
                )| {
                    let tool_calls = serde_json::from_str(&tool_calls_json).unwrap_or_default();
                    let files_read = serde_json::from_str(&files_read_json).unwrap_or_default();
                    let files_written =
                        serde_json::from_str(&files_written_json).unwrap_or_default();
                    HistoryTurnRecord {
                        index,
                        user_message,
                        prose,
                        code,
                        output,
                        error,
                        tool_calls,
                        files_read,
                        files_written,
                    }
                },
            )
            .collect()
    }

    pub fn history_load(&self, agent_id: &str, turns: &[serde_json::Value]) {
        let mut conn = self.conn.lock().unwrap();
        let tx = match conn.transaction() {
            Ok(tx) => tx,
            Err(_) => return,
        };
        let _ = tx.execute(
            "DELETE FROM history_turns WHERE agent_id = ?1",
            params![agent_id],
        );
        for turn in turns {
            Self::history_add_turn_locked(&tx, agent_id, turn);
        }
        let _ = tx.commit();
    }

    // ─── Memory operations ───

    pub fn mem_set_turn(&self, agent_id: &str, turn: i64) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO mem_state (agent_id, current_turn) VALUES (?1, ?2)
             ON CONFLICT(agent_id) DO UPDATE SET current_turn = excluded.current_turn",
            params![agent_id, turn],
        );
    }

    pub fn mem_set(&self, agent_id: &str, key: &str, description: &str, value: &str) {
        let conn = self.conn.lock().unwrap();
        let current_turn: i64 = conn
            .query_row(
                "SELECT current_turn FROM mem_state WHERE agent_id = ?1",
                params![agent_id],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let _ = conn.execute(
            "INSERT OR REPLACE INTO mem_entries (agent_id, key, description, value, turn)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![agent_id, key, description, value, current_turn],
        );
    }

    pub fn mem_get(&self, agent_id: &str, key: &str) -> Option<MemRecord> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT key, description, value, turn
             FROM mem_entries
             WHERE agent_id = ?1 AND key = ?2",
            params![agent_id, key],
            |row| {
                Ok(MemRecord {
                    key: row.get(0)?,
                    description: row.get(1)?,
                    value: row.get(2)?,
                    turn: row.get(3)?,
                })
            },
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn mem_delete(&self, agent_id: &str, key: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM mem_entries WHERE agent_id = ?1 AND key = ?2",
            params![agent_id, key],
        )
        .unwrap_or(0)
            > 0
    }

    pub fn mem_export(&self, agent_id: &str) -> Vec<MemRecord> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT key, description, value, turn
             FROM mem_entries
             WHERE agent_id = ?1
             ORDER BY key",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map(params![agent_id], |row| {
            Ok(MemRecord {
                key: row.get(0)?,
                description: row.get(1)?,
                value: row.get(2)?,
                turn: row.get(3)?,
            })
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.filter_map(|r| r.ok()).collect()
    }

    pub fn mem_load(&self, agent_id: &str, entries: &[serde_json::Value]) {
        let mut conn = self.conn.lock().unwrap();
        let tx = match conn.transaction() {
            Ok(tx) => tx,
            Err(_) => return,
        };
        let _ = tx.execute(
            "DELETE FROM mem_entries WHERE agent_id = ?1",
            params![agent_id],
        );
        let mut max_turn = 0_i64;
        for entry in entries {
            let key = entry
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if key.is_empty() {
                continue;
            }
            let description = entry
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let value = entry
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let turn = entry.get("turn").and_then(|v| v.as_i64()).unwrap_or(0);
            max_turn = max_turn.max(turn);
            let _ = tx.execute(
                "INSERT OR REPLACE INTO mem_entries (agent_id, key, description, value, turn)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![agent_id, key, description, value, turn],
            );
        }
        let _ = tx.execute(
            "INSERT INTO mem_state (agent_id, current_turn) VALUES (?1, ?2)
             ON CONFLICT(agent_id) DO UPDATE SET current_turn = excluded.current_turn",
            params![agent_id, max_turn],
        );
        let _ = tx.commit();
    }

    // ─── Internal helpers (require caller to already hold the lock) ───

    fn history_add_turn_locked(conn: &Connection, agent_id: &str, turn: &serde_json::Value) {
        let index = turn.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
        let user_message = turn
            .get("user_message")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let prose = turn
            .get("prose")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let code = turn
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let output = turn
            .get("output")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let error = turn
            .get("error")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let tool_calls = turn
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut files_read = std::collections::BTreeSet::new();
        let mut files_written = std::collections::BTreeSet::new();
        for tc in &tool_calls {
            let tool = tc.get("tool").and_then(|v| v.as_str()).unwrap_or_default();
            let path = tc
                .get("args")
                .and_then(|v| v.as_object())
                .and_then(|obj| obj.get("path"))
                .and_then(|v| v.as_str());
            if let Some(path) = path {
                match tool {
                    "read_file" | "glob" | "grep" => {
                        files_read.insert(path.to_string());
                    }
                    "write_file" | "edit_file" | "find_replace" => {
                        files_written.insert(path.to_string());
                    }
                    _ => {}
                }
            }
        }

        let tool_calls_json = serde_json::to_string(&tool_calls).unwrap_or_else(|_| "[]".into());
        let files_read_json = serde_json::to_string(&files_read.into_iter().collect::<Vec<_>>())
            .unwrap_or_else(|_| "[]".into());
        let files_written_json =
            serde_json::to_string(&files_written.into_iter().collect::<Vec<_>>())
                .unwrap_or_else(|_| "[]".into());

        let _ = conn.execute(
            "INSERT OR REPLACE INTO history_turns
             (agent_id, turn_index, user_message, prose, code, output, error, tool_calls_json, files_read_json, files_written_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                agent_id,
                index,
                user_message,
                prose,
                code,
                output,
                error,
                tool_calls_json,
                files_read_json,
                files_written_json
            ],
        );
    }

    /// Build dependency maps in one pass:
    /// - blocks_map: blocker_id -> [blocked_id]
    /// - blocked_by_map: blocked_id -> [blocker_id]
    /// - actively_blocked: blocked task ids that currently have unfinished blockers
    fn load_dependency_maps_locked(&self, conn: &Connection) -> DependencyMaps {
        let mut blocks_map: DependencyMap = HashMap::new();
        let mut blocked_by_map: DependencyMap = HashMap::new();
        let mut actively_blocked: ActiveBlockedSet = HashSet::new();

        let mut stmt = match conn.prepare(
            "SELECT d.blocker_id, d.blocked_id, t.status
             FROM task_deps d
             JOIN tasks t ON t.id = d.blocker_id
             ORDER BY d.blocker_id, d.blocked_id",
        ) {
            Ok(s) => s,
            Err(_) => return (blocks_map, blocked_by_map, actively_blocked),
        };

        let rows = match stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        }) {
            Ok(r) => r,
            Err(_) => return (blocks_map, blocked_by_map, actively_blocked),
        };

        for (blocker, blocked, blocker_status) in rows.flatten() {
            blocks_map
                .entry(blocker.clone())
                .or_default()
                .push(blocked.clone());
            blocked_by_map
                .entry(blocked.clone())
                .or_default()
                .push(blocker);

            if blocker_status != "completed" && blocker_status != "cancelled" {
                actively_blocked.insert(blocked);
            }
        }

        for values in blocks_map.values_mut() {
            values.sort();
        }
        for values in blocked_by_map.values_mut() {
            values.sort();
        }

        (blocks_map, blocked_by_map, actively_blocked)
    }

    /// Tasks that `id` blocks (id is the blocker).
    fn get_blocks_locked(&self, conn: &Connection, id: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT blocked_id FROM task_deps WHERE blocker_id = ?1 ORDER BY blocked_id")
            .unwrap();
        stmt.query_map(params![id], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    /// Tasks that block `id` (id is the blocked).
    fn get_blocked_by_locked(&self, conn: &Connection, id: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT blocker_id FROM task_deps WHERE blocked_id = ?1 ORDER BY blocker_id")
            .unwrap();
        stmt.query_map(params![id], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    /// Is this task blocked by any non-completed/non-cancelled task?
    fn is_blocked_locked(&self, conn: &Connection, id: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM task_deps d
             JOIN tasks t ON t.id = d.blocker_id
             WHERE d.blocked_id = ?1
               AND t.status NOT IN ('completed', 'cancelled')
             LIMIT 1",
            params![id],
            |_| Ok(()),
        )
        .is_ok()
    }
}

/// Internal row type for reading from SQLite before adding dep info.
struct TaskEntryRow {
    id: String,
    subject: String,
    description: String,
    status: String,
    priority: String,
    active_form: String,
    owner: String,
    metadata_str: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Store {
        Store::memory().unwrap()
    }

    // ── Archive ──

    #[test]
    fn archive_round_trip() {
        let s = mem();
        let hash = s.store_archive("hello world");
        assert_eq!(hash.len(), 12);
        assert_eq!(s.get_archive(&hash).unwrap(), "hello world");
    }

    #[test]
    fn archive_dedup() {
        let s = mem();
        let h1 = s.store_archive("same content");
        let h2 = s.store_archive("same content");
        assert_eq!(h1, h2);
    }

    #[test]
    fn archive_missing_hash() {
        let s = mem();
        assert!(s.get_archive("000000000000").is_none());
    }

    // ── Task ID ──

    #[test]
    fn next_task_id_increments() {
        let s = mem();
        assert_eq!(s.next_task_id(), "0001");
        assert_eq!(s.next_task_id(), "0002");
        assert_eq!(s.next_task_id(), "0003");
    }

    // ── claim_task ──

    #[test]
    fn claim_pending_unblocked() {
        let s = mem();
        let id = s.next_task_id();
        s.create_task(&id, "t1", "", "medium", "", &serde_json::json!({}));
        let t = s.claim_task(Some(&id), "agent-a").unwrap();
        assert_eq!(t.owner, "agent-a");
        assert_eq!(t.status, "in_progress");
    }

    #[test]
    fn claim_nonexistent() {
        let s = mem();
        let err = s.claim_task(Some("9999"), "a").unwrap_err();
        assert!(err.contains("task not found"));
    }

    #[test]
    fn claim_completed() {
        let s = mem();
        let id = s.next_task_id();
        s.create_task(&id, "t", "", "medium", "", &serde_json::json!({}));
        s.update_task(&id, None, None, Some("completed"), None, None, None);
        let err = s.claim_task(Some(&id), "a").unwrap_err();
        assert!(err.contains("completed"));
    }

    #[test]
    fn claim_cancelled() {
        let s = mem();
        let id = s.next_task_id();
        s.create_task(&id, "t", "", "medium", "", &serde_json::json!({}));
        s.update_task(&id, None, None, Some("cancelled"), None, None, None);
        let err = s.claim_task(Some(&id), "a").unwrap_err();
        assert!(err.contains("cancelled"));
    }

    #[test]
    fn claim_different_owner() {
        let s = mem();
        let id = s.next_task_id();
        s.create_task(&id, "t", "", "medium", "", &serde_json::json!({}));
        s.claim_task(Some(&id), "agent-a").unwrap();
        let err = s.claim_task(Some(&id), "agent-b").unwrap_err();
        assert!(err.contains("already claimed by agent-a"));
    }

    #[test]
    fn claim_same_owner_idempotent() {
        let s = mem();
        let id = s.next_task_id();
        s.create_task(&id, "t", "", "medium", "", &serde_json::json!({}));
        s.claim_task(Some(&id), "agent-a").unwrap();
        let t = s.claim_task(Some(&id), "agent-a").unwrap();
        assert_eq!(t.owner, "agent-a");
    }

    #[test]
    fn claim_blocked_task() {
        let s = mem();
        let id1 = s.next_task_id();
        let id2 = s.next_task_id();
        s.create_task(&id1, "blocker", "", "medium", "", &serde_json::json!({}));
        s.create_task(&id2, "blocked", "", "medium", "", &serde_json::json!({}));
        s.add_dep(&id1, &id2);
        let err = s.claim_task(Some(&id2), "a").unwrap_err();
        assert!(err.contains("blocked by"));
        assert!(err.contains(&id1));
    }

    #[test]
    fn auto_pick_claims_highest_priority() {
        let s = mem();
        let low = s.next_task_id();
        let high = s.next_task_id();
        s.create_task(&low, "low-task", "", "low", "", &serde_json::json!({}));
        s.create_task(&high, "high-task", "", "high", "", &serde_json::json!({}));
        let t = s.claim_task(None, "a").unwrap();
        assert_eq!(t.id, high);
    }

    #[test]
    fn auto_pick_no_claimable() {
        let s = mem();
        let err = s.claim_task(None, "a").unwrap_err();
        assert!(err.contains("no claimable tasks"));
    }

    // ── Dependencies ──

    #[test]
    fn add_remove_dep() {
        let s = mem();
        let a = s.next_task_id();
        let b = s.next_task_id();
        s.create_task(&a, "A", "", "medium", "", &serde_json::json!({}));
        s.create_task(&b, "B", "", "medium", "", &serde_json::json!({}));
        s.add_dep(&a, &b);
        let tb = s.get_task(&b).unwrap();
        assert!(tb.blocked_by.contains(&a));
        let ta = s.get_task(&a).unwrap();
        assert!(ta.blocks.contains(&b));
        s.remove_dep(&a, &b);
        let tb2 = s.get_task(&b).unwrap();
        assert!(tb2.blocked_by.is_empty());
    }

    // ── Owner auto-clear ──

    #[test]
    fn owner_cleared_on_pending() {
        let s = mem();
        let id = s.next_task_id();
        s.create_task(&id, "t", "", "medium", "", &serde_json::json!({}));
        s.claim_task(Some(&id), "agent-a").unwrap();
        s.update_task(&id, None, None, Some("pending"), None, None, None);
        let t = s.get_task(&id).unwrap();
        assert!(t.owner.is_empty());
    }

    #[test]
    fn owner_cleared_on_completed() {
        let s = mem();
        let id = s.next_task_id();
        s.create_task(&id, "t", "", "medium", "", &serde_json::json!({}));
        s.claim_task(Some(&id), "agent-a").unwrap();
        s.update_task(&id, None, None, Some("completed"), None, None, None);
        let t = s.get_task(&id).unwrap();
        assert!(t.owner.is_empty());
    }

    // ── Metadata merge ──

    #[test]
    fn metadata_merge_add_and_delete() {
        let s = mem();
        let id = s.next_task_id();
        s.create_task(&id, "t", "", "medium", "", &serde_json::json!({"a": 1}));
        let mut merge = serde_json::Map::new();
        merge.insert("b".into(), serde_json::json!(2));
        merge.insert("a".into(), serde_json::Value::Null);
        s.update_task(&id, None, None, None, None, None, Some(&merge));
        let t = s.get_task(&id).unwrap();
        assert_eq!(t.metadata.get("b").unwrap(), 2);
        assert!(t.metadata.get("a").is_none());
    }

    // ── Blocked filter ──

    #[test]
    fn list_tasks_blocked_filter() {
        let s = mem();
        let a = s.next_task_id();
        let b = s.next_task_id();
        s.create_task(&a, "A", "", "medium", "", &serde_json::json!({}));
        s.create_task(&b, "B", "", "medium", "", &serde_json::json!({}));
        s.add_dep(&a, &b);
        let blocked = s.list_tasks(None, Some(true));
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].id, b);
        let unblocked = s.list_tasks(None, Some(false));
        assert_eq!(unblocked.len(), 1);
        assert_eq!(unblocked[0].id, a);
    }

    // ── Agent state ──

    #[test]
    fn agent_state_round_trip() {
        let s = mem();
        s.save_agent_state(
            "ag1",
            Some("parent"),
            "active",
            "[]",
            5,
            "{}",
            None,
            100,
            50,
            10,
        );
        let a = s.load_agent_state("ag1").unwrap();
        assert_eq!(a.agent_id, "ag1");
        assert_eq!(a.parent_id.as_deref(), Some("parent"));
        assert_eq!(a.status, "active");
        assert_eq!(a.iteration, 5);
        assert_eq!(a.input_tokens, 100);
    }

    #[test]
    fn mark_agent_done() {
        let s = mem();
        s.save_agent_state("ag1", None, "active", "[]", 0, "{}", None, 0, 0, 0);
        s.mark_agent_done("ag1");
        let a = s.load_agent_state("ag1").unwrap();
        assert_eq!(a.status, "done");
    }

    #[test]
    fn list_active_agents_filters() {
        let s = mem();
        s.save_agent_state("ag1", Some("p1"), "active", "[]", 0, "{}", None, 0, 0, 0);
        s.save_agent_state("ag2", Some("p1"), "active", "[]", 0, "{}", None, 0, 0, 0);
        s.save_agent_state("ag3", Some("p2"), "active", "[]", 0, "{}", None, 0, 0, 0);
        s.mark_agent_done("ag2");
        let active = s.list_active_agents(Some("p1"));
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].agent_id, "ag1");
    }
}
