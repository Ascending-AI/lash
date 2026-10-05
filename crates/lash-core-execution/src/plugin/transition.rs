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
    pub base: PluginTransitionBase,
    pub target: PluginAdmission,
}

/// The retained session head or captured process segment this transition admits.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginTransitionBase {
    /// The command run's first recorded effect selects and retains its head.
    SessionCommand {
        run: crate::TurnId,
    },
    Session {
        head: crate::store::SessionHeadRef,
    },
    Process {
        environment: crate::ProcessExecutionEnvRef,
        segment: crate::ExecutionScope,
    },
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
    /// The executor bound from the converted candidate by the recorded transition.
    pub generation: Option<crate::ExecutableGeneration>,
    pub publication: Option<Box<crate::store::RuntimeCommit>>,
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
        let native_config = preflight.clone().and_then(|()| self.decode_config(config));
        let mut namespaces: BTreeMap<String, Result<PluginNamespaceState, PluginError>> = state
            .plugins
            .iter()
            .map(|(id, namespace)| {
                let result = match &native_config {
                    Err(refusal) => Err(PluginError::from(refusal.clone())),
                    Ok(_) => self.decode_namespace(id, namespace),
                };
                (id.clone(), result)
            })
            .collect();
        for factory in self.factories() {
            if !namespaces.contains_key(factory.id()) {
                let result = match &native_config {
                    Err(refusal) => Err(PluginError::from(refusal.clone())),
                    Ok(config) => {
                        factory
                            .initialize_state(&request.owner, config)
                            .and_then(|values| {
                                super::state::validate_namespace(&values)?;
                                Ok(PluginNamespaceState {
                                    format_version: factory.declaration().format_version,
                                    generation: 0,
                                    publication: Default::default(),
                                    values,
                                })
                            })
                    }
                };
                namespaces.insert(factory.id().into(), result);
            }
        }
        PluginTransitionRecord {
            request,
            source: super::state::state_ref(state),
            namespaces,
            config: native_config,
            generation: None,
            publication: None,
        }
    }
}

impl PluginSession {
    /// Bind the recorded native candidate privately, preserving this session's
    /// factory context. Publication installs the candidate on the resident session.
    pub fn materialize_transition_candidate(
        self: &std::sync::Arc<Self>,
        record: &PluginTransitionRecord,
    ) -> Result<std::sync::Arc<Self>, PluginError> {
        let (state, config) = record.candidate()?;
        if self.is_materialized() {
            return Ok(std::sync::Arc::clone(self));
        }
        let authority = self.live_authority();
        let mut plugin_config = authority.plugin_config;
        plugin_config.config = std::sync::Arc::new(config);
        let config = SessionAuthorityContext {
            tool_access: authority.tool_access,
            subagent: authority.subagent,
            plugin_config,
        };
        let materialization = match self.materialization {
            PluginSessionMaterialization::Creation => {
                PluginSessionMaterializationRequest::Creation {
                    config,
                    seed_snapshot: Some(&state),
                }
            }
            PluginSessionMaterialization::Rematerialization => {
                PluginSessionMaterializationRequest::Rematerialization {
                    snapshot: &state,
                    config,
                }
            }
        };
        let candidate = self
            .host
            .isolated_registry()
            .defer_session(PluginSessionRequest {
                owner: self.owner.clone(),
                parent_session_id: self.parent_session_id.clone(),
                materialization,
                tool_catalog_overlay: self.tool_catalog_overlay.clone(),
                tool_snapshot: self.tool_snapshot.clone(),
            })?;
        candidate.adopt_plugin_transition(record)?;
        candidate.materialize()?;
        Ok(candidate)
    }

    /// Install a journaled complete result without calling a converter.
    pub fn adopt_plugin_transition(
        &self,
        record: &PluginTransitionRecord,
    ) -> Result<(), PluginError> {
        if record.request.owner != self.owner {
            return Err(PluginStateError::EffectOwnerMismatch.into());
        }
        let (candidate, config) = record.candidate()?;
        *self.native_view.lock_recover() = Some(PluginNativeView {
            request: record.request.clone(),
            source: record.source.clone(),
            generation: record.generation.clone(),
            state: candidate.clone(),
            config: config.clone(),
        });
        let mut live = self.state.lock_recover();
        live.hydrate_live(&candidate);
        live.source = Some(record.source.clone());
        drop(live);
        self.adopt_plugin_admission(record.request.target.clone());
        self.authority.write_recover().plugin_config.config = Arc::new(config);
        Ok(())
    }
}

/// The successful transition and current native values carried by its checkpoint.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginNativeView {
    pub request: PluginTransitionRequest,
    pub source: crate::BlobRef,
    pub generation: Option<crate::ExecutableGeneration>,
    pub state: PluginState,
    pub config: PluginConfig,
}

