//! Host-owned approval policy for the Agent Workbench.
//!
//! Lash owns only the durable completion-key wait. This module owns the
//! product policy around that primitive: which tool requires approval, the
//! operator ledger, and the approve/deny decision.

use lash::SessionId;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash::tools::{
    PendingCompletion, ToolBinding, ToolCall, ToolContract, ToolDefinition,
    ToolDefinitionBindingExt, ToolManifest, ToolOutcome, ToolProvider,
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

/// The operator's decision on a pending approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApprovalDecision {
    Approved,
    Denied,
}

impl ApprovalDecision {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Denied => "denied",
        }
    }

    fn from_stored(stored: &str) -> Option<Self> {
        match stored {
            "approved" => Some(Self::Approved),
            "denied" => Some(Self::Denied),
            _ => None,
        }
    }
}

/// A ledger row whose operator decision was recorded. The wait it resolves
/// can still be outstanding — the ledger write and the completion resolve are
/// two writes — so decided rows feed the idempotent repair in
/// `decide_approval` and the boot reconcile.
#[derive(Clone, Debug)]
pub(crate) struct DecidedApproval {
    pub key: String,
    pub completion_key: lash::AwaitEventKey,
    pub decision: ApprovalDecision,
    pub tool: String,
    pub arguments: Value,
    pub requesting_session: String,
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
               decision TEXT CHECK (decision IS NULL OR decision IN ('approved', 'denied')),
               decided_at_ms INTEGER,
               -- The pair is one atomic fact: a decision exists only together
               -- with the timestamp it was made at. The CHECK exists on
               -- databases created under this schema; `mark_decided` writes
               -- both columns in one statement either way.
               CHECK ((decision IS NULL) = (decided_at_ms IS NULL))
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
        session_id: &SessionId,
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
                session_id.as_str(),
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
        decision: ApprovalDecision,
    ) -> Result<(), ApprovalError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApprovalError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE approval_waits
             SET decision = ?2, decided_at_ms = ?3
             WHERE key_id = ?1 AND decision IS NULL",
            params![
                key_id,
                decision.as_str(),
                chrono::Utc::now().timestamp_millis()
            ],
        )?;
        if changed == 0 {
            return Err(ApprovalError::NotPending(key_id.to_string()));
        }
        Ok(())
    }

    /// Rows whose decision was written. A crash between `mark_decided` and the
    /// completion resolve leaves a decided row over an outstanding wait;
    /// callers re-resolve these idempotently.
    pub(crate) fn decided(&self) -> Result<Vec<DecidedApproval>, ApprovalError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApprovalError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT key_id, completion_key_json, decision, tool_name,
                    arguments_json, session_id
             FROM approval_waits
             WHERE decision IS NOT NULL
             ORDER BY requested_at_ms, key_id",
        )?;
        let rows = statement.query_map([], |row| {
            let key_id: String = row.get(0)?;
            let completion_key_json: String = row.get(1)?;
            let decision_stored: String = row.get(2)?;
            let arguments_json: String = row.get(4)?;
            let corrupt = |error: serde_json::Error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            };
            let decision = ApprovalDecision::from_stored(&decision_stored).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    format!("unknown approval decision `{decision_stored}`").into(),
                )
            })?;
            Ok(DecidedApproval {
                key: key_id,
                completion_key: serde_json::from_str(&completion_key_json).map_err(corrupt)?,
                decision,
                tool: row.get(3)?,
                arguments: serde_json::from_str(&arguments_json).map_err(corrupt)?,
                requesting_session: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
        .with_tool_binding(
            ToolBinding::new(["ops"], "apply_change").with_authority_type("Ops"),
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
        if let Err(error) =
            self.approvals
                .record(&key, call.args, &SessionId::from(call.context.session_id()))
        {
            return ToolOutcome::err_fmt(error);
        }
        ToolOutcome::pending(PendingCompletion::new())
    }
}

/// The wait resolution a recorded decision derives: identical for the live
/// route and for repair passes over decided rows.
pub(crate) fn resolution_for(decision: ApprovalDecision, arguments: &Value) -> lash::Resolution {
    match decision {
        ApprovalDecision::Approved => lash::Resolution::Ok(json!({
            "status": "applied",
            "target": arguments.get("target").cloned().unwrap_or(Value::Null),
            "change": arguments.get("change").cloned().unwrap_or(Value::Null),
        })),
        ApprovalDecision::Denied => denial_resolution(),
    }
}

#[cfg(test)]
pub(crate) fn approval_resolution(approval: &PendingApproval) -> lash::Resolution {
    resolution_for(ApprovalDecision::Approved, &approval.arguments)
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
                &SessionId::from("approval-session"),
            )
            .expect("record approval");

        let reopened = WorkbenchApprovals::open(&path).expect("reopen approval ledger");
        let pending = reopened.pending().expect("list pending approvals");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].key, "approval-key-1");
        assert_eq!(pending[0].requesting_session, "approval-session");
        assert_eq!(reopened.completion_key("approval-key-1").unwrap(), key);
    }

    /// FIG-3293: `decision`/`decided_at_ms` are one atomic fact — neither half
    /// may be written without the other, and the decision vocabulary is closed.
    #[test]
    fn decision_pair_is_constrained() {
        let approvals = WorkbenchApprovals::in_memory().expect("in-memory ledger");
        let connection = approvals.connection.lock().expect("ledger lock");
        connection
            .execute(
                "INSERT INTO approval_waits (
                   key_id, completion_key_json, tool_name, arguments_json,
                   session_id, requested_at_ms, decision, decided_at_ms
                 ) VALUES ('k1', '{}', 'tool', '{}', 's', 0, 'approved', NULL)",
                [],
            )
            .expect_err("a decision without its timestamp must be rejected");
        connection
            .execute(
                "INSERT INTO approval_waits (
                   key_id, completion_key_json, tool_name, arguments_json,
                   session_id, requested_at_ms, decision, decided_at_ms
                 ) VALUES ('k2', '{}', 'tool', '{}', 's', 0, NULL, 1)",
                [],
            )
            .expect_err("a timestamp without its decision must be rejected");
        connection
            .execute(
                "INSERT INTO approval_waits (
                   key_id, completion_key_json, tool_name, arguments_json,
                   session_id, requested_at_ms, decision, decided_at_ms
                 ) VALUES ('k3', '{}', 'tool', '{}', 's', 0, 'shrugged', 1)",
                [],
            )
            .expect_err("a decision outside the vocabulary must be rejected");
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
