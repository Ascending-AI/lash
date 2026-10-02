use std::collections::BTreeMap;

use super::{
    FormatNamespace, FormatRefusal, FormatVersion, PluginConfig, PluginError, PluginHost,
    PluginState,
};

impl PluginHost {
    fn check_stamp(
        &self,
        id: &str,
        namespace: FormatNamespace,
        stored: FormatVersion,
    ) -> Result<(), FormatRefusal> {
        if let Some(factory) = self.factories().iter().find(|factory| factory.id() == id) {
            let readable = factory.declaration().format_version;
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

    /// Decode every active namespace before materialization. Inactive namespaces
    /// retain their original format and values.
    pub fn decode_state(&self, state: &PluginState) -> Result<PluginState, PluginError> {
        self.validate_state_formats(state)?;
        let mut decoded = state.clone();
        for factory in self.factories() {
            let Some(namespace) = decoded.plugins.get_mut(factory.id()) else {
                continue;
            };
            let native = factory.declaration().format_version;
            if namespace.format_version == native {
                continue;
            }
            let value = serde_json::to_value(&namespace.values).map_err(|error| {
                PluginError::StoredDataCorrupt {
                    record_kind: "plugin_state".into(),
                    message: error.to_string(),
                }
            })?;
            let value =
                factory.migrate_format(namespace.format_version, FormatNamespace::State, value)?;
            let values =
                serde_json::from_value(value).map_err(|error| PluginError::StoredDataCorrupt {
                    record_kind: "plugin_state".into(),
                    message: error.to_string(),
                })?;
            super::state::validate_namespace(&values)?;
            namespace.values = values;
            namespace.format_version = native;
            // Representation changes participate in the same capture identity as
            // mediated writes, even when no ordinary callback changes a value.
            namespace.generation = namespace.generation.checked_add(1).ok_or_else(|| {
                PluginError::MonotonicCounterOverflow {
                    counter: format!("plugin {} migration generation", factory.id()),
                    current: namespace.generation,
                }
            })?;
        }
        Ok(decoded)
    }

    pub fn decode_config(&self, config: &PluginConfig) -> Result<PluginConfig, FormatRefusal> {
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
            let Some(namespace) = encoded.plugins.get_mut(factory.id()) else {
                continue;
            };
            let to = writers
                .get(factory.id())
                .copied()
                .ok_or_else(|| FormatRefusal {
                    plugin: factory.id().into(),
                    namespace: FormatNamespace::State,
                    stored: namespace.format_version,
                    readable: factory.declaration().format_version,
                })?;
            let value = serde_json::to_value(&namespace.values).map_err(|error| {
                PluginError::StoredDataCorrupt {
                    record_kind: "plugin_state".into(),
                    message: error.to_string(),
                }
            })?;
            let value = factory.encode_format(to, FormatNamespace::State, &value)?;
            namespace.values =
                serde_json::from_value(value).map_err(|error| PluginError::StoredDataCorrupt {
                    record_kind: "plugin_state".into(),
                    message: error.to_string(),
                })?;
            super::state::validate_namespace(&namespace.values)?;
            namespace.format_version = to;
        }
        Ok(encoded)
    }

    pub fn encode_config(
        &self,
        config: &PluginConfig,
        writers: &BTreeMap<String, FormatVersion>,
    ) -> Result<PluginConfig, FormatRefusal> {
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
                    readable: factory.declaration().format_version,
                })?;
            encoded.insert_versioned(
                factory.id(),
                to,
                factory.encode_format(to, FormatNamespace::Config, &namespace.value)?,
            );
        }
        Ok(encoded)
    }
}

pub(super) fn decode_config_for(
    factories: &[std::sync::Arc<dyn super::PluginFactory>],
    config: &PluginConfig,
) -> Result<PluginConfig, FormatRefusal> {
    for factory in factories {
        if let Some(namespace) = config.namespace(factory.id())
            && namespace.format_version > factory.declaration().format_version
        {
            return Err(FormatRefusal {
                plugin: factory.id().into(),
                namespace: FormatNamespace::Config,
                stored: namespace.format_version,
                readable: factory.declaration().format_version,
            });
        }
    }
    let mut decoded = config.clone();
    for factory in factories {
        if let Some(namespace) = config.namespace(factory.id())
            && namespace.format_version != factory.declaration().format_version
        {
            let value = factory.migrate_format(
                namespace.format_version,
                FormatNamespace::Config,
                namespace.value.clone(),
            )?;
            decoded.insert_versioned(factory.id(), factory.declaration().format_version, value);
        }
    }
    Ok(decoded)
}
