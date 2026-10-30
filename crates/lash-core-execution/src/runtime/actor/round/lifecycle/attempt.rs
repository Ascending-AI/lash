//! Runs one admitted attempt under its call decision scope.

use super::super::decision;
use super::super::{ToolBody, run_bounded};
use super::*;
use std::sync::{Mutex, PoisonError};
use tracing::Instrument as _;

impl Lifecycle {
    pub(super) fn spawn(
        &mut self,
        execution: AdmittedExecution,
        retained: Option<serde_json::Value>,
    ) {
        let body = self.bodies.body(&execution);
        let stop_grace = self.bodies.stop_grace();
        let cx = self.cx.clone();
        let cancel = self.token(execution.call());
        // Only a step that may run again from its committed start can drop
        // an attempt: a capacity verdict from any other records the
        // attempt's `Interrupted`.
        let pinned = execution.policy();
        let honours_capacity = matches!(
            pinned,
            lash_kernel_value::tool_contract::ExecutionPolicy::Repeatable { .. }
        ) && self
            .policies
            .permits_repeat(execution.draft().tool(), pinned);
        self.in_flight.insert(execution.id().clone());
        let protected = CancellationToken::new();
        if retained.is_some() {
            protected.cancel();
        }
        let scope = Arc::new(decision::FinalScope {
            execution: execution.clone(),
            sender: self.final_sender.clone(),
            state: std::sync::Mutex::new(match retained {
                Some(payload) => decision::FinalState::Protected(payload),
                None => decision::FinalState::Open,
            }),
            protected,
        });
        let outcome_scope = Arc::clone(&scope);
        let attempt = async move {
            type Carried = (
                Vec<StoreLocalEffect>,
                Option<crate::ProcessId>,
                Option<DurableInstant>,
                Option<CapacityVerdict>,
                Option<StepCandidate>,
            );
            let carried: Arc<Mutex<Carried>> = Arc::default();
            let slot = Arc::clone(&carried);
            let tool_body: ToolBody = Box::new(move |token| {
                Box::pin(async move {
                    let result = body(token).await;
                    *slot.lock().unwrap_or_else(PoisonError::into_inner) = (
                        result.store_local,
                        result.terminal,
                        result.timer,
                        result.capacity,
                        result.candidate,
                    );
                    result.output
                })
            });
            let result = run_bounded(&cx, &execution, tool_body, &cancel, stop_grace)
                .await
                .and_then(|output| {
                    let (store_local, terminal, timer, capacity, candidate) = std::mem::take(
                        &mut *carried.lock().unwrap_or_else(PoisonError::into_inner),
                    );
                    match capacity {
                        Some(verdict) if honours_capacity => Err(Stop::Capacity(verdict)),
                        _ => Ok(MemberOutcome {
                            output,
                            store_local,
                            terminal,
                            timer,
                            capacity: None,
                            candidate,
                        }),
                    }
                });
            Finished {
                id: execution.id().clone(),
                held: HeldOutcome {
                    final_at_outcome: result.is_ok() && outcome_scope.at_outcome(),
                    result,
                },
            }
        };
        self.running.spawn(
            lash_core_ids::operational_metrics::turn::carry(
                decision::FINAL_SCOPE.scope(scope, attempt),
            )
            .instrument(tracing::Span::current()),
        );
    }
}
