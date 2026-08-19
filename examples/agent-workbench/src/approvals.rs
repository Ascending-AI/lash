//! Host-owned approval policy for the Agent Workbench.
//!
//! Lash owns only the durable completion-key wait. This module owns the
//! product policy around that primitive: which tool requires approval, the
//! operator ledger, and the approve/deny decision.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash::tools::{
    LashlangToolBinding, PendingCompletion, ToolCall, ToolContract, ToolDefinition,
    ToolDefinitionLashlangExt, ToolManifest, ToolOutcome, ToolProvider,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

pub(crate) const APPROVAL_TOOL_NAME: &str = "workbench_ops_apply_change";

#[derive(Clone)]
pub(crate) struct WorkbenchApprovals {
    connection: Arc<Mutex<Connection>>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct PendingApproval {
    pub key: String,
    pub tool: String,
    pub arguments: Value,
    pub requesting_session: String,
    pub requested_at_ms: i64,
    pub age_ms: i64,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ApprovalError {
    #[error("approval ledger lock is poisoned")]
    Poisoned,
    #[error("approval `{0}` is not pending")]
    NotPending(String),
    #[error("approval ledger failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("approval completion key is invalid: {0}")]
    Key(#[from] serde_json::Error),
}

impl WorkbenchApprovals {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, ApprovalError> {
        let connection = Connection::open(path)?;
        Self::from_connection(connection)
    }

    pub(crate) fn in_memory() -> Result<Self, ApprovalError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self, ApprovalError> {
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 15000;
             CREATE TABLE IF NOT EXISTS approval_waits (
               key_id TEXT PRIMARY KEY,
               completion_key_json TEXT NOT NULL,
               tool_name TEXT NOT NULL,
               arguments_json TEXT NOT NULL,
               session_id TEXT NOT NULL,
               requested_at_ms INTEGER NOT NULL,
               decision TEXT,
               decided_at_ms INTEGER
             );",
        )?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub(crate) fn provider(&self) -> Arc<dyn ToolProvider> {
        Arc::new(ApprovalToolProvider {
            approvals: self.clone(),
        })
    }

    fn record(
        &self,
        key: &lash::AwaitEventKey,
        args: &Value,
        session_id: &str,
    ) -> Result<(), ApprovalError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApprovalError::Poisoned)?;
        connection.execute(
            "INSERT INTO approval_waits (
               key_id, completion_key_json, tool_name, arguments_json,
               session_id, requested_at_ms, decision, decided_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL)
             ON CONFLICT(key_id) DO NOTHING",
            params![
                key.key_id,
                serde_json::to_string(key)?,
                APPROVAL_TOOL_NAME,
                serde_json::to_string(args)?,
                session_id,
                chrono::Utc::now().timestamp_millis(),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn pending(&self) -> Result<Vec<PendingApproval>, ApprovalError> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApprovalError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT key_id, tool_name, arguments_json, session_id, requested_at_ms
             FROM approval_waits
             WHERE decision IS NULL
             ORDER BY requested_at_ms, key_id",
        )?;
        let rows = statement.query_map([], |row| {
            let requested_at_ms: i64 = row.get(4)?;
            let arguments_json: String = row.get(2)?;
            Ok(PendingApproval {
                key: row.get(0)?,
                tool: row.get(1)?,
                arguments: serde_json::from_str(&arguments_json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        arguments_json.len(),
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
                requesting_session: row.get(3)?,
                requested_at_ms,
                age_ms: now_ms.saturating_sub(requested_at_ms),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn completion_key(
        &self,
        key_id: &str,
    ) -> Result<lash::AwaitEventKey, ApprovalError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApprovalError::Poisoned)?;
        let serialized = connection
            .query_row(
                "SELECT completion_key_json FROM approval_waits
                 WHERE key_id = ?1 AND decision IS NULL",
                [key_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| ApprovalError::NotPending(key_id.to_string()))?;
        Ok(serde_json::from_str(&serialized)?)
    }

    pub(crate) fn mark_decided(
        &self,
        key_id: &str,
        decision: &'static str,
    ) -> Result<(), ApprovalError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApprovalError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE approval_waits
             SET decision = ?2, decided_at_ms = ?3
             WHERE key_id = ?1 AND decision IS NULL",
            params![key_id, decision, chrono::Utc::now().timestamp_millis()],
        )?;
        if changed == 0 {
            return Err(ApprovalError::NotPending(key_id.to_string()));
        }
        Ok(())
    }
}

struct ApprovalToolProvider {
    approvals: WorkbenchApprovals,
}

impl ApprovalToolProvider {
    fn definition() -> ToolDefinition {
        ToolDefinition::raw(
            "tool:workbench_ops_apply_change",
            APPROVAL_TOOL_NAME,
            "Stage an operational change and wait durably for a human operator to approve or deny it. Approval is required before the operation reports success.",
            json!({
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "The demo system to change." },
                    "change": { "type": "string", "description": "The change that requires sign-off." }
                },
                "required": ["target", "change"],
                "additionalProperties": false
            }),
            json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string", "enum": ["applied"] },
                    "target": { "type": "string" },
                    "change": { "type": "string" }
                },
                "required": ["status", "target", "change"],
                "additionalProperties": false
            }),
        )
        .with_lashlang_binding(
            LashlangToolBinding::new(["ops"], "apply_change").with_authority_type("Ops"),
        )
    }
}

