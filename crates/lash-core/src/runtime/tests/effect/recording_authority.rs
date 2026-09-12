use super::*;

#[async_trait::async_trait]
impl crate::EffectHost for RecordingEffectController {
    fn turn_control_binding_id(&self) -> String {
        self.await_event_authority_binding_id()
            .expect("recorder authority")
    }

    fn await_event_resolver(&self) -> &dyn crate::AwaitEventResolver {
        self
    }

    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        ScopedEffectController::borrowed(self, scope)
    }
}

pub(in crate::runtime::tests) fn host_with_effect_recorder(
    recorder: RecordingEffectController,
) -> EmbeddedRuntimeHost {
    let mut config = if recorder.controller_owned_replay {
        let mut config = test_runtime_host_config();
        config.control.effect_host = Arc::new(recorder);
        config
    } else {
        runtime_host_config_with_native_controller(Arc::new(recorder))
    };
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        mock_provider(Vec::new()).into_handle(),
    ));
    EmbeddedRuntimeHost::new(config)
}

pub(in crate::runtime::tests) fn runtime_host_config_with_native_controller(
    controller: Arc<dyn RuntimeEffectController>,
) -> RuntimeHostConfig {
    let mut config = test_runtime_host_config();
    config.control.effect_host = Arc::new(NativeEffectHost::new(controller));
    config
}
