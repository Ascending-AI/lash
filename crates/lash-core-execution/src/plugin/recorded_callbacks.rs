//! The recorded step of a sequential callback slot (K10, FIG-4878).
//!
//! Before-turn and after-turn callbacks run inside one recorded step per
//! turn. The step records the slot's decisions
//! with the resolutions of the state commands its callbacks returned; replay
//! serves both and runs no callback or reducer. A callback's failure is the
//! step's recorded answer, and publishes none of the slot's commands.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::{PluginError, PluginSession, RecordedTurnContribution};
use crate::{RuntimeEffectCommand, RuntimeEffectControllerError, RuntimeEffectOutcome};

/// Which sequential callback slot a [`RuntimeEffectCommand::PluginCallbacks`]
/// step records.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecordedCallbackPhase {
    /// A turn's before-turn callbacks.
    BeforeTurn,
    /// A turn's after-turn callbacks.
    AfterTurn,
}

/// The callbacks a step runs, to their decisions.
pub type PluginCallbackBody = Pin<
    Box<dyn Future<Output = Result<Vec<RecordedTurnContribution>, PluginError>> + Send + 'static>,
>;

struct PluginCallbackRunner {
    plugins: Arc<PluginSession>,
    body: PluginCallbackBody,
}

#[async_trait::async_trait]
impl crate::runtime::effect::executor::RuntimeEffectLocalRunner for PluginCallbackRunner {
    fn plugin_state_session(&self) -> Option<Arc<PluginSession>> {
        Some(Arc::clone(&self.plugins))
    }

    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if !matches!(
            envelope.command,
            RuntimeEffectCommand::PluginCallbacks { .. }
        ) {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "a plugin callback step received another command",
            ));
        }
        let Self { plugins, body } = *self;
        let (result, proposals) = super::collect_proposals(&plugins, body).await;
        if result.is_ok() {
            super::propose_all(&plugins, proposals)?;
        }
        Ok(RuntimeEffectOutcome::PluginCallbacks { result })
    }
}

/// Run `body` as the recorded `phase` step at `step` under `controller`: the
/// decisions it returns, served from the journal on replay.
///
/// # Errors
///
/// The engine's failure to record or serve the step. A callback's own
/// failure is the step's recorded answer, in the inner result.
pub async fn record_plugin_callbacks(
    controller: &crate::ScopedEffectController<'_>,
    attribution: crate::RuntimeAttribution,
    step: String,
    phase: RecordedCallbackPhase,
    plugins: Arc<PluginSession>,
    body: PluginCallbackBody,
) -> Result<Result<Vec<RecordedTurnContribution>, PluginError>, RuntimeEffectControllerError> {
    let invocation = crate::RuntimeEffectInvocation::new(
        crate::EffectAddress::new(controller.execution_scope().clone(), step.clone())?,
        attribution,
        step,
    );
    let outcome = controller
        .execute_effect(
            crate::RuntimeEffectEnvelope::new(
                invocation,
                RuntimeEffectCommand::PluginCallbacks { phase },
            ),
            crate::runtime::effect::executor::owned_runner_executor(
                Box::new(PluginCallbackRunner { plugins, body }),
                None,
            ),
        )
        .await?;
    match outcome {
        RuntimeEffectOutcome::PluginCallbacks { result } => Ok(result),
        other => Err(RuntimeEffectControllerError::wrong_outcome(
            crate::RuntimeEffectKind::PluginCallbacks,
            other.kind(),
        )),
    }
}
