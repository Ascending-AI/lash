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

pub(crate) mod advance;
pub(crate) mod injection;
pub(crate) mod state;
mod trace;
pub(crate) mod vm_run;

use std::sync::Arc;

use lash_core::{EngineStepKind, EngineStepRun, SettledOutput};
use tokio_util::sync::CancellationToken;

pub use state::{LASHLANG_SEGMENT_STATE_VERSION, VM_RUN_STEP};

use crate::LashlangProcessEngine;

/// The lashlang engine's own step body: `vm_run`. A sleep, alone or as an
/// aggregate's timer leaf, runs no body: the process sleeps on a durable
/// wake.
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

/// The lashlang engine's bound on one run of a step it issues, a VM run
/// segment: the engine sets it, as a host sets its tools' bounds.
const LASHLANG_STEP_EXECUTION: std::time::Duration = std::time::Duration::from_secs(2 * 60);

/// How many times a `vm_run` may run before its failure ends the process:
/// a VM segment is a recomputation from the committed snapshot with no
/// effect of its own, so a worker fault or a crash runs it again at once.
const VM_RUN_ATTEMPTS: std::num::NonZeroU32 = std::num::NonZeroU32::MIN.saturating_add(2);

#[async_trait::async_trait]
impl lash_core::EngineSteps for LashlangEngineSteps {
    fn kinds(&self) -> Vec<EngineStepKind> {
        vec![EngineStepKind::new(VM_RUN_STEP)]
    }

    fn execution(&self, _kind: &EngineStepKind) -> std::time::Duration {
        LASHLANG_STEP_EXECUTION
    }

    fn retry(&self, _kind: &EngineStepKind) -> lash_core::ExecutionPolicy {
        lash_core::ExecutionPolicy::repeatable(VM_RUN_ATTEMPTS, 0, 0)
    }

    async fn run(&self, run: EngineStepRun, cancel: CancellationToken) -> SettledOutput {
        vm_run::run_vm_step(&self.engine, run, cancel).await
    }
}

#[cfg(test)]
#[path = "engine/advance_tests.rs"]
mod advance_tests;

impl LashlangProcessEngine {
    /// Observes process language execution through the deployment's trace sinks.
    #[must_use]
    pub fn with_trace_runtime(mut self, runtime: lash_core::trace::TraceRuntime) -> Self {
        self.trace_runtime = Some(runtime);
        self
    }
}
