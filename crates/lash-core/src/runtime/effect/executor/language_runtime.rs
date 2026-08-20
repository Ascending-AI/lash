use super::*;

pub(super) struct LanguageRuntimeValueRunner {
    pub(super) clock: Arc<dyn crate::Clock>,
}

impl RuntimeEffectLocalExecutor<'_> {
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
