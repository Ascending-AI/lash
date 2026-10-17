//! A process's terminal transaction and its parks (ADR 0132 §3, §11).

use lash_durable::domain::{ExecRootWrite, ProcessWrite, ScopeKey};
use lash_durable::{ActorTx, DomainWrite, DurableError, StoreFailure, StoreFailureKind};
use lash_execution_contracts::park::ProcessParkReason;

use crate::runtime::actor::waits;
use lash_execution_contracts::process::ProcessOutcome;
use lash_protocol_types::CancelOrigin;
use lash_protocol_types::ProcessId;

/// The terminal of a process cancelled with `origin`: forced when lash
/// ended it at its grace.
#[must_use]
pub fn cancelled(origin: CancelOrigin, forced: bool) -> ProcessOutcome {
    let cancellation = lash_protocol_types::ToolCancellation::runtime("the process was cancelled")
        .with_origin(origin);
    let cancellation = if forced {
        cancellation.forced()
    } else {
        cancellation
    };
    lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
        lash_protocol_types::ToolCallOutput::cancelled(cancellation),
    )
}

/// Record `process`'s terminal transaction on `tx`, the same for every
/// cause: the terminal and its event, the process-terminal waits on it
/// resolved, its own pending waits revoked, its engine state deleted, and
/// its cascade begun. The cascade over its `Until` children runs in
/// further `cascade.batch` commits while the actor stays runnable.
///
/// # Errors
///
/// The outcome's encoding.
pub fn record_terminal(
    tx: &mut ActorTx,
    process: &ProcessId,
    outcome: &ProcessOutcome,
) -> Result<(), DurableError> {
    let outcome_json = serde_json::to_string(&crate::Durable(outcome)).map_err(|error| {
        DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Corrupt,
            message: format!("the terminal of process {process} does not encode: {error}"),
        })
    })?;
    tx.write(DomainWrite::Process(ProcessWrite::Terminal {
        process: process.clone(),
        outcome_json,
    }));
    waits::resolve_process_terminal_waits(tx, process, outcome)?;
    waits::revoke_scope(tx, &ScopeKey::Process(process.clone()));
    tx.write(DomainWrite::ExecRoot(ExecRootWrite::Clear {
        exec: lash_durable::domain::ExecKey::Process(process.clone()),
    }));
    Ok(())
}

/// Park the committing actor with `reason` on `tx`.
pub fn record_park(tx: &mut ActorTx, reason: &ProcessParkReason) {
    tx.write(DomainWrite::ActorPark {
        reason_json: Some(reason.encode()),
    });
    tx.give_up(lash_durable::Release::Parked);
}

impl super::activation::ProcessActivation {
    /// Commit `tx`, which records `process`'s terminal, and report it with
    /// the workflow node spans `nodes` the terminal closes.
    ///
    /// The acknowledged commit is the terminal's first writer: an owner that
    /// finds the process ended commits no terminal and reports none. The
    /// record is read back for the scope its registration retained and the
    /// time its terminal retained; an owner that loses the acknowledgement,
    /// or the read, reports nothing.
    pub(super) async fn commit_terminal(
        &self,
        owned: &lash_durable::runner::Owned,
        tx: ActorTx,
        process: &ProcessId,
        nodes: super::node_trace::NodeCloses,
    ) -> Result<(), DurableError> {
        owned
            .commit(tx, lash_durable::CommitLabel::PROCESS_TERMINAL)
            .await?;
        let Some(tracing) = self
            .tracing
            .as_ref()
            .filter(|tracing| tracing.is_observed())
        else {
            return Ok(());
        };
        let Ok(Some(record)) = self.backend.process_registry().get_process(process).await else {
            return Ok(());
        };
        if let lash_execution_contracts::process::ProcessLifecycleState::Terminal {
            occurred_at_ms,
            ..
        } = &record.lifecycle
        {
            nodes.publish(Some(tracing), &record, *occurred_at_ms);
        }
        let (
            Some(scope),
            lash_execution_contracts::process::ProcessLifecycleState::Terminal {
                outcome,
                occurred_at_ms,
            },
        ) = (record.trace, record.lifecycle)
        else {
            return Ok(());
        };
        let status = match outcome.status() {
            lash_execution_contracts::process::TerminalProcessStatus::Completed => {
                lash_trace::TraceDomainStatus::Completed
            }
            lash_execution_contracts::process::TerminalProcessStatus::Cancelled => {
                lash_trace::TraceDomainStatus::Cancelled
            }
            lash_execution_contracts::process::TerminalProcessStatus::Failed
            | lash_execution_contracts::process::TerminalProcessStatus::Abandoned => {
                lash_trace::TraceDomainStatus::Failed
            }
        };
        let subject = match record.input.as_ref() {
            lash_execution_contracts::process::ProcessInput::Engine { kind, .. }
                if record.trace_class
                    == lash_execution_contracts::process::ProcessTraceClass::Workflow =>
            {
                lash_trace::TraceDomainSubject::Workflow {
                    kind: kind.clone(),
                    name: record.identity.label.clone(),
                }
            }
            input => lash_trace::TraceDomainSubject::Process {
                kind: match input {
                    lash_execution_contracts::process::ProcessInput::SessionTurn { .. } => {
                        "session_turn".into()
                    }
                    lash_execution_contracts::process::ProcessInput::Engine { kind, .. } => {
                        kind.clone()
                    }
                },
                label: record.identity.label.clone(),
            },
        };
        let started_at_ms = scope.started_at_ms;
        tracing.unreplayed(Some(scope)).transition(
            Some(&lash_trace::EmissionPermit::new_transition()),
            occurred_at_ms,
            lash_trace::TraceTransitionKind::Terminal,
            0,
            || {
                (
                    lash_trace::TraceContext::default(),
                    lash_trace::TraceEvent::DomainCompleted {
                        completion: lash_trace::TraceDomainCompletion::new(
                            subject,
                            started_at_ms,
                            status,
                        ),
                    },
                )
            },
        );
        Ok(())
    }
}
