//! The process-mail seam: a detached process that idles; the host signals
//! it (`mail.process`), the process answers the signal with an event, and
//! the host then cancels it (`process.cancel`); the process answers its
//! cancel with its terminal (`process.terminal`).
//!
//! Laws: the signal reached the engine as an event before the cancel was
//! requested, and the process ended cancelled by the operator.

use std::sync::{Arc, Mutex};

use lash_core::sync::MutexExt as _;
use lash_core_execution::{
    CancelOrigin, CancelRequest, DurableProcessWork, ProcessId, ProcessSignal,
    ProcessSignalIdentity, ProcessWorkSubstrate as _,
};
use lash_durable::domain::SIGNAL_MAIL;
use lash_durable::{CommitLabel, DurableError, MailKind, MailTx, StoreFailure, StoreFailureKind};
use lash_durable_test::{Cut, SimNodes};
use serde_json::json;

use super::{LOOK_EVERY, LOOKS, ended, event_types, find, outcome, process_actor, register};
use crate::crash_matrix::deployment::Workload;
use crate::crash_matrix::engine::{SIGNALLED_EVENT, hold};
use crate::crash_matrix::world::{World, poll, retry};

#[derive(Default)]
pub struct SignalCase {
    process: Mutex<Option<ProcessId>>,
}

impl SignalCase {
    fn process(&self) -> Option<ProcessId> {
        self.process.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl Workload for SignalCase {
    async fn seed(&self, world: &Arc<World>, _nodes: &Arc<SimNodes>) -> Result<(), String> {
        let process = register(world, hold("p"), None).await?;
        *self.process.lock_recover() = Some(process.clone());
        let host = Arc::clone(world);
        world.spawn(async move { signal_then_cancel(&host, &process).await });
        Ok(())
    }

    async fn done(&self, _world: &World, nodes: &SimNodes) -> bool {
        match self.process() {
            Some(process) => ended(nodes, &[process]).await,
            None => false,
        }
    }

    async fn laws(&self, world: &World, _nodes: &SimNodes, _cut: Option<&Cut>) -> Vec<String> {
        let Some(process) = self.process() else {
            return vec!["the process was never seeded".to_owned()];
        };
        let mut violations = Vec::new();
        if !event_types(world, &process)
            .await
            .iter()
            .any(|event| event == SIGNALLED_EVENT)
        {
            violations.push("the signal never reached the engine".to_owned());
        }
        match outcome(world, &process).await {
            Some(end) if find(&end, "origin") == Some(&json!("operator_requested")) => {}
            other => violations.push(format!("the process did not end by its cancel: {other:?}")),
        }
        violations
    }
}

/// A store failure for a refused process-work call, so the host retries
/// what a real host would.
fn work_failure(error: impl std::fmt::Display) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Unavailable,
        message: error.to_string(),
    })
}

/// The host: signal the process, wait for its answer, then cancel it.
async fn signal_then_cancel(world: &Arc<World>, process: &ProcessId) {
    let Ok(actor) = process_actor(process) else {
        return;
    };
    let Ok(identity) = ProcessSignalIdentity::new(process.clone(), "poke", "lash-sim-poke") else {
        return;
    };
    let Ok(body) = serde_json::to_string(&ProcessSignal::new(identity, json!({ "poke": 1 })))
    else {
        return;
    };
    let sent = retry(world, |host| {
        let mut tx = MailTx::new();
        tx.append(actor.clone(), MailKind::new(SIGNAL_MAIL), body.clone());
        async move {
            host.commit_mail(tx, CommitLabel::MAIL_PROCESS)
                .await
                .map(drop)
        }
    })
    .await;
    if sent.is_err() {
        return;
    }
    let answered = poll(world, LOOK_EVERY, LOOKS, || async {
        event_types(world, process)
            .await
            .iter()
            .any(|event| event == SIGNALLED_EVENT)
            .then_some(())
    })
    .await;
    if answered.is_none() {
        return;
    }
    let requested_at = world.now_ms();
    let cancelled = retry(world, |host| {
        let process = process.clone();
        async move {
            DurableProcessWork::new(host)
                .deliver_cancel(
                    &process,
                    &CancelRequest::new(CancelOrigin::OperatorRequested, "lash-sim", requested_at),
                    "",
                )
                .await
                .map_err(work_failure)
        }
    })
    .await;
    if cancelled.is_ok() {
        world.note("cancel.answered");
    }
}
