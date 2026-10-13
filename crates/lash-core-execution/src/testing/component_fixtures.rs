//! Process component fixtures for the fleet writer-range laws (FIG-6061):
//! a component that writes config format 2 natively, and an environment
//! naming it.

/// The components of a fleet writer-range law: one component `id`, config
/// `()`, that reads and writes config formats 1 and 2 and writes 2
/// natively, and contributes nothing.
pub fn native_format_two_component(id: &'static str) -> crate::plugin::ProcessComponentRegistry {
    struct FormatTwo(crate::plugin::ProcessComponentDeclaration);

    impl crate::plugin::ProcessComponentFactory for FormatTwo {
        fn declaration(&self) -> crate::plugin::ProcessComponentDeclaration {
            self.0.clone()
        }

        fn config_type(&self) -> std::any::TypeId {
            std::any::TypeId::of::<()>()
        }

        fn validate(
            &self,
            _: &crate::ProcessComponentEntry,
            _: &crate::ProcessEnv,
        ) -> Result<(), crate::ProcessEnvRefusal> {
            Ok(())
        }

        fn recipe(
            self: std::sync::Arc<Self>,
        ) -> Result<crate::plugin::ProcessRecipe, crate::PluginError> {
            crate::plugin::ProcessComponentSpec::new().recipe(self.declaration().id.as_str())
        }
    }

    let two = crate::FormatVersion::new(2).expect("format 2");
    let mut declaration =
        crate::plugin::ProcessComponentDeclaration::initial(id, two).expect("a component id");
    declaration.readable_config_formats = vec![crate::FormatVersion::ONE, two];
    declaration.writable_config_formats = vec![crate::FormatVersion::ONE, two];
    let mut registry = crate::plugin::ProcessComponentRegistry::new();
    registry
        .register(
            std::sync::Arc::new(FormatTwo(declaration)),
            crate::ProcessConfigKey::<()>::new(id, two),
        )
        .expect("the component registers");
    registry
}

/// An environment of no tools naming component `id` at config `format`.
pub fn component_env(id: &'static str, format: crate::FormatVersion) -> crate::ProcessEnv {
    crate::ProcessEnv::builder(crate::MaxToolCalls::new(16))
        .component(crate::ProcessConfigKey::<()>::new(id, format), &())
        .build()
        .expect("the environment builds")
}