#[async_trait]
impl ToolProvider for ApprovalToolProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == APPROVAL_TOOL_NAME).then(|| Arc::new(Self::definition().contract()))
    }

    /// The attempt parks on a human decision, so the runtime pre-derives the
    /// completion key the body reads from its `AttemptContext`.
    fn attempt_may_defer(&self, tool_id: &lash::tools::ToolId) -> bool {
        tool_id == Self::definition().id()
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        if call.name != APPROVAL_TOOL_NAME {
            return ToolOutcome::err_fmt(format_args!("unknown approval tool `{}`", call.name));
        }
        let key = match call.context.completion_key() {
            Ok(key) => key,
            Err(error) => return ToolOutcome::err_fmt(error),
        };
        if let Err(error) = self
            .approvals
            .record(&key, call.args, call.context.session_id())
        {
            return ToolOutcome::err_fmt(error);
        }
        ToolOutcome::pending(PendingCompletion::new())
    }
}

pub(crate) fn approval_resolution(approval: &PendingApproval) -> lash::Resolution {
    lash::Resolution::Ok(json!({
        "status": "applied",
        "target": approval.arguments.get("target").cloned().unwrap_or(Value::Null),
        "change": approval.arguments.get("change").cloned().unwrap_or(Value::Null),
    }))
}

pub(crate) fn denial_resolution() -> lash::Resolution {
    let mut error =
        lash::ExternalCompletionError::new("approval_denied", "the operator denied this change");
    error.raw = Some(json!({ "policy": "agent_workbench_human_approval" }));
    lash::Resolution::Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_ledger_survives_reopen() {
        let directory = tempfile::tempdir().expect("approval tempdir");
        let path = directory.path().join("approvals.db");
        let key = lash::AwaitEventKey {
            scope: lash::runtime::ExecutionScope::turn("approval-session", "turn-1"),
            wait: lash::AwaitEventWaitIdentity::tool_completion("tool-call-1"),
            key_id: "approval-key-1".to_string(),
            signature: "test-signature".to_string(),
        };
        WorkbenchApprovals::open(&path)
            .expect("open approval ledger")
            .record(
                &key,
                &json!({ "target": "demo", "change": "enable safe mode" }),
                "approval-session",
            )
            .expect("record approval");

        let reopened = WorkbenchApprovals::open(&path).expect("reopen approval ledger");
        let pending = reopened.pending().expect("list pending approvals");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].key, "approval-key-1");
        assert_eq!(pending[0].requesting_session, "approval-session");
        assert_eq!(reopened.completion_key("approval-key-1").unwrap(), key);
    }

    #[test]
    fn denial_preserves_host_policy_metadata_in_the_typed_resolution() {
        let resolution = denial_resolution();
        let lash::Resolution::Err(error) = &resolution else {
            panic!("denial must be an error resolution");
        };
        let expected = json!({ "policy": "agent_workbench_human_approval" });
        assert_eq!(error.code, "approval_denied");
        assert_eq!(error.message, "the operator denied this change");
        assert_eq!(error.raw, Some(expected));
        assert_eq!(resolution, lash::Resolution::Err(error.clone()));
    }

    #[test]
    fn approval_resolution_builds_success_payload() {
        let approval = PendingApproval {
            key: "test-key".to_string(),
            tool: APPROVAL_TOOL_NAME.to_string(),
            arguments: json!({ "target": "demo", "change": "restart" }),
            requesting_session: "session-1".to_string(),
            requested_at_ms: 0,
            age_ms: 0,
        };
        let resolution = approval_resolution(&approval);
        assert!(matches!(resolution, lash::Resolution::Ok(_)));
        let lash::Resolution::Ok(payload) = resolution else {
            panic!("expected ok resolution");
        };
        assert_eq!(payload.get("status"), Some(&json!("applied")));
    }
}
