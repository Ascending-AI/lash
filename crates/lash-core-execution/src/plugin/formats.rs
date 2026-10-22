use std::collections::BTreeMap;

use super::FormatNamespace;
use super::FormatRefusal;
use super::FormatVersion;
use super::PluginConfig;
use super::PluginHost;
use super::PluginState;
use lash_core_store::plugin_state::ConfigAdmissionError;
use lash_execution_contracts::plugin_error::PluginError;

impl PluginHost {
    fn check_stamp(
        &self,
        id: &str,
        namespace: FormatNamespace,
        stored: FormatVersion,
    ) -> Result<(), FormatRefusal> {
        if let Some(factory) = self.factories().iter().find(|factory| factory.id() == id) {
            let readable = factory.plugin_declaration().format_version;
            if stored > readable {
                return Err(FormatRefusal {
                    plugin: id.into(),
                    namespace,
                    stored,
                    readable,
                });
            }
        }
        Ok(())
    }

    pub fn validate_state_formats(&self, state: &PluginState) -> Result<(), FormatRefusal> {
        for (id, namespace) in &state.plugins {
            self.check_stamp(id, FormatNamespace::State, namespace.format_version)?;
        }
        Ok(())
    }

    pub fn validate_config_formats(&self, config: &PluginConfig) -> Result<(), FormatRefusal> {
        for (id, namespace) in config.namespaces() {
            self.check_stamp(id, FormatNamespace::Config, namespace.format_version)?;
        }
        Ok(())
    }

    pub(super) fn validate_native_formats(
        &self,
        state: &PluginState,
        config: &PluginConfig,
    ) -> Result<(), FormatRefusal> {
        for factory in self.factories() {
            let native = factory.plugin_declaration().format_version;
            for (namespace, stored) in [
                (
                    FormatNamespace::State,
                    state.plugins.get(factory.id()).map(|ns| ns.format_version),
                ),
                (
                    FormatNamespace::Config,
                    config.namespace(factory.id()).map(|ns| ns.format_version),
                ),
            ] {
                if let Some(stored) = stored
                    && stored != native
                {
                    return Err(FormatRefusal {
                        plugin: factory.id().into(),
                        namespace,
                        stored,
                        readable: native,
                    });
                }
            }
        }
        Ok(())
    }

    /// Decode every active namespace before materialization. Inactive namespaces
    /// retain their original format and values.
    pub fn decode_state(&self, state: &PluginState) -> Result<PluginState, PluginError> {
        self.validate_state_formats(state)?;
        let mut decoded = PluginState::empty_in(state.allocation_budget())?;
        for (id, namespace) in &state.plugins {
            decoded
                .plugins
                .insert_ref(id, self.decode_namespace(id, namespace)?)?;
        }
        Ok(decoded)
    }

    pub(super) fn decode_namespace(
        &self,
        id: &str,
        original: &super::PluginNamespaceState,
    ) -> Result<super::PluginNamespaceState, PluginError> {
        let mut namespace = original.clone();
        let Some(factory) = self.factories().iter().find(|factory| factory.id() == id) else {
            return Ok(namespace);
        };
        decode_namespace_for(
            id,
            &mut namespace,
            factory.plugin_declaration().format_version,
            self.data_limits().plugin_state,
            |from, value| factory.migrate_format(from, FormatNamespace::State, value),
        )?;
        Ok(namespace)
    }

    pub fn decode_config(
        &self,
        config: &PluginConfig,
    ) -> Result<PluginConfig, ConfigAdmissionError> {
        decode_config_for(self.factories(), config)
    }

    /// Encode with the caller's recorded writer choice, never a live fleet read.
    pub fn encode_state(
        &self,
        state: &PluginState,
        writers: &BTreeMap<String, FormatVersion>,
    ) -> Result<PluginState, PluginError> {
        let mut encoded = self.decode_state(state)?;
        for factory in self.factories() {
            let Some(mut namespace) = encoded.plugins.get(factory.id()).cloned() else {
                continue;
            };
            let to = writers
                .get(factory.id())
                .copied()
                .ok_or_else(|| FormatRefusal {
                    plugin: factory.id().into(),
                    namespace: FormatNamespace::State,
                    stored: namespace.format_version,
                    readable: factory.plugin_declaration().format_version,
                })?;
            if to == namespace.format_version {
                continue;
            }
            let value = serde_json::to_value(&namespace.values).map_err(|error| {
                PluginError::StoredDataCorrupt {
                    record_kind: "plugin_state".into(),
                    message: error.to_string(),
                }
            })?;
            let value = factory.encode_format(to, FormatNamespace::State, &value)?;
            namespace.replace_values(serde_json::from_value(value).map_err(|error| {
                PluginError::StoredDataCorrupt {
                    record_kind: "plugin_state".into(),
                    message: error.to_string(),
                }
            })?)?;
            super::state::validate_namespace(&namespace.values, self.data_limits().plugin_state)?;
            namespace.format_version = to;
            encoded.plugins.insert(factory.id().to_owned(), namespace)?;
        }
        Ok(encoded)
    }

    pub fn encode_config(
        &self,
        config: &PluginConfig,
        writers: &BTreeMap<String, FormatVersion>,
    ) -> Result<PluginConfig, ConfigAdmissionError> {
        let decoded = self.decode_config(config)?;
        let mut encoded = decoded.clone();
        for factory in self.factories() {
            let Some(namespace) = decoded.namespace(factory.id()) else {
                continue;
            };
            let to = writers
                .get(factory.id())
                .copied()
                .ok_or_else(|| FormatRefusal {
                    plugin: factory.id().into(),
                    namespace: FormatNamespace::Config,
                    stored: namespace.format_version,
                    readable: factory.plugin_declaration().format_version,
                })?;
            if to == namespace.format_version {
                continue;
            }
            encoded.insert_versioned(
                factory.id(),
                to,
                factory.encode_format(to, FormatNamespace::Config, &namespace.value)?,
            )?;
        }
        Ok(encoded)
    }
}

