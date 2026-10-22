//! Complete namespace conversion results at an engine admission boundary.
use lash_protocol_types::{sync::MutexExt, sync::RwLockExt};
use std::collections::BTreeMap;

use super::*;
use crate::store::plugin_writers::PluginAdmission;

/// A transition uses the engine's existing effect address as its identity.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PluginTransitionId(pub crate::EffectAddress);

/// The recorded base and target of one complete plugin-set transition.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PluginTransitionRequest(Arc<PluginTransitionRequestStorage>);
/// Immutable request fields, borrowed through a transition request.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginTransitionRequestData {
    pub id: PluginTransitionId,
    pub owner: crate::RuntimeOwner,
    pub base: PluginTransitionBase,
    pub target: PluginAdmission,
}
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
struct PluginTransitionRequestStorage {
    value: PluginTransitionRequestData,
    #[serde(skip)]
    allocation: lash_protocol_types::allocation::AllocationHold,
    #[serde(skip)]
    budget: lash_protocol_types::allocation::AllocationBudget,
}
impl std::ops::Deref for PluginTransitionRequest {
    type Target = PluginTransitionRequestData;
    fn deref(&self) -> &Self::Target {
        &self.0.value
    }
}
impl PluginTransitionRequest {
    /// Cold request input; serving owners adopt it with clone_in.
    pub fn new(value: PluginTransitionRequestData) -> Self {
        Self(Arc::new(PluginTransitionRequestStorage {
            value,
            allocation: Default::default(),
            budget: Default::default(),
        }))
    }
    /// Share admitted metadata or reserve before copying a foreign request.
    pub fn clone_in(
        &self,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<Self, lash_protocol_types::allocation::AllocationError> {
        if self.0.budget.identity() == budget.identity() {
            return Ok(self.clone());
        }
        let allocation = budget.reserve(lash_protocol_types::allocation::payload_capacity(
            &self.0.value,
            std::mem::size_of::<PluginTransitionRequestStorage>(),
        ))?;
        let target = self.target.clone_in(budget)?;
        Ok(Self(Arc::new(PluginTransitionRequestStorage {
            value: PluginTransitionRequestData {
                id: self.id.clone(),
                owner: self.owner.clone(),
                base: self.base.clone(),
                target,
            },
            allocation,
            budget: budget.clone(),
        })))
    }
    /// Transfer an existing whole-source decode reservation before publishing.
    pub fn decoded_in(
        &mut self,
        allocation: lash_protocol_types::allocation::AllocationHold,
        budget: lash_protocol_types::allocation::AllocationBudget,
    ) -> bool {
        let Some(source) = Arc::get_mut(&mut self.0) else {
            return false;
        };
        if !source
            .value
            .target
            .decoded_in(allocation.clone(), budget.clone())
        {
            return false;
        }
        source.allocation = allocation;
        source.budget = budget;
        true
    }
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
        environment: crate::ProcessEnvRef,
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
    pub config: Result<PluginConfig, lash_core_store::plugin_state::ConfigAdmissionError>,
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
        let budget = plugins
            .values()
            .next()
            .map(|namespace| namespace.values.allocation_budget())
            .cloned()
            .unwrap_or_default();
        let plugins = lash_core_store::source_map::SharedSourceMap::from_map_in(&plugins, &budget)?;
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
        if let Err(refusal) = self.composition() {
            let namespaces = state
                .plugins
                .keys()
                .map(String::as_str)
                .chain(self.factories().iter().map(|factory| factory.id()))
                .map(|id| {
                    (
                        id.to_owned(),
                        Err(PluginError::Declaration(refusal.clone())),
                    )
                })
                .collect();
            return PluginTransitionRecord {
                request,
                source: super::state::state_ref(state),
                namespaces,
                config: Ok(config.clone()),
                generation: None,
                publication: None,
            };
        }
        let preflight = self
            .validate_state_formats(state)
            .and_then(|()| self.validate_config_formats(config));
        let native_config = preflight
            .clone()
            .map_err(lash_core_store::plugin_state::ConfigAdmissionError::from)
            .and_then(|()| self.decode_config(config));
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
                                if !values.is_empty() && !self.keeps_state(factory.id()) {
                                    return Err(PluginError::Registration(format!(
                                        "stateless plugin `{}` initializes state",
                                        factory.id()
                                    )));
                                }
                                super::state::validate_namespace(
                                    &values,
                                    self.data_limits().plugin_state,
                                )?;
                                let declaration = factory.plugin_declaration();
                                Ok(PluginNamespaceState {
                                    format_version: declaration.format_version,
                                    generation: 0,
                                    publication: Default::default(),
                                    fork: declaration.state_fork,
                                    values: values.into(),
                                    encoding: Default::default(),
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
        let config = SessionAuthorityContext { plugin_config };
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
                allocation_budget: self.state.lock_recover().data.allocation_budget().clone(),
                owner: self.owner.clone(),
                materialization,
                tool_catalog_overlay: self.tool_catalog_overlay.clone(),
            })?;
        candidate.adopt_plugin_transition(record)?;
        candidate.materialize()?;
        Ok(candidate)
    }

