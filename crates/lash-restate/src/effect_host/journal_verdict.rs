//! Restate's journal verdict (ADR 0113 §2.5): whether an effect journal may
//! still replay or append, which the artifact-cleanup executor asks before it
//! severs anything an execution referrer or a gate protects.

use std::sync::Arc;

use lash_core::{AwaitEventResolver as _, ExecutionScope, RuntimeError, RuntimeErrorCode};
use lash_sansio::{ProcessId, SessionId};

use super::RestateEffectHost;
use crate::LashService;

/// The engine's reads behind [`lash_core::EffectHost::journal_replay`] (ADR 0113 §2.5):
/// Restate's invocation table through the admin API, and the store set's
/// root terminals and process rows.
#[derive(Clone)]
pub struct RestateJournalAuthority {
    admin: crate::RestateAdminClient,
    stores: Arc<dyn lash_core::StoreSet>,
}

impl RestateJournalAuthority {
    pub fn new(admin: crate::RestateAdminClient, stores: Arc<dyn lash_core::StoreSet>) -> Self {
        Self { admin, stores }
    }
}

/// Restate's verdict on one journal (ADR 0113 §2.5). A session delete
/// runs no publication and is always settled. A runtime operation is
/// settled once its durable waits are retired under `WhenQuiescent`,
/// which the facade does only after the operation's commit. A turn is
/// settled once its root has terminal evidence and Restate holds no open
/// run of that root; a queue drain once Restate holds no open drive of
/// its session and no open run of any of its roots; a process once it is
/// terminal, or pruned, and Restate holds no open run of any of its
/// segments. A wait retirement alone settles nothing else.
pub(super) async fn journal_replay(
    host: &RestateEffectHost,
    journal: &lash_sansio::EffectJournalIdentity,
) -> Result<lash_core::JournalReplay, RuntimeError> {
    use lash_core::JournalReplay::{MayReplay, Settled};
    let scope = ExecutionScope::from_journal_key(journal.key()).ok_or_else(|| {
        RuntimeError::new(
            RuntimeErrorCode::EngineAwaitEventRevocationRead,
            format!("journal `{}` names no execution scope", journal.key()),
        )
    })?;
    match &scope {
        ExecutionScope::SessionDelete { .. } => return Ok(Settled),
        ExecutionScope::RuntimeOperation { .. } => {
            return Ok(
                if host.controller.await_event_scope_is_retired(&scope).await? {
                    Settled
                } else {
                    MayReplay
                },
            );
        }
        ExecutionScope::Turn { .. }
        | ExecutionScope::QueueDrain { .. }
        | ExecutionScope::Process { .. } => {}
    }
    let Some(authority) = host.journal_authority.get() else {
        return Ok(MayReplay);
    };
    let settled = match &scope {
        ExecutionScope::Turn {
            session_id,
            turn_id,
        } => turn_journal_settled(authority, host.namespace(), session_id, turn_id).await?,
        ExecutionScope::QueueDrain { session_id, .. } => {
            drain_journal_settled(authority, host.namespace(), session_id).await?
        }
        ExecutionScope::Process { process_id } => {
            process_journal_settled(authority, host.namespace(), process_id).await?
        }
        ExecutionScope::SessionDelete { .. } | ExecutionScope::RuntimeOperation { .. } => true,
    };
    Ok(if settled { Settled } else { MayReplay })
}

fn journal_read_error(what: &str, error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::EngineAwaitEventRevocationRead,
        format!("journal verdict could not read {what}: {error}"),
    )
}

/// A turn's journal is settled once its root has terminal evidence and
/// Restate holds no open run of the root's workflow, on any lane.
async fn turn_journal_settled(
    authority: &RestateJournalAuthority,
    namespace: &crate::RestateNamespace,
    session_id: &SessionId,
    root: &lash_core::TurnId,
) -> Result<bool, RuntimeError> {
    if authority
        .stores
        .session_store_factory()
        .root_terminal(session_id, root)
        .await
        .map_err(|error| journal_read_error("the root terminal", error))?
        .is_none()
    {
        return Ok(false);
    }
    let runs = authority
        .admin
        .root_runs(
            namespace,
            &[crate::session_driver::turn_workflow_key(session_id, root)],
        )
        .await
        .map_err(|error| journal_read_error("root runs", error))?;
    Ok(!runs.iter().any(|run| run.status.is_open()))
}

/// A queue drain names no root, so its journal is settled only once Restate
/// holds no open drive of its session and no open run of any of the
/// session's roots: nothing of the session can replay it.
async fn drain_journal_settled(
    authority: &RestateJournalAuthority,
    namespace: &crate::RestateNamespace,
    session_id: &SessionId,
) -> Result<bool, RuntimeError> {
    let session = sql_text(session_id.as_str());
    // `LIKE` wildcards inside the session id only widen the match to other
    // sessions' runs, which can only answer `MayReplay`.
    let roots_prefix = sql_text(&format!(
        "{}:{}%",
        session_id.as_str().len(),
        session_id.as_str()
    ));
    let drives = namespace.service_lanes_sql(LashService::SessionDriver);
    let roots = namespace.service_lanes_sql(LashService::TurnDriver);
    let runs: Vec<crate::RestateInvocationStatus> = authority
        .admin
        .query_json(&format!(
            "SELECT id, target, target_service_name, target_service_key, target_handler_name, \
             status, completion_result, completion_failure FROM sys_invocation WHERE \
             ({drives} AND target_handler_name = 'drive' AND target_service_key = {session}) \
             OR ({roots} AND target_handler_name = 'run' AND target_service_key LIKE \
             {roots_prefix})"
        ))
        .await
        .map_err(|error| journal_read_error("session runs", error))?;
    Ok(!runs.iter().any(|run| run.status.is_open()))
}

/// A process journal is settled once the process is terminal, or pruned
/// (prune requires terminal and retired journals), and Restate holds no
/// open run of any of its segments.
async fn process_journal_settled(
    authority: &RestateJournalAuthority,
    namespace: &crate::RestateNamespace,
    process_id: &ProcessId,
) -> Result<bool, RuntimeError> {
    let Some(record) = authority
        .stores
        .process_registry()
        .get_process(process_id)
        .await
        .map_err(|error| journal_read_error("the process row", error))?
    else {
        return Ok(true);
    };
    if !record.status.is_terminal() {
        return Ok(false);
    }
    let last_segment = record
        .external_ref
        .as_ref()
        .filter(|reference| reference.backend == "restate")
        .map_or(0, lash_core::ProcessExternalRef::segment_ordinal);
    let segment_keys: Vec<String> = (0..=last_segment)
        .map(|ordinal| crate::process::process_segment_workflow_key(process_id, ordinal))
        .collect();
    let runs = authority
        .admin
        .segment_runs(namespace, &segment_keys)
        .await
        .map_err(|error| journal_read_error("segment runs", error))?;
    Ok(!runs.iter().any(|run| run.status.is_open()))
}

/// `text` as a SQL string literal.
fn sql_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}
