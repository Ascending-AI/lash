//! The lash_vm process engine on the durable substrate (ADR 0132 §8, §10;
//! D-L6a, FIG-5198).
//!
//! A lash_vm process's engine state is its VM snapshot and the operation
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

pub use state::{LASH_VM_SEGMENT_STATE_VERSION, VM_RUN_STEP};

use crate::LashVmProcessEngine;

/// The lash_vm engine's own step body: `vm_run`. A sleep, alone or as an
/// aggregate's timer leaf, runs no body: the process sleeps on a durable
/// wake.
#[derive(Clone)]
pub struct LashVmEngineSteps {
    engine: Arc<LashVmProcessEngine>,
}

impl LashVmEngineSteps {
    /// The step bodies of `engine`.
    #[must_use]
    pub fn new(engine: Arc<LashVmProcessEngine>) -> Self {
        Self { engine }
    }
}

/// Host-selected policy for a VM segment, separate from recorded process bounds.
#[derive(Clone, Copy, Debug)]
pub struct VmSegmentPolicy {
    pub execution: std::time::Duration,
    pub attempts: std::num::NonZeroU32,
    pub retry_initial_ms: u64,
    pub retry_max_ms: u64,
}
impl VmSegmentPolicy {
    /// Existing provisional preset: 120 seconds, three attempts, immediate retries.
    /// No workload measurement backs these values. The VM segment recomputes
    /// only effect-free work from the last committed snapshot.
    pub const fn standard() -> Self {
        Self {
            execution: std::time::Duration::from_secs(120),
            attempts: std::num::NonZeroU32::MIN.saturating_add(2),
            retry_initial_ms: 0,
            retry_max_ms: 0,
        }
    }
}
impl Default for VmSegmentPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

#[async_trait::async_trait]
impl lash_core::EngineSteps for LashVmEngineSteps {
    fn kinds(&self) -> Vec<EngineStepKind> {
        vec![EngineStepKind::new(VM_RUN_STEP)]
    }

    fn execution(&self, _kind: &EngineStepKind) -> std::time::Duration {
        self.engine.segment_policy.execution
    }

    fn retry(&self, _kind: &EngineStepKind) -> lash_core::ExecutionPolicy {
        lash_core::ExecutionPolicy::repeatable(
            self.engine.segment_policy.attempts,
            self.engine.segment_policy.retry_initial_ms,
            self.engine.segment_policy.retry_max_ms,
        )
    }

    async fn run(&self, run: EngineStepRun, cancel: CancellationToken) -> SettledOutput {
        vm_run::run_vm_step(&self.engine, run, cancel).await
    }
}

#[cfg(test)]
#[path = "engine/advance_tests.rs"]
mod advance_tests;

impl LashVmProcessEngine {
    /// Observes process language execution through the deployment's trace sinks.
    #[must_use]
    pub fn with_trace_runtime(mut self, runtime: lash_core::trace::TraceRuntime) -> Self {
        self.trace_runtime = Some(runtime);
        self
    }
}