pub(super) fn decode_config_for(
    factories: &[std::sync::Arc<dyn super::PluginFactory>],
    config: &PluginConfig,
) -> Result<PluginConfig, ConfigAdmissionError> {
    for factory in factories {
        if let Some(namespace) = config.namespace(factory.id())
            && namespace.format_version > factory.plugin_declaration().format_version
        {
            return Err(FormatRefusal {
                plugin: factory.id().into(),
                namespace: FormatNamespace::Config,
                stored: namespace.format_version,
                readable: factory.plugin_declaration().format_version,
            }
            .into());
        }
    }
    let mut decoded = config.clone();
    for factory in factories {
        if let Some(namespace) = config.namespace(factory.id())
            && namespace.format_version != factory.plugin_declaration().format_version
        {
            let value = factory.migrate_format(
                namespace.format_version,
                FormatNamespace::Config,
                namespace.value.as_ref().clone(),
            )?;
            decoded.insert_versioned(
                factory.id(),
                factory.plugin_declaration().format_version,
                value,
            )?;
        }
    }
    Ok(decoded)
}

/// The whole-view conversion shared by session plugins and process components.
fn decode_namespace_for(
    id: &str,
    namespace: &mut super::PluginNamespaceState,
    native: FormatVersion,
    limits: crate::tool_run::StateCommandLimits,
    migrate: impl FnOnce(FormatVersion, serde_json::Value) -> Result<serde_json::Value, FormatRefusal>,
) -> Result<(), PluginError> {
    super::state::validate_namespace(&namespace.values, limits)?;
    if namespace.format_version == native {
        return Ok(());
    }
    let value = serde_json::to_value(&namespace.values).map_err(|error| {
        PluginError::StoredDataCorrupt {
            record_kind: "plugin_state".into(),
            message: error.to_string(),
        }
    })?;
    let value = migrate(namespace.format_version, value)?;
    let values = serde_json::from_value(value).map_err(|error| PluginError::StoredDataCorrupt {
        record_kind: "plugin_state".into(),
        message: error.to_string(),
    })?;
    super::state::validate_namespace(&values, limits)?;
    namespace.replace_values(values.into())?;
    namespace.format_version = native;
    namespace.generation = namespace.generation.checked_add(1).ok_or_else(|| {
        PluginError::MonotonicCounterOverflow {
            counter: format!("plugin {id} migration generation"),
            current: namespace.generation,
        }
    })?;
    Ok(())
}

impl super::ProcessComponentRegistry {
    /// Convert the complete state and config view, for the owner to record
    /// before binding capabilities. Original publication receipts survive.
    #[doc(hidden)]
    pub fn decode_components(
        &self,
        env: &crate::ProcessEnv,
        state: &PluginState,
        limits: crate::tool_run::StateCommandLimits,
    ) -> Result<(crate::ProcessEnv, PluginState), PluginError> {
        let mut decoded = state.clone();
        let mut entries = Vec::with_capacity(env.components().len());
        let retired = self.retired_in(env);
        for entry in env.components() {
            // A retired component converts nothing: its entry and state stay
            // as recorded, and the build judges what the run left on it.
            if retired.contains(entry.id()) {
                entries.push(entry.clone());
                continue;
            }
            let registered = self.components.get(entry.id().as_str()).ok_or_else(|| {
                PluginError::ProcessEnvRefused {
                    refusal: crate::ProcessEnvRefusal::ComponentNotInstalled {
                        id: entry.id().to_string(),
                    },
                }
            })?;
            let declaration = &registered.declaration;
            let native = declaration.format_version;
            let mut namespace = match decoded.plugins.get(entry.id().as_str()) {
                Some(namespace) => namespace.clone(),
                None => {
                    let mut namespace =
                        super::PluginNamespaceState::empty_in(decoded.allocation_budget())?;
                    namespace.format_version = entry.config().format_version();
                    namespace
                }
            };
            for (kind, stored) in [
                (FormatNamespace::State, namespace.format_version),
                (FormatNamespace::Config, entry.config().format_version()),
            ] {
                if !declaration.readable_formats.contains(&stored) {
                    return Err(FormatRefusal {
                        plugin: entry.id().to_string(),
                        namespace: kind,
                        stored,
                        readable: native,
                    }
                    .into());
                }
            }
            decode_namespace_for(
                entry.id().as_str(),
                &mut namespace,
                native,
                limits,
                |from, value| {
                    registered
                        .factory
                        .migrate_format(from, FormatNamespace::State, value)
                },
            )?;
            decoded.plugins.insert(entry.id().to_string(), namespace)?;
            let config = if entry.config().format_version() == native {
                entry.config().value().clone()
            } else {
                registered.factory.migrate_format(
                    entry.config().format_version(),
                    FormatNamespace::Config,
                    entry.config().value().clone(),
                )?
            };
            entries.push(crate::ProcessComponentEntry::from_parts(
                entry.id().clone(),
                crate::ProcessComponentConfig::new(native, config),
                entry.sessions().clone(),
            ));
        }
        Ok((env.clone().with_component_entries(entries), decoded))
    }
}