    /// Install a journaled complete result without calling a converter, and
    /// keep it as the native view this session's checkpoints carry.
    pub fn adopt_plugin_transition(
        &self,
        record: &PluginTransitionRecord,
    ) -> Result<(), PluginError> {
        if record.request.owner != self.owner {
            return Err(PluginStateError::EffectOwnerMismatch.into());
        }
        let (state, config) = record.candidate()?;
        let native = RetainedPluginNativeView::capture_parts_in(
            &record.request,
            &record.source,
            &record.generation,
            &state,
            &config,
            config.allocation_budget(),
        )
        .map_err(|error| PluginError::from(crate::StoreError::Allocation(error)))?;
        self.install_plugin_transition(record)?;
        *self.native_view.lock_recover() = Some(native);
        Ok(())
    }

    /// Install a journaled complete result without calling a converter, and
    /// keep no native view of it. A store-backed session's head records its
    /// namespaces as their own components and its config in its authority,
    /// so a view would copy every namespace into every commit and could
    /// outlive a later commit's state or config.
    pub fn install_plugin_transition(
        &self,
        record: &PluginTransitionRecord,
    ) -> Result<(PluginState, PluginConfig), PluginError> {
        if record.request.owner != self.owner {
            return Err(PluginStateError::EffectOwnerMismatch.into());
        }
        let (candidate, config) = record.candidate()?;
        let admission = record
            .request
            .target
            .clone_in(config.allocation_budget())
            .map_err(|error| PluginError::from(crate::StoreError::Allocation(error)))?;
        let mut live = self.state.lock_recover();
        live.hydrate_live(&candidate);
        live.source = Some(record.source.clone());
        drop(live);
        *self.admission.lock_recover() = Some(admission);
        self.authority.write_recover().plugin_config.config = Arc::new(config.clone());
        Ok((candidate, config))
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

#[derive(Clone, Debug)]
pub(super) struct RetainedPluginNativeView(Arc<RetainedNativeSource>);
#[derive(Debug)]
struct RetainedNativeSource {
    view: PluginNativeView,
    _allocation: lash_protocol_types::allocation::AllocationHold,
}
impl RetainedPluginNativeView {
    pub(super) fn capture_in(
        view: &PluginNativeView,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<Self, lash_protocol_types::allocation::AllocationError> {
        Self::capture_parts_in(
            &view.request,
            &view.source,
            &view.generation,
            &view.state,
            &view.config,
            budget,
        )
    }
    fn capture_parts_in(
        request: &PluginTransitionRequest,
        source: &crate::BlobRef,
        generation: &Option<crate::ExecutableGeneration>,
        state: &PluginState,
        config: &PluginConfig,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<Self, lash_protocol_types::allocation::AllocationError> {
        #[derive(serde::Serialize)]
        struct Capture<'a> {
            request: &'a PluginTransitionRequest,
            source: &'a crate::BlobRef,
            generation: &'a Option<crate::ExecutableGeneration>,
            state: &'a PluginState,
            config: &'a PluginConfig,
        }
        let capacity = lash_protocol_types::allocation::payload_capacity(
            &Capture {
                request,
                source,
                generation,
                state,
                config,
            },
            std::mem::size_of::<RetainedNativeSource>(),
        );
        let allocation = budget.reserve(capacity)?;
        let request = request.clone_in(budget)?;
        let config = config.clone_in(budget)?;
        Ok(Self(Arc::new(RetainedNativeSource {
            view: PluginNativeView {
                request,
                source: source.clone(),
                generation: generation.clone(),
                state: state.clone(),
                config,
            },
            _allocation: allocation,
        })))
    }
    fn view(&self) -> &PluginNativeView {
        &self.0.view
    }
}

/// Version of the opaque plugin admission checkpoint body (ADR 0078 §6).
/// version_surface = "migrate"
/// format_manifest = "PluginAdmissionCheckpoint"
/// version_guard(roots(PluginAdmissionCheckpoint))
pub const PLUGIN_ADMISSION_CHECKPOINT_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginAdmissionCheckpoint<V> {
    format: u32,
    view: V,
}

impl PluginNativeView {
    pub fn encode(
        &self,
        fleet: crate::FleetFormat,
    ) -> Result<lash_kernel_doc::allocation::SharedBytes, PluginError> {
        let retained = RetainedPluginNativeView::capture_in(self, self.config.allocation_budget())
            .map_err(|error| PluginError::from(crate::StoreError::Allocation(error)))?;
        retained.view().encode_admitted(fleet)
    }
    fn encode_admitted(
        &self,
        fleet: crate::FleetFormat,
    ) -> Result<lash_kernel_doc::allocation::SharedBytes, PluginError> {
        let body = PluginAdmissionCheckpoint {
            format: fleet.writer_version(lash_core_store::surface_format!(
                PLUGIN_ADMISSION_CHECKPOINT_VERSION
            )),
            view: self,
        };
        lash_core_store::store::encode_checkpoint_component(
            crate::store::PLUGIN_ADMISSION_CHECKPOINT_COMPONENT,
            &body,
            self.config.allocation_budget(),
        )
        .map_err(PluginError::from)
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
        let record: PluginAdmissionCheckpoint<PluginNativeView> =
            rmp_serde::from_slice(bytes).map_err(corrupt)?;
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
        let budget = self
            .authority
            .read_recover()
            .plugin_config
            .config
            .allocation_budget()
            .clone();
        let retained = RetainedPluginNativeView::capture_in(&view, &budget)
            .map_err(|error| PluginError::from(crate::StoreError::Allocation(error)))?;
        let view = retained.view();
        self.state.lock_recover().hydrate_live(&view.state);
        *self.admission.lock_recover() = Some(view.request.target.clone());
        self.authority.write_recover().plugin_config.config = Arc::new(view.config.clone());
        *self.native_view.lock_recover() = Some(retained);
        Ok(())
    }

    /// A private build of the published plugin view `bytes` records, for a
    /// query on a session whose head records one and whose own plugins no
    /// run built here (FIG-5344). It registers on no host, installs nothing
    /// and publishes nothing: this session stays unbuilt.
    pub fn build_published_view(
        &self,
        bytes: &[u8],
        fleet: crate::FleetFormat,
    ) -> Result<Arc<Self>, PluginError> {
        let view = PluginNativeView::decode(bytes, fleet)?;
        let built = self.defer_private_build(&view.state, Some(view.config.clone()))?;
        built.adopt_native_view(bytes, fleet)?;
        built.materialize()?;
        Ok(built)
    }

    /// A private build of the namespaces and config this unbuilt session
    /// hydrated from its store-backed head, for a query on a session whose
    /// last run or command published them there (FIG-5344). Like
    /// [`Self::build_published_view`], it registers, installs and publishes
    /// nothing.
    pub fn build_recorded_head(&self) -> Result<Arc<Self>, PluginError> {
        self.require_runtime_owner()?;
        let built = self.defer_private_build(&self.capture_state(), None)?;
        built.materialize()?;
        Ok(built)
    }

    fn defer_private_build(
        &self,
        state: &PluginState,
        config: Option<PluginConfig>,
    ) -> Result<Arc<Self>, PluginError> {
        let authority = self.live_authority();
        let mut plugin_config = authority.plugin_config;
        if let Some(config) = config {
            plugin_config.config = Arc::new(config);
        }
        let built = self
            .host
            .isolated_registry()
            .defer_session(PluginSessionRequest {
                allocation_budget: self.state.lock_recover().data.allocation_budget().clone(),
                owner: self.owner.clone(),
                materialization: PluginSessionMaterializationRequest::Rematerialization {
                    snapshot: state,
                    config: SessionAuthorityContext { plugin_config },
                },
                tool_catalog_overlay: self.tool_catalog_overlay.clone(),
            })?;
        built.bind_services(self.service_bindings())?;
        Ok(built)
    }

    pub fn native_view(
        &self,
        fleet: crate::FleetFormat,
    ) -> Result<Option<lash_kernel_doc::allocation::SharedBytes>, PluginError> {
        if !self.host.export_plugin_namespaces {
            return Ok(None);
        }
        self.capture_native_view(None, fleet)
    }

    pub(super) fn capture_native_view(
        &self,
        config: Option<&PluginConfig>,
        fleet: crate::FleetFormat,
    ) -> Result<Option<lash_kernel_doc::allocation::SharedBytes>, PluginError> {
        self.require_runtime_owner()?;
        let Some(retained) = self.native_view.lock_recover().clone() else {
            return Ok(None);
        };
        let view = retained.view();
        let state = self.capture_state();
        let config = match config {
            Some(config) => {
                if self.host.validate_native_formats(&state, config).is_err() {
                    let admission = view
                        .request
                        .target
                        .clone_in(config.allocation_budget())
                        .map_err(|error| PluginError::from(crate::StoreError::Allocation(error)))?;
                    let writers = admission
                        .writers()
                        .map_err(|error| PluginError::from(crate::StoreError::Allocation(error)))?;
                    if self.host.encode_config(&view.config, &writers)? != *config {
                        self.host.validate_native_formats(&state, config)?;
                    }
                }
                config
            }
            None => &view.config,
        };
        let captured = RetainedPluginNativeView::capture_parts_in(
            &view.request,
            &view.source,
            &view.generation,
            &state,
            config,
            config.allocation_budget(),
        )
        .map_err(|error| PluginError::from(crate::StoreError::Allocation(error)))?;
        captured.view().encode_admitted(fleet).map(Some)
    }
}

#[cfg(test)]
mod nested_format_tests {
    /// LO-EVICT-NATIVE-METADATA: capture admits request/composition metadata
    /// before copying, and readers keep that source's reservation.
    #[test]
    fn native_view_capture_preadmits_request_and_admission_metadata() {
        use super::*;
        use lash_protocol_types::allocation::*;
        use std::sync::atomic::{AtomicU64, Ordering};
        #[derive(Clone)]
        struct Ledger {
            used: Arc<AtomicU64>,
            limit: u64,
        }
        struct Hold(Ledger, u64);
        impl AllocationReservation for Hold {
            fn retain(&self) {}
        }
        impl Drop for Hold {
            fn drop(&mut self) {
                self.0.used.fetch_sub(self.1, Ordering::SeqCst);
            }
        }
        impl AllocationLedger for Ledger {
            fn reserve(&self, bytes: u64) -> Result<AllocationHold, AllocationError> {
                self.used
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                        used.checked_add(bytes).filter(|total| *total <= self.limit)
                    })
                    .map_err(|_| AllocationError {
                        kind: ReserveError::Pressure,
                        actor: "native".into(),
                        requested: bytes,
                        capacity: self.limit,
                    })?;
                Ok(AllocationHold::new(Hold(self.clone(), bytes)))
            }
            fn reserve_shared(
                &self,
                _: usize,
                bytes: u64,
            ) -> Result<AllocationHold, AllocationError> {
                self.reserve(bytes)
            }
        }
        let view = PluginNativeView {
            request: PluginTransitionRequest::new(PluginTransitionRequestData {
                id: PluginTransitionId(
                    crate::EffectAddress::new(
                        crate::ExecutionScope::turn("capture", "run"),
                        "plugin-transition",
                    )
                    .unwrap(),
                ),
                owner: crate::RuntimeOwner::Session("capture".into()),
                base: PluginTransitionBase::SessionCommand { run: "run".into() },
                target: crate::store::plugin_writers::PluginAdmission::from_plugins(vec![
                    crate::store::plugin_writers::AdmittedPlugin {
                        plugin: "plugin".repeat(1024),
                        writer: crate::FormatVersion::ONE,
                    },
                ]),
            }),
            source: crate::BlobRef::for_content(b"source"),
            generation: Some(crate::ExecutableGeneration::new("generation".repeat(1024))),
            state: PluginState::default(),
            config: PluginConfig::default(),
        };
        let used = Arc::new(AtomicU64::new(0));
        let tiny = AllocationBudget::new(Ledger {
            used: used.clone(),
            limit: 1,
        });
        assert!(
            RetainedPluginNativeView::capture_in(&view, &tiny).is_err(),
            "one-byte capture must refuse metadata before copying"
        );
        assert_eq!(used.load(Ordering::SeqCst), 0);
        let budget = AllocationBudget::new(Ledger {
            used: used.clone(),
            limit: 1 << 20,
        });
        let captured = RetainedPluginNativeView::capture_in(&view, &budget).unwrap();
        let charged = used.load(Ordering::SeqCst);
        assert!(charged >= view.generation.as_ref().unwrap().as_str().len() as u64);
        let reader = captured.clone();
        drop(captured);
        assert_eq!(used.load(Ordering::SeqCst), charged);
        assert_eq!(reader.view().source, view.source);
        let request = reader.view().request.clone();
        let admission = request.target.clone();
        let shared = request.clone_in(&budget).unwrap();
        assert!(
            Arc::ptr_eq(&request.0, &shared.0),
            "same-owner request clones share metadata"
        );
        drop(shared);
        assert!(request.clone_in(&tiny).is_err());
        assert!(admission.clone_in(&tiny).is_err());
        drop(reader);
        assert!(
            used.load(Ordering::SeqCst) > 0,
            "escaped request retains metadata admission"
        );
        drop(request);
        assert!(used.load(Ordering::SeqCst) >= admission.plugins()[0].plugin.len() as u64);
        let writers = admission.writers().unwrap();
        assert_eq!(writers.len(), 1);
        drop(admission);
        assert!(
            used.load(Ordering::SeqCst) > 0,
            "writer projection retains its own admission"
        );
        drop(writers);
        assert_eq!(used.load(Ordering::SeqCst), 0);
    }

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
