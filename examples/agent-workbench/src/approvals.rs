//! Host-owned approval policy for the Agent Workbench.
//!
//! Lash owns the parked tool call and its completion key: the deferring
//! tool's body records its request under its call id and parks, under a short
//! execution bound for the body and a park that lasts until the turn that
//! asked ends, so a human decides in their own time. This module owns the
//! product policy around it: which tool requires approval, the operator
//! ledger, and the approve/deny decision.
//!
//! `Completions::parked` is the one source of what is pending and of the key
//! that resolves it. The ledger holds no key: it holds each request's context
//! (arguments, requesting session) and, between the operator's decision and
//! the resolve's answer, the decision. A row is deleted once `resolve`
//! answers, or once its call is no longer parked.

use lash::SessionId;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash::tools::{
    PendingCompletion, ToolAttemptOutcome, ToolBinding, ToolCall, ToolContract, ToolDefinition,
    ToolDefinitionBindingExt, ToolManifest, ToolOutcome, ToolProvider,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

pub(crate) const APPROVAL_TOOL_NAME: &str = "workbench_ops_apply_change";
pub(crate) const APPROVAL_TOOL_ID: &str = "tool:workbench_ops_apply_change";

#[derive(Clone)]
pub(crate) struct WorkbenchApprovals {
    connection: Arc<Mutex<Connection>>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct PendingApproval {
    pub key: String,
    pub tool: String,
    /// The tool call that waits on this approval, so the page anchors the
    /// card to that call rather than to whatever row shares its name.
    pub call_id: Option<String>,
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

/// One ledger row: a request and, once the operator decided, the decision.
/// The decision is written before the wait is resolved, so a crash between
/// the two leaves a decided row over a call that is still parked; the route
/// and the boot reconcile resolve it with the recorded decision.
#[derive(Clone, Debug)]
pub(crate) struct ApprovalRequest {
    /// The call that waits on the approval.
    pub call_id: String,
    pub decision: Option<ApprovalDecision>,
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
    #[error("approval arguments do not encode: {0}")]
    Arguments(#[from] serde_json::Error),
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
               -- The approval's id: the call that waits on it. The key that
               -- resolves the call is not stored; `Completions::parked`
               -- answers it for this call id.
               key_id TEXT PRIMARY KEY,
               tool_name TEXT NOT NULL,
               arguments_json TEXT NOT NULL,
               session_id TEXT NOT NULL,
               requested_at_ms INTEGER NOT NULL,
               decision TEXT CHECK (decision IS NULL OR decision IN ('approved', 'denied')),
               decided_at_ms INTEGER,
               -- The pair is one atomic fact: a decision exists only together
               -- with the timestamp it was made at. The CHECK exists on
               -- databases created under this schema; `decide` writes
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
        call_id: &str,
        args: &Value,
        session_id: &SessionId,
    ) -> Result<(), ApprovalError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApprovalError::Poisoned)?;
        connection.execute(
            "INSERT INTO approval_waits (
               key_id, tool_name, arguments_json,
               session_id, requested_at_ms, decision, decided_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL)
             ON CONFLICT(key_id) DO NOTHING",
            params![
                call_id,
                APPROVAL_TOOL_NAME,
                serde_json::to_string(args)?,
                session_id.as_str(),
                chrono::Utc::now().timestamp_millis(),
            ],
        )?;
        Ok(())
    }

    /// The undecided request the call `call_id` recorded, if it recorded one.
    pub(crate) fn undecided(
        &self,
        call_id: &str,
    ) -> Result<Option<PendingApproval>, ApprovalError> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApprovalError::Poisoned)?;
        connection
            .query_row(
                "SELECT key_id, tool_name, arguments_json, session_id, requested_at_ms
                 FROM approval_waits
                 WHERE key_id = ?1 AND decision IS NULL",
                [call_id],
                |row| {
                    let requested_at_ms: i64 = row.get(4)?;
                    let arguments_json: String = row.get(2)?;
                    let key: String = row.get(0)?;
                    Ok(PendingApproval {
                        call_id: Some(key.clone()),
                        key,
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
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Record `decision` on the request `call_id` unless it already has one,
    /// and answer the decision the row holds: the first one wins.
    pub(crate) fn decide(
        &self,
        call_id: &str,
        decision: ApprovalDecision,
    ) -> Result<ApprovalDecision, ApprovalError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApprovalError::Poisoned)?;
        connection.execute(
            "UPDATE approval_waits
             SET decision = ?2, decided_at_ms = ?3
             WHERE key_id = ?1 AND decision IS NULL",
            params![
                call_id,
                decision.as_str(),
                chrono::Utc::now().timestamp_millis()
            ],
        )?;
        let stored: Option<Option<String>> = connection
            .query_row(
                "SELECT decision FROM approval_waits WHERE key_id = ?1",
                [call_id],
                |row| row.get(0),
            )
            .optional()?;
        stored
            .flatten()
            .as_deref()
            .and_then(ApprovalDecision::from_stored)
            .ok_or_else(|| ApprovalError::NotPending(call_id.to_string()))
    }

    /// Delete the request `call_id`: its resolve answered, or its call is no
    /// longer parked.
    pub(crate) fn forget(&self, call_id: &str) -> Result<(), ApprovalError> {
        self.connection
            .lock()
            .map_err(|_| ApprovalError::Poisoned)?
            .execute("DELETE FROM approval_waits WHERE key_id = ?1", [call_id])?;
        Ok(())
    }

    /// The request the call `call_id` recorded, if the ledger still has it.
    pub(crate) fn request(&self, call_id: &str) -> Result<Option<ApprovalRequest>, ApprovalError> {
        Ok(self
            .requests()?
            .into_iter()
            .find(|request| request.call_id == call_id))
    }

    /// Every request the ledger holds, decided or not.
    pub(crate) fn requests(&self) -> Result<Vec<ApprovalRequest>, ApprovalError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| ApprovalError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT key_id, decision, tool_name, arguments_json, session_id
             FROM approval_waits
             ORDER BY requested_at_ms, key_id",
        )?;
        let rows = statement.query_map([], |row| {
            let decision: Option<String> = row.get(1)?;
            let arguments_json: String = row.get(3)?;
            let decision = decision
                .map(|stored| {
                    ApprovalDecision::from_stored(&stored).ok_or_else(|| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            format!("unknown approval decision `{stored}`").into(),
                        )
                    })
                })
                .transpose()?;
            Ok(ApprovalRequest {
                call_id: row.get(0)?,
                decision,
                tool: row.get(2)?,
                arguments: serde_json::from_str(&arguments_json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
                requesting_session: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

struct ApprovalToolProvider {
    approvals: WorkbenchApprovals,
}

impl ApprovalToolProvider {
    #[expect(
        clippy::expect_used,
        reason = "this module declares the tool or payload schema and admission checks its invariant"
    )]
    fn definition() -> ToolDefinition {
        ToolDefinition::raw(
            APPROVAL_TOOL_ID,
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
        ).expect("valid declared tool schemas")
        // The body only records its request and parks: a short bound.
        .with_execution(std::time::Duration::from_secs(30))
        .with_tool_binding(
            ToolBinding::new(["ops"], "apply_change").with_authority_type("Ops"),
        )
        // The attempt parks on a human decision, so admission records that it
        // may defer and its round pins the completion wait, whose key the
        // host reads from `Completions::parked` when the operator decides.
        .with_declaration(lash::tools::ToolDeclaration::deferring())
        // A human decides in their own time: the park lasts until the
        // decision or the end of the turn that asked.
        .with_park(lash::tools::ParkBound::UntilScopeEnd)
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

    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        (async {
            if call.name() != APPROVAL_TOOL_NAME {
                return ToolOutcome::err_fmt(format_args!(
                    "unknown approval tool `{}`",
                    call.name()
                ));
            }
            let session_id = match call.context.session_id() {
                Ok(session_id) => session_id.clone(),
                Err(error) => return ToolOutcome::err_fmt(error),
            };
            if let Err(error) =
                self.approvals
                    .record(call.context.call_id().as_str(), call.args, &session_id)
            {
                return ToolOutcome::err_fmt(error);
            }
            ToolOutcome::pending(PendingCompletion::new())
        })
        .await
        .into()
    }
}

/// The wait resolution a recorded decision derives: identical for the live
/// route and for the boot reconcile over decided rows.
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

#[expect(
    clippy::expect_used,
    reason = "the literal `agent_workbench` is a fixed valid host-namespace spelling, so validation cannot fail"
)]
pub(crate) fn denial_resolution() -> lash::Resolution {
    let mut error = lash::ExternalCompletionError::new(
        lash::provider::FailureCode::foreign(
            lash::provider::Namespace::host("agent_workbench").expect("valid namespace"),
            "approval_denied",
        )
        .expect("a validated host namespace is foreign-mintable"),
        "the operator denied this change",
    );
    error.raw = Some(json!({ "policy": "agent_workbench_human_approval" }));
    lash::Resolution::Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-3293: `decision`/`decided_at_ms` are one atomic fact — neither half
    /// may be written without the other, and the decision vocabulary is closed.
    #[test]
    fn decision_pair_is_constrained() {
        let approvals = WorkbenchApprovals::in_memory().expect("in-memory ledger");
        let connection = approvals.connection.lock().expect("ledger lock");
        connection
            .execute(
                "INSERT INTO approval_waits (
                   key_id, tool_name, arguments_json,
                   session_id, requested_at_ms, decision, decided_at_ms
                 ) VALUES ('k1', 'tool', '{}', 's', 0, 'approved', NULL)",
                [],
            )
            .expect_err("a decision without its timestamp must be rejected");
        connection
            .execute(
                "INSERT INTO approval_waits (
                   key_id, tool_name, arguments_json,
                   session_id, requested_at_ms, decision, decided_at_ms
                 ) VALUES ('k2', 'tool', '{}', 's', 0, NULL, 1)",
                [],
            )
            .expect_err("a timestamp without its decision must be rejected");
        connection
            .execute(
                "INSERT INTO approval_waits (
                   key_id, tool_name, arguments_json,
                   session_id, requested_at_ms, decision, decided_at_ms
                 ) VALUES ('k3', 'tool', '{}', 's', 0, 'shrugged', 1)",
                [],
            )
            .expect_err("a decision outside the vocabulary must be rejected");
    }
}
