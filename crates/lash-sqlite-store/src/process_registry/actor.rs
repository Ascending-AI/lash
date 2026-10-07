//! The process actor's registry writes (L6, FIG-5175): its first cancel
//! request and its terminal, applied on the connection of a durable commit,
//! so the registry row and the actor's rows commit together.

use super::*;
use lash_sansio::ProcessId;

/// What recording a cancel request found.
pub(crate) enum CancelRecorded {
    /// This request is the first: recorded at `at_ms`.
    Requested { at_ms: u64 },
    /// An earlier request stands, recorded at `at_ms`.
    AlreadyRequested { at_ms: u64 },
    /// The process is terminal.
    Ended,
}

impl SqliteProcessRegistry {
    /// Record `origin`'s cancel of `process_id` at `now_ms` unless one is
    /// recorded: the first request wins and keeps its timestamp.
    pub(crate) fn record_cancel_conn(
        conn: &rusqlite::Connection,
        process_id: &ProcessId,
        origin: lash_core_execution::CancelOrigin,
        requester: &str,
        now_ms: u64,
        fleet_format: lash_core_execution::FleetFormat,
    ) -> Result<CancelRecorded, lash_core_execution::PluginError> {
        let mut record = Self::require_process_conn(conn, process_id)?;
        if record.is_terminal() {
            return Ok(CancelRecorded::Ended);
        }
        if let Some(existing) = record.cancel_request.as_deref() {
            return Ok(CancelRecorded::AlreadyRequested {
                at_ms: existing.requested_at_ms,
            });
        }
        let request = lash_core_execution::CancelRequest::new(origin, requester, now_ms);
        match lash_core_execution::runtime::prepare_process_transition(
            &record,
            ProcessTransition::RequestCancel(request),
        )? {
            ProcessTransitionPlan::Unchanged => {}
            ProcessTransitionPlan::Append(append) => {
                Self::append_event_conn(conn, &mut record, *append, now_ms, fleet_format)?;
            }
        }
        Ok(CancelRecorded::Requested { at_ms: now_ms })
    }

    /// End `process_id` with `output` under its actor's `epoch`. A process
    /// already terminal keeps its first terminal; answers whether this call
    /// ended it.
    pub(crate) fn record_terminal_conn(
        conn: &rusqlite::Connection,
        process_id: &ProcessId,
        output: &lash_core_execution::ProcessAwaitOutput,
        epoch: u64,
        now_ms: u64,
        fleet_format: lash_core_execution::FleetFormat,
    ) -> Result<bool, lash_core_execution::PluginError> {
        let mut record = Self::require_process_conn(conn, process_id)?;
        if record.is_terminal() {
            return Ok(false);
        }
        let output = output.clone().with_cancel_origin(
            record
                .cancel_request
                .as_deref()
                .map(|request| request.origin),
        );
        let authority = lash_core_execution::ProcessCompletionAuthority::ActorEpoch { epoch };
        let mut batch = ProcessEventBatch::for_fleet(fleet_format);
        let request = lash_core_execution::facade_support::terminal_append_request(
            process_id,
            &output,
            Some(&authority),
        );
        batch.stage(conn, &mut record, request, now_ms)?;
        batch.commit(conn, &record)?;
        Ok(true)
    }

    /// Append `event_type` with `payload` to `process_id` under
    /// `replay_key`, on a durable commit's connection: a repeat under the
    /// same key is a no-op.
    pub(crate) fn record_event_conn(
        conn: &rusqlite::Connection,
        process_id: &ProcessId,
        event_type: &str,
        payload: serde_json::Value,
        replay_key: &str,
        now_ms: u64,
        fleet_format: lash_core_execution::FleetFormat,
    ) -> Result<(), lash_core_execution::PluginError> {
        let mut record = Self::require_process_conn(conn, process_id)?;
        let request =
            ProcessEventAppendRequest::new(event_type, payload).with_replay_key(replay_key);
        Self::append_event_conn(conn, &mut record, request, now_ms, fleet_format)?;
        Ok(())
    }
}
