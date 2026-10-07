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

pub(crate) mod advance;
pub(crate) mod injection;
pub(crate) mod state;
pub(crate) mod vm_run;

use std::sync::Arc;

use lash_core::{EngineStepKind, EngineStepRun, SettledOutput};
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

#[cfg(test)]
#[path = "engine/advance_tests.rs"]
mod advance_tests;
