//! Complete namespace conversion results at an engine admission boundary.
use lash_sansio::sync::{MutexExt, RwLockExt};
use std::collections::BTreeMap;

use super::*;
use crate::store::plugin_writers::PluginAdmission;

/// A transition uses the engine's existing effect address as its identity.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PluginTransitionId(pub crate::EffectAddress);

/// The recorded base and target of one complete plugin-set transition.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginTransitionRequest {
    pub id: PluginTransitionId,
    pub owner: crate::RuntimeOwner,
    pub base: crate::store::SessionHeadRef,
    pub target: PluginAdmission,
}

/// Each namespace retains either its complete postimage or its typed refusal.
/// No namespace is installed when any state or config conversion refuses.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginTransitionRecord {
    pub request: PluginTransitionRequest,
    pub source: crate::BlobRef,
    pub namespaces: BTreeMap<String, Result<PluginNamespaceState, PluginError>>,
    pub config: Result<PluginConfig, FormatRefusal>,
}

impl PluginTransitionRecord {
    /// Validate the complete candidate before making any part available.
    pub fn candidate(&self) -> Result<(PluginState, PluginConfig), PluginError> {
        let config = self.config.clone()?;
        let plugins = self
            .namespaces
            .iter()
            .map(|(id, result)| result.clone().map(|namespace| (id.clone(), namespace)))
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        Ok((PluginState { plugins }, config))
    }
}

impl PluginHost {
    /// Pure conversion, called only by the transition's recorded effect body.
    /// Inactive namespaces remain byte-for-byte equivalent values.
    pub fn transition_plugins(
        &self,
        request: PluginTransitionRequest,
        state: &PluginState,
        config: &PluginConfig,
    ) -> PluginTransitionRecord {
        let preflight = self
            .validate_state_formats(state)
            .and_then(|()| self.validate_config_formats(config));
        let namespaces = state
            .plugins
            .iter()
            .map(|(id, namespace)| {
                let result = match &preflight {
                    Err(refusal) => Err(PluginError::from(refusal.clone())),
                    Ok(()) => self.decode_namespace(id, namespace),
                };
                (id.clone(), result)
            })
            .collect();
        PluginTransitionRecord {
            request,
            source: super::state::state_ref(state),
            namespaces,
            config: preflight.and_then(|()| self.decode_config(config)),
        }
    }
}

impl PluginSession {
    /// Install a journaled complete result without calling a converter.
    pub fn adopt_plugin_transition(
        &self,
        record: &PluginTransitionRecord,
    ) -> Result<(), PluginError> {
        if record.request.owner != self.owner {
            return Err(PluginStateError::EffectOwnerMismatch.into());
        }
        let (candidate, config) = record.candidate()?;
        let mut live = self.state.lock_recover();
        live.hydrate_live(&candidate);
        live.source = Some(record.source.clone());
        drop(live);
        self.adopt_plugin_admission(record.request.target.clone());
        self.authority.write_recover().plugin_config.config = Arc::new(config);
        Ok(())
    }
}