/// Version of the opaque plugin admission checkpoint body (ADR 0078 §6).
/// version_surface = "migrate"
/// format_manifest = "PluginAdmissionCheckpoint"
/// version_guard(roots(PluginAdmissionCheckpoint))
pub const PLUGIN_ADMISSION_CHECKPOINT_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginAdmissionCheckpoint {
    format: u32,
    view: PluginNativeView,
}

impl PluginNativeView {
    pub fn encode(&self, fleet: crate::FleetFormat) -> Result<Arc<[u8]>, PluginError> {
        rmp_serde::to_vec_named(&PluginAdmissionCheckpoint {
            format: fleet.writer_version(lash_core_store::surface_format!(
                PLUGIN_ADMISSION_CHECKPOINT_VERSION
            )),
            view: self.clone(),
        })
        .map(Arc::from)
        .map_err(|error| PluginError::StoredDataCorrupt {
            record_kind: crate::store::PLUGIN_ADMISSION_CHECKPOINT_COMPONENT.into(),
            message: error.to_string(),
        })
    }
    pub fn decode(bytes: &[u8], fleet: crate::FleetFormat) -> Result<Self, PluginError> {
        #[derive(serde::Deserialize)]
        struct Stamp {
            format: u32,
        }
        let corrupt = |error: rmp_serde::decode::Error| PluginError::StoredDataCorrupt {
            record_kind: crate::store::PLUGIN_ADMISSION_CHECKPOINT_COMPONENT.into(),
            message: error.to_string(),
        };
        let stamp: Stamp = rmp_serde::from_slice(bytes).map_err(corrupt)?;
        if !fleet
            .read_window(lash_core_store::surface_format!(
                PLUGIN_ADMISSION_CHECKPOINT_VERSION
            ))
            .admits(stamp.format)
        {
            return Err(crate::StoreError::Incompatible {
                refusal: crate::compat::CompatRefusal::UnknownVocabulary {
                    surface: "plugin admission checkpoint format".into(),
                    label: stamp.format.to_string(),
                },
            }
            .into());
        }
        let record: PluginAdmissionCheckpoint = rmp_serde::from_slice(bytes).map_err(corrupt)?;
        Ok(record.view)
    }
}

impl PluginSession {
    pub fn adopt_native_view(
        &self,
        bytes: &[u8],
        fleet: crate::FleetFormat,
    ) -> Result<(), PluginError> {
        let view = PluginNativeView::decode(bytes, fleet)?;
        if view.request.owner != self.owner {
            return Err(PluginStateError::EffectOwnerMismatch.into());
        }
        for admitted in view.request.target.plugins() {
            if !view.state.plugins.contains_key(&admitted.plugin) {
                return Err(PluginError::StoredDataCorrupt {
                    record_kind: crate::store::PLUGIN_ADMISSION_CHECKPOINT_COMPONENT.into(),
                    message: format!(
                        "admitted plugin `{}` has no recorded namespace",
                        admitted.plugin
                    ),
                });
            }
        }
        self.host
            .validate_native_formats(&view.state, &view.config)?;
        self.state.lock_recover().hydrate_live(&view.state);
        self.adopt_plugin_admission(view.request.target.clone());
        self.authority.write_recover().plugin_config.config = Arc::new(view.config.clone());
        *self.native_view.lock_recover() = Some(view);
        Ok(())
    }

    pub fn native_view(&self, fleet: crate::FleetFormat) -> Result<Option<Arc<[u8]>>, PluginError> {
        if !self.host.export_plugin_namespaces {
            return Ok(None);
        }
        self.capture_native_view(None, fleet)
    }

    pub(super) fn capture_native_view(
        &self,
        config: Option<&PluginConfig>,
        fleet: crate::FleetFormat,
    ) -> Result<Option<Arc<[u8]>>, PluginError> {
        let Some(mut view) = self.native_view.lock_recover().clone() else {
            return Ok(None);
        };
        view.state = self.capture_state();
        if let Some(config) = config {
            if self
                .host
                .validate_native_formats(&view.state, config)
                .is_ok()
            {
                view.config = config.clone();
            } else {
                let writers = view.request.target.writers();
                if self.host.encode_config(&view.config, &writers)? != *config {
                    self.host.validate_native_formats(&view.state, config)?;
                }
            }
        }
        view.encode(fleet).map(Some)
    }
}

#[cfg(test)]
mod nested_format_tests {
    #[test]
    fn admission_refuses_a_foreign_format_before_reading_the_view() {
        let bytes =
            rmp_serde::to_vec_named(&serde_json::json!({"format": u32::MAX})).expect("stamp");
        let error = super::PluginNativeView::decode(&bytes, crate::FleetFormat::current())
            .expect_err("foreign format");
        assert!(error.to_string().contains("4294967295"), "{error}");
        assert!(
            !error.to_string().contains("missing field"),
            "the stamp refuses before decoding: {error}"
        );
    }
}
