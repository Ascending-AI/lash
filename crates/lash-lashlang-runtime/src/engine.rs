//! The lashlang process engine on the durable substrate (ADR 0132 §8, §10;
//! D-L6a, FIG-5198).
//!
//! A lashlang process's engine state is its VM snapshot and the operation
//! the VM parked on ([`state`]). `advance` is pure ([`advance`]); the VM runs
//! only in the `vm_run` engine step ([`vm_run`]), from the committed
//! snapshot to its next quiet point, and each operation it issues there
//! leaves as its own action. On resume, the operation's outcome is fed back
//! by the operation's number ([`injection`]): nothing re-runs and nothing is
//! dispatched again, and the VM never runs from its program's entry.
//!
//! A trigger command the VM issues is a host step ([`LashlangHostSteps`]):
//! it runs the trigger command handler once over the process's step
//! context, which acts as the process's recorded originator, and its
//! trigger write is the step's store-local effect.

pub(crate) mod advance;
pub(crate) mod injection;
pub(crate) mod state;
pub(crate) mod vm_run;

use std::sync::Arc;

use lash_core::{EngineStepKind, EngineStepRun, HostStepRun, SettledOutput};
use tokio_util::sync::CancellationToken;

pub use state::{LASHLANG_SEGMENT_STATE_VERSION, TIMER_STEP, VM_RUN_STEP};

use crate::LashlangProcessEngine;

/// The lashlang engine's own step bodies: `vm_run` and an aggregate's
/// `timer`.
#[derive(Clone)]
pub struct LashlangEngineSteps {
    engine: Arc<LashlangProcessEngine>,
}

impl LashlangEngineSteps {
    /// The step bodies of `engine`.
    #[must_use]
    pub fn new(engine: Arc<LashlangProcessEngine>) -> Self {
        Self { engine }
    }
}

#[async_trait::async_trait]
impl lash_core::EngineSteps for LashlangEngineSteps {
    fn kinds(&self) -> Vec<EngineStepKind> {
        vec![
            EngineStepKind::new(VM_RUN_STEP),
            EngineStepKind::new(TIMER_STEP),
        ]
    }

    async fn run(&self, run: EngineStepRun, cancel: CancellationToken) -> SettledOutput {
        if run.kind.0 == TIMER_STEP {
            vm_run::run_timer_step(run, cancel).await
        } else {
            vm_run::run_vm_step(&self.engine, run, cancel).await
        }
    }
}

/// The lashlang engine's host steps: a process's trigger commands, run
/// through the trigger command handler foreground cells share.
#[derive(Clone)]
pub struct LashlangHostSteps {
    engine: Arc<LashlangProcessEngine>,
}

impl LashlangHostSteps {
    /// The host steps of `engine`.
    #[must_use]
    pub fn new(engine: Arc<LashlangProcessEngine>) -> Self {
        Self { engine }
    }
}

#[async_trait::async_trait]
impl lash_core::EngineHostSteps for LashlangHostSteps {
    fn serves(&self, operation: &str) -> bool {
        lashlang::TriggerHostOperation::from_host_operation(operation).is_some()
    }

    async fn run(
        &self,
        context: lash_core::RuntimeExecutionContext<'static>,
        run: HostStepRun,
    ) -> lash_core::ToolCallOutput {
        let Some(operation) = lashlang::TriggerHostOperation::from_host_operation(&run.operation)
        else {
            return lash_core::ToolCallOutput::failure(lash_core::ToolFailure::runtime(
                lash_core::ToolFailureClass::Internal,
                "host_step_unknown",
                format!("`{}` is not a trigger command", run.operation),
            ));
        };
        let answer = crate::execute_trigger_operation(
            &self.engine.workers,
            &context,
            &self.engine.artifact_store,
            operation,
            run.input,
            run.call.to_string(),
        )
        .await
        .and_then(|value| crate::lashlang_value_to_json(&value));
        match answer {
            Ok(value) => lash_core::ToolCallOutput::success(value),
            Err(error) => lash_core::ToolCallOutput::failure(trigger_step_failure(&error)),
        }
    }
}

/// A refused trigger command as its step's tool failure, its schema cause
/// kept typed.
fn trigger_step_failure(error: &lashlang::ExecutionHostError) -> lash_core::ToolFailure {
    let failure = lash_core::ToolFailure::runtime(
        lash_core::ToolFailureClass::Execution,
        "trigger_command_failed",
        error.message(),
    );
    match error.schema_admission() {
        Some(source) => failure.with_cause(lash_core::ToolFailureCause::SchemaAdmission {
            source: source.clone(),
        }),
        None => failure,
    }
}

#[cfg(test)]
#[path = "engine/advance_tests.rs"]
mod advance_tests;
