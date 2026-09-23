use super::*;

pub(super) struct LanguageRuntimeValueRunner {
    pub(super) clock: Arc<dyn crate::Clock>,
}

/// The operation prefix a replayed language run's seal is journaled under
/// (FIG-3586): `{prefix}:{facts}`, where the facts are the run's issued
/// count and dispatched-ordinals digest. They are part of the envelope, so a
/// redrive that issued a different run refuses at the seal.
pub const RUN_SEAL_OPERATION: &str = "lashlang-run-seal";

/// Records the value the language run supplied for its seal: the producer
/// that wrote the journal. Replay serves the recorded value, never this one —
/// which is what makes it attribution rather than a check.
pub(super) struct RunSealRunner {
    value: serde_json::Value,
}

impl RuntimeEffectLocalExecutor<'_> {
    /// Builds the executor a language run's seal journals `value` through
    /// (FIG-3586).
    pub fn run_seal(value: serde_json::Value) -> RuntimeEffectLocalExecutor<'static> {
        RuntimeEffectLocalExecutor {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(Box::new(
                RunSealRunner { value },
            ))),
            replay_trace: None,
        }
    }

    /// Builds a journaled language-runtime value executor using the host clock.
    pub fn language_runtime_value(
        clock: Arc<dyn crate::Clock>,
    ) -> RuntimeEffectLocalExecutor<'static> {
        RuntimeEffectLocalExecutor {
            state: RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(Box::new(
                LanguageRuntimeValueRunner { clock },
            ))),
            replay_trace: None,
        }
    }
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for LanguageRuntimeValueRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::LanguageRuntimeValue { operation } = envelope.command else {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "language runtime executor requires a language_runtime_value command",
            ));
        };
        let value = match operation.as_str() {
            "now" => serde_json::json!(self.clock.timestamp_ms()),
            "random" => {
                // UUID v4 fixes high-order version/variant bits. The low 53 bits
                // remain random and map exactly onto JavaScript's unit interval.
                let bits = (uuid::Uuid::new_v4().as_u128() & ((1_u128 << 53) - 1)) as u64;
                serde_json::json!(bits as f64 / ((1_u64 << 53) as f64))
            }
            _ => {
                return Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                    format!("unknown language runtime operation `{operation}`"),
                ));
            }
        };
        Ok(RuntimeEffectOutcome::LanguageRuntimeValue { value })
    }
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for RunSealRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        match envelope.command {
            RuntimeEffectCommand::LanguageRuntimeValue { operation }
                if operation
                    .strip_prefix(RUN_SEAL_OPERATION)
                    .is_some_and(|facts| facts.starts_with(':')) =>
            {
                Ok(RuntimeEffectOutcome::LanguageRuntimeValue { value: self.value })
            }
            _ => Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "a run-seal executor requires a run-seal language_runtime_value command",
            )),
        }
    }
}
