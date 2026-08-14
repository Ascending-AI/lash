use super::*;

pub(super) struct LanguageRuntimeValueRunner {
    pub(super) clock: Arc<dyn crate::Clock>,
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
                let bits = (uuid::Uuid::new_v4().as_u128() >> (128 - 53)) as u64;
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
