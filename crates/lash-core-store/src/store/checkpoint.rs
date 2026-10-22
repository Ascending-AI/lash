use super::*;
use lash_kernel_doc::allocation::SharedBytes;
use std::sync::Arc;

/// `serde_bytes` wire shape for an `SharedBytes` body: identical bytes encoding
/// on serialize, one copy into the shared buffer on deserialize.
mod arc_serde_bytes {
    use lash_kernel_doc::allocation::SharedBytes;

    pub(super) fn serialize<S: serde::Serializer>(
        body: &SharedBytes,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(body)
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<SharedBytes, D::Error> {
        let bytes: Vec<u8> = serde_bytes::deserialize(deserializer)?;
        Ok(bytes.into())
    }
}

/// Version 4 (FIG-1961) retypes `PersistedTurnState.last_prompt_usage` to the
/// checked `LlmUsage` shape; v3 roots carrying the retired `PromptUsage`
/// snapshot fields are refused rather than remapped.
///
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "migrate"
/// format_manifest = "SessionCheckpointManifest"
pub const SESSION_CHECKPOINT_SCHEMA_VERSION: u32 = 1;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the surface one version on
/// with version 4's shape; its registered lift reads what N wrote.
#[cfg(feature = "synthetic-next")]
/// version_surface = "migrate"
/// format_manifest = "SessionCheckpointManifest"
pub const SESSION_CHECKPOINT_SCHEMA_VERSION: u32 = 2;

/// Encoding implemented for checkpoint-component logical bytes in this build.
///
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "migrate"
/// format_manifest = "CheckpointComponentEncoding"
pub const CHECKPOINT_COMPONENT_ENCODING_VERSION: u32 = 1;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the surface one version on
/// with version 2's shape; its registered lift reads what N wrote.
#[cfg(feature = "synthetic-next")]
/// version_surface = "migrate"
/// format_manifest = "CheckpointComponentEncoding"
pub const CHECKPOINT_COMPONENT_ENCODING_VERSION: u32 = 2;
/// The component key the retired tool-state snapshot was stored under. A
/// checkpoint that still names it is refused at decode (FIG-6074).
pub const RETIRED_TOOL_STATE_CHECKPOINT_COMPONENT: &str = "tool_state";
/// Well-known component key used by the runtime's plugin-session snapshot.
pub const PLUGIN_STATE_CHECKPOINT_COMPONENT: &str = "plugin_state";
/// Well-known component key for the recorded plugin admission and native view.
pub const PLUGIN_ADMISSION_CHECKPOINT_COMPONENT: &str = "plugin_admission";

/// Complete durable root for a session checkpoint.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SessionCheckpoint {
    pub schema_version: u32,
    pub turn_state: crate::PersistedTurnState,
    /// A key absent here is deleted.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub components: std::collections::BTreeMap<String, CheckpointComponentDescriptor>,
    /// The session's protocol execution state: the session descriptor page
    /// the manifest names as a typed child (ADR 0145 §9), or none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_ref: Option<BlobRef>,
}

impl Default for SessionCheckpoint {
    /// A manifest outside any store — snapshots, tests, and in-memory state —
    /// carries this build's own fleet format, the only generation a context
    /// that never consulted a store could write.
    fn default() -> Self {
        Self::for_fleet(crate::store::FleetFormat::current())
    }
}

impl SessionCheckpoint {
    /// The manifest a durable writer publishes under `fleet_format`: the
    /// `schema_version` stamp is the fleet's writer version for the
    /// checkpoint surface (FIG-3796), not the bare build constant.
    pub fn for_fleet(fleet_format: crate::store::FleetFormat) -> Self {
        Self {
            schema_version: fleet_format
                .writer_version(crate::surface_format!(SESSION_CHECKPOINT_SCHEMA_VERSION)),
            turn_state: crate::PersistedTurnState::default(),
            components: std::collections::BTreeMap::new(),
            execution_ref: None,
        }
    }

    pub fn new(
        turn_state: crate::PersistedTurnState,
        components: std::collections::BTreeMap<String, CheckpointComponentDescriptor>,
        execution_ref: Option<BlobRef>,
        fleet_format: crate::store::FleetFormat,
    ) -> Self {
        Self {
            schema_version: fleet_format
                .writer_version(crate::surface_format!(SESSION_CHECKPOINT_SCHEMA_VERSION)),
            turn_state,
            components,
            execution_ref,
        }
    }

    pub fn component_ref(&self, key: &str) -> Option<&BlobRef> {
        self.components
            .get(key)
            .map(|component| component.blob_ref.as_ref())
    }

    /// A manifest outside any store validates component encodings against
    /// this build's newest version alone; a bound store's reads go through
    /// [`Self::validate_component_encoding_versions_for_fleet`].
    pub fn validate_component_encoding_versions(&self) -> Result<(), StoreError> {
        self.validate_component_encoding_versions_for_fleet(crate::store::FleetFormat::current())
    }

    /// The fleet leg of [`Self::validate_component_encoding_versions`]: a
    /// component's `encoding_version` admits this build's newest and the
    /// version `fleet` records for the component-encoding surface — the
    /// `[N-1, N]` reader window of ADR 0106 §2 (FIG-3796).
    pub fn validate_component_encoding_versions_for_fleet(
        &self,
        fleet_format: crate::store::FleetFormat,
    ) -> Result<(), StoreError> {
        let window =
            fleet_format.read_window(crate::surface_format!(SESSION_CHECKPOINT_SCHEMA_VERSION));
        if !window.admits(self.schema_version) {
            crate::store::ensure_supported_schema_version(
                "SessionCheckpoint",
                self.schema_version,
                window.newest(),
            )?;
        }
        for (key, descriptor) in &self.components {
            ensure_checkpoint_component_encoding_version_for_fleet(
                key,
                descriptor.encoding_version,
                fleet_format,
            )?;
        }
        Ok(())
    }
}

/// An immutable checkpoint address, with admission on the allocation that
/// descriptor and component clones retain. Its serialized form is BlobRef.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct SharedCheckpointAddress(Arc<CheckpointAddressStorage>);
#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
struct CheckpointAddressStorage {
    value: BlobRef,
    #[serde(skip)]
    _allocation: lash_protocol_types::allocation::AllocationHold,
    #[serde(skip)]
    budget: lash_protocol_types::allocation::AllocationBudget,
}
impl SharedCheckpointAddress {
    /// Reserve address and storage capacity before copying a borrowed reference.
    pub fn copy_from_ref_in(
        value: &BlobRef,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<Self, lash_protocol_types::allocation::AllocationError> {
        let allocation = budget.reserve(
            value.0.capacity() as u64 + std::mem::size_of::<CheckpointAddressStorage>() as u64 + 64,
        )?;
        Ok(Self(Arc::new(CheckpointAddressStorage {
            value: value.clone(),
            _allocation: allocation,
            budget: budget.clone(),
        })))
    }
    /// Share this ledger's admitted address; reserve before copying a foreign one.
    pub fn clone_in(
        &self,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<Self, lash_protocol_types::allocation::AllocationError> {
        if self.0.budget.identity() == budget.identity() {
            return Ok(self.clone());
        }
        Self::copy_from_ref_in(self, budget)
    }
    /// Transfer fresh whole-source decode admission before exposing readers.
    pub fn decoded_in(
        &mut self,
        allocation: lash_protocol_types::allocation::AllocationHold,
        budget: lash_protocol_types::allocation::AllocationBudget,
    ) -> bool {
        let Some(storage) = Arc::get_mut(&mut self.0) else {
            return false;
        };
        storage._allocation = allocation;
        storage.budget = budget;
        true
    }
}
impl std::fmt::Display for SharedCheckpointAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.value.fmt(f)
    }
}
impl From<BlobRef> for SharedCheckpointAddress {
    fn from(value: BlobRef) -> Self {
        Self(Arc::new(CheckpointAddressStorage {
            value,
            _allocation: Default::default(),
            budget: Default::default(),
        }))
    }
}
impl std::ops::Deref for SharedCheckpointAddress {
    type Target = BlobRef;
    fn deref(&self) -> &Self::Target {
        &self.0.value
    }
}
impl AsRef<BlobRef> for SharedCheckpointAddress {
    fn as_ref(&self) -> &BlobRef {
        &self.0.value
    }
}
impl PartialEq for SharedCheckpointAddress {
    fn eq(&self, other: &Self) -> bool {
        self.0.value == other.0.value
    }
}
impl Eq for SharedCheckpointAddress {}
impl PartialEq<BlobRef> for SharedCheckpointAddress {
    fn eq(&self, other: &BlobRef) -> bool {
        self.as_ref() == other
    }
}
impl PartialEq<SharedCheckpointAddress> for BlobRef {
    fn eq(&self, other: &SharedCheckpointAddress) -> bool {
        self == other.as_ref()
    }
}

/// Durable address and codec identity for one checkpoint component.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct CheckpointComponentDescriptor {
    pub blob_ref: SharedCheckpointAddress,
    pub encoding_version: u32,
}

impl CheckpointComponentDescriptor {
    pub fn clone_in(
        &self,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<Self, lash_protocol_types::allocation::AllocationError> {
        Ok(Self {
            blob_ref: self.blob_ref.clone_in(budget)?,
            encoding_version: self.encoding_version,
        })
    }
    /// Transfer a preadmitted fresh decode before sharing its address.
    pub fn decoded_in(
        &mut self,
        allocation: lash_protocol_types::allocation::AllocationHold,
        budget: lash_protocol_types::allocation::AllocationBudget,
    ) -> bool {
        self.blob_ref.decoded_in(allocation, budget)
    }
}

/// One entry in a complete keyed checkpoint component set.
///
/// The variants make changed bytes, unchanged content references, and a
/// store-hydrated ref/body pair distinct. No invalid neither/neither or
/// ambiguous both/both state is representable.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**
/// consume these entries while committing a checkpoint and produce hydrated
/// entries while loading one. The containing map is authoritative and
/// complete: a key omitted from it is deleted from the new checkpoint rather
/// than inherited from the previous root.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum HydratedCheckpointComponent {
    Changed {
        encoding_version: u32,
        /// Content address derived when the changed body entered the commit plan.
        #[serde(skip_serializing)]
        body_ref: SharedCheckpointAddress,
        #[serde(with = "arc_serde_bytes")]
        body: SharedBytes,
    },
    Unchanged {
        descriptor: CheckpointComponentDescriptor,
    },
    Hydrated {
        descriptor: CheckpointComponentDescriptor,
        #[serde(with = "arc_serde_bytes")]
        body: SharedBytes,
    },
}

impl<'de> serde::Deserialize<'de> for HydratedCheckpointComponent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        enum SerializedHydratedCheckpointComponent {
            Changed {
                encoding_version: u32,
                #[serde(with = "arc_serde_bytes")]
                body: SharedBytes,
            },
            Unchanged {
                descriptor: CheckpointComponentDescriptor,
            },
            Hydrated {
                descriptor: CheckpointComponentDescriptor,
                #[serde(with = "arc_serde_bytes")]
                body: SharedBytes,
            },
        }

        match <SerializedHydratedCheckpointComponent as serde::Deserialize>::deserialize(
            deserializer,
        )? {
            SerializedHydratedCheckpointComponent::Changed {
                encoding_version,
                body,
            } => Ok(Self::changed_with_encoding_version(body, encoding_version)),
            SerializedHydratedCheckpointComponent::Unchanged { descriptor } => {
                Ok(Self::Unchanged { descriptor })
            }
            SerializedHydratedCheckpointComponent::Hydrated { descriptor, body } => {
                Ok(Self::Hydrated { descriptor, body })
            }
        }
    }
}

impl HydratedCheckpointComponent {
    pub fn clone_in(
        &self,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<Self, lash_protocol_types::allocation::AllocationError> {
        Ok(match self {
            Self::Changed {
                encoding_version,
                body_ref,
                body,
            } => Self::Changed {
                encoding_version: *encoding_version,
                body_ref: body_ref.clone_in(budget)?,
                body: body.clone_in(budget)?,
            },
            Self::Unchanged { descriptor } => Self::Unchanged {
                descriptor: descriptor.clone_in(budget)?,
            },
            Self::Hydrated { descriptor, body } => Self::Hydrated {
                descriptor: descriptor.clone_in(budget)?,
                body: body.clone_in(budget)?,
            },
        })
    }

    /// Transfer whole-source admission to every immutable escaping source.
    /// Call only on a fresh decode, before publishing any component readers.
    pub fn decoded_in(
        &mut self,
        allocation: lash_protocol_types::allocation::AllocationHold,
        budget: lash_protocol_types::allocation::AllocationBudget,
    ) -> bool {
        match self {
            Self::Changed { body_ref, body, .. } => {
                body_ref.decoded_in(allocation.clone(), budget.clone())
                    && body.decoded_in(allocation, budget)
            }
            Self::Unchanged { descriptor } => descriptor.decoded_in(allocation, budget),
            Self::Hydrated { descriptor, body } => {
                descriptor.decoded_in(allocation.clone(), budget.clone())
                    && body.decoded_in(allocation, budget)
            }
        }
    }

    /// Admit a serving component's body and address before copying or hashing.
    pub fn changed_for_fleet_in(
        body: &SharedBytes,
        fleet_format: crate::store::FleetFormat,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<Self, lash_protocol_types::allocation::AllocationError> {
        let allocation =
            budget.reserve(64 + std::mem::size_of::<CheckpointAddressStorage>() as u64 + 64)?;
        let body = body.clone_in(budget)?;
        let body_ref = SharedCheckpointAddress(Arc::new(CheckpointAddressStorage {
            value: BlobRef::for_content(&body),
            _allocation: allocation,
            budget: budget.clone(),
        }));
        #[cfg(feature = "perf-witness")]
        crate::perf_witness::record_hash_pass(body.len());
        Ok(Self::Changed {
            encoding_version: fleet_format.writer_version(crate::surface_format!(
                CHECKPOINT_COMPONENT_ENCODING_VERSION
            )),
            body_ref,
            body,
        })
    }

    /// Marks logical component bytes as changed and in need of durable storage.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// must persist `body` under its content-derived [`BlobRef`] before
    /// publishing the checkpoint root. This constructor records the component
    /// encoding implemented by the current Lash build; callers must not use it
    /// to smuggle bytes encoded for a different version. The content address is
    /// retained beside the body so every later commit projection can reuse the
    /// same digest.
    ///
    /// Non-durable and test callers mint at the build's current format; a
    /// durable writer calls [`Self::changed_for_fleet`] with its store's `F`.
    pub fn changed(body: impl Into<SharedBytes>) -> Self {
        Self::changed_for_fleet(body, crate::store::FleetFormat::current())
    }

    /// The fleet-stamped variant: `encoding_version` is the fleet's writer
    /// version for the component-encoding surface (FIG-3796).
    pub fn changed_for_fleet(
        body: impl Into<SharedBytes>,
        fleet_format: crate::store::FleetFormat,
    ) -> Self {
        Self::changed_with_encoding_version(
            body.into(),
            fleet_format.writer_version(crate::surface_format!(
                CHECKPOINT_COMPONENT_ENCODING_VERSION
            )),
        )
    }

    fn changed_with_encoding_version(body: SharedBytes, encoding_version: u32) -> Self {
        let body_ref = BlobRef::for_content(&body).into();
        #[cfg(feature = "perf-witness")]
        crate::perf_witness::record_hash_pass(body.len());
        Self::Changed {
            encoding_version,
            body_ref,
            body,
        }
    }

    /// Carries forward an existing durable component without resubmitting it.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// must verify that the descriptor's blob still exists before publishing
    /// the new checkpoint root. A reference-only entry is not permission to
    /// synthesize missing content or to skip encoding-version validation.
    pub fn unchanged(descriptor: &CheckpointComponentDescriptor) -> Self {
        Self::Unchanged {
            descriptor: descriptor.clone(),
        }
    }

    /// Pairs a manifest descriptor with the logical bytes loaded from storage.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// produce this form only after resolving the descriptor from a fully
    /// loaded checkpoint manifest. The caller must supply the bytes named by
    /// `descriptor`; [`HydratedSessionCheckpoint::manifest`] verifies their
    /// content address before the value may be projected back to a root.
    pub fn hydrated(
        descriptor: CheckpointComponentDescriptor,
        body: impl Into<SharedBytes>,
    ) -> Self {
        Self::Hydrated {
            descriptor,
            body: body.into(),
        }
    }

    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// use this value to reject incompatible components before publishing or
    /// returning a checkpoint; they must not reinterpret an unknown version as
    /// the current encoding.
    pub fn encoding_version(&self) -> u32 {
        match self {
            Self::Changed {
                encoding_version, ..
            } => *encoding_version,
            Self::Unchanged { descriptor } | Self::Hydrated { descriptor, .. } => {
                descriptor.encoding_version
            }
        }
    }

    /// Returns the existing durable address carried by this entry, if any.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// receive `None` only for [`Self::Changed`], whose address must be derived
    /// from and used to persist its body. `Some` does not by itself prove that
    /// the referenced blob exists; commit paths must verify that invariant.
    pub fn blob_ref(&self) -> Option<&BlobRef> {
        match self {
            Self::Changed { .. } => None,
            Self::Unchanged { descriptor } | Self::Hydrated { descriptor, .. } => {
                Some(&descriptor.blob_ref)
            }
        }
    }

    /// Returns submitted or hydrated logical bytes, if this entry carries them.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// receive `None` for [`Self::Unchanged`], where the existing blob must be
    /// resolved by its descriptor. A missing body there does not mean the key
    /// is deleted; deletion is represented only by absence from the containing
    /// complete component map.
    pub fn body(&self) -> Option<&[u8]> {
        match self {
            Self::Changed { body, .. } | Self::Hydrated { body, .. } => Some(body),
            Self::Unchanged { .. } => None,
        }
    }

    /// Integrator class (ADR 0051): same contract as [`Self::body`], but the
    /// clone is a refcount bump instead of a `to_vec` copy — callers that keep
    /// or forward the bytes should prefer it.
    pub fn body_arc(&self) -> Option<SharedBytes> {
        match self {
            Self::Changed { body, .. } | Self::Hydrated { body, .. } => Some(body.clone()),
            Self::Unchanged { .. } => None,
        }
    }

    /// Returns the existing durable descriptor carried by this entry, if any.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// receive `None` only for [`Self::Changed`], whose descriptor is projected
    /// from its body. For referenced entries, callers must preserve both the
    /// address and encoding version and must not substitute a backend-local
    /// codec identity.
    pub fn descriptor(&self) -> Option<&CheckpointComponentDescriptor> {
        match self {
            Self::Changed { .. } => None,
            Self::Unchanged { descriptor } | Self::Hydrated { descriptor, .. } => Some(descriptor),
        }
    }

    fn projected_descriptor(
        &self,
        key: &str,
        fleet_format: crate::store::FleetFormat,
    ) -> Result<CheckpointComponentDescriptor, StoreError> {
        ensure_checkpoint_component_encoding_version_for_fleet(
            key,
            self.encoding_version(),
            fleet_format,
        )?;
        match self {
            Self::Changed {
                encoding_version,
                body_ref,
                ..
            } => Ok(CheckpointComponentDescriptor {
                blob_ref: body_ref.clone(),
                encoding_version: *encoding_version,
            }),
            Self::Unchanged { descriptor } => Ok(descriptor.clone()),
            Self::Hydrated { descriptor, body } => {
                let actual_ref = BlobRef::for_content(body);
                #[cfg(feature = "perf-witness")]
                crate::perf_witness::record_hash_pass(body.len());
                if actual_ref != descriptor.blob_ref {
                    return Err(StoreError::StoredDataCorrupt {
                        record_kind: "HydratedSessionCheckpoint",
                        message: format!(
                            "component `{key}` body hashes to `{actual_ref}` instead of `{}`",
                            descriptor.blob_ref
                        ),
                    });
                }
                Ok(descriptor.clone())
            }
        }
    }
}

/// The no-store form: admits this build's newest encoding alone. Bound
/// stores route through [`ensure_checkpoint_component_encoding_version_for_fleet`].
pub fn ensure_checkpoint_component_encoding_version(
    key: &str,
    actual: u32,
) -> Result<(), StoreError> {
    ensure_checkpoint_component_encoding_version_for_fleet(
        key,
        actual,
        crate::store::FleetFormat::current(),
    )
}

/// The fleet leg of [`ensure_checkpoint_component_encoding_version`]:
/// `actual` admits the `[N-1, N]` window `fleet` records for the
/// component-encoding surface (FIG-3796).
pub fn ensure_checkpoint_component_encoding_version_for_fleet(
    key: &str,
    actual: u32,
    fleet_format: crate::store::FleetFormat,
) -> Result<(), StoreError> {
    let window = fleet_format.read_window(crate::surface_format!(
        CHECKPOINT_COMPONENT_ENCODING_VERSION
    ));
    if window.admits(actual) {
        Ok(())
    } else {
        Err(StoreError::CheckpointComponentEncodingVersionMismatch {
            key: key.to_string(),
            actual,
            expected: window.newest(),
        })
    }
}

/// A checkpoint root paired with the component information needed to commit or load it.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**
/// consume this value on commit and return it on load. `components` is a
/// complete set only when it was assembled by the runtime or derived from a
/// fully hydrated [`SessionCheckpoint`] manifest. Absence of a key in that
/// complete set means deletion; a backend must never merge omitted keys from a
/// previous root.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct HydratedSessionCheckpoint {
    pub turn_state: crate::PersistedTurnState,
    /// A key absent here is deleted.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub components: std::collections::BTreeMap<String, HydratedCheckpointComponent>,
    /// The session's protocol execution state as graph nodes, or none.
    #[serde(skip)]
    pub execution: Option<CheckpointExecution>,
    /// Keyed namespace nodes submitted with this checkpoint, or its loaded closure.
    #[serde(skip)]
    pub namespace_nodes: std::collections::BTreeMap<
        crate::checkpoint_graph::NodeId,
        crate::checkpoint_graph::EncodedNode,
    >,
}

/// A session checkpoint's execution state: its descriptor page (ADR 0145
/// §9). The heap pages under it are published by the commit's candidate
/// (FIG-6418), never carried here.
///
/// Built by the runtime and read back from a store, `descriptor` is the
/// descriptor page's bytes: a store installs it with the manifest, and
/// refuses the commit when a node it names is in neither the store nor the
/// commit. A session commit decoded from its transaction carries none: the
/// descriptor was staged in that transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointExecution {
    /// A SessionExecution node.
    pub root: crate::checkpoint_graph::NodeId,
    pub descriptor: Option<lash_kernel_doc::allocation::SharedBytes>,
}

impl CheckpointExecution {
    /// The root as the manifest names it.
    #[must_use]
    pub fn root_ref(&self) -> BlobRef {
        BlobRef(self.root.to_hex())
    }

    /// The descriptor as the node a commit stages, when it is held.
    #[must_use]
    pub fn node(&self) -> Option<crate::checkpoint_graph::EncodedNode> {
        self.descriptor
            .as_ref()
            .map(|bytes| crate::checkpoint_graph::EncodedNode {
                id: self.root,
                bytes: bytes.clone(),
            })
    }
}

impl HydratedSessionCheckpoint {
    /// Looks up one entry in the checkpoint's authoritative component set.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// may treat `None` as deletion only because the containing checkpoint was
    /// derived from a complete manifest. Callers must not construct a commit
    /// from a partial projection and then use absence to delete unknown keys.
    pub fn component(&self, key: &str) -> Option<&HydratedCheckpointComponent> {
        self.components.get(key)
    }

    /// Returns the existing durable address for a referenced component entry.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// must distinguish `None` because a key is absent from `None` because a
    /// present [`HydratedCheckpointComponent::Changed`] entry still needs its
    /// address derived and persisted.
    pub fn component_ref(&self, key: &str) -> Option<&BlobRef> {
        self.component(key)
            .and_then(HydratedCheckpointComponent::blob_ref)
    }

    /// Returns logical bytes for a changed or store-hydrated component entry.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// must distinguish a reference-only [`HydratedCheckpointComponent::Unchanged`]
    /// entry from an absent key. Only the latter means deletion in this complete
    /// component set.
    pub fn component_body(&self, key: &str) -> Option<&[u8]> {
        self.component(key)
            .and_then(HydratedCheckpointComponent::body)
    }

    /// Returns a hydrated component body after enforcing the component codec.
    ///
    /// Opaque owners such as protocol execution state use this instead of
    /// [`Self::decode_component`]: Lash validates the envelope but does not
    /// interpret owner-defined body bytes.
    /// The component's `encoding_version` admits this build's newest and the
    /// version `fleet` records for the component-encoding surface (FIG-3796).
    pub(crate) fn checked_component_body_for_fleet(
        &self,
        key: &str,
        fleet_format: crate::store::FleetFormat,
    ) -> Result<Option<SharedBytes>, StoreError> {
        let Some(component) = self.component(key) else {
            return Ok(None);
        };
        ensure_checkpoint_component_encoding_version_for_fleet(
            key,
            component.encoding_version(),
            fleet_format,
        )?;
        component
            .body_arc()
            .map(Some)
            .ok_or_else(|| StoreError::StoredDataCorrupt {
                record_kind: "HydratedSessionCheckpoint",
                message: format!("component `{key}` was not hydrated"),
            })
    }

    /// Decodes one fully hydrated component after validating its codec version.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// use this on values returned from a full manifest hydration. `Ok(None)`
    /// means the authoritative manifest omitted `key`; a present reference-only
    /// entry is corrupt at this boundary rather than equivalent to absence.
    /// A manifest outside any store admits this build's newest component
    /// encoding alone; a bound store's reads go through
    /// [`Self::decode_component_for_fleet`].
    pub fn decode_component<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, StoreError> {
        self.decode_component_for_fleet(key, crate::store::FleetFormat::current())
    }

    /// The fleet leg of [`Self::decode_component`]: the component's
    /// `encoding_version` admits the `[N-1, N]` window `fleet` records
    /// (FIG-3796).
    pub fn decode_component_for_fleet<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
        fleet_format: crate::store::FleetFormat,
    ) -> Result<Option<T>, StoreError> {
        let Some(body) = self.checked_component_body_for_fleet(key, fleet_format)? else {
            return Ok(None);
        };
        if crate::plugin_state::namespace_component_plugin(key).is_some() {
            let root_ref = BlobRef::for_content(&body);
            let root =
                crate::checkpoint_graph::NodeId::from_hex(root_ref.as_str()).ok_or_else(|| {
                    StoreError::StoredDataCorrupt {
                        record_kind: "SessionCheckpoint component",
                        message: format!("namespace `{key}` has an invalid root"),
                    }
                })?;
            let namespace = crate::plugin_state::NamespaceBody::load(
                &self.namespace_nodes,
                root,
                crate::checkpoint_graph::DecodeLimits::within_total(1 << 32),
            )
            .map_err(|error| StoreError::StoredDataCorrupt {
                record_kind: "SessionCheckpoint component",
                message: format!("failed to read `{key}`: {error}"),
            })?;
            let map = serde_json::Value::Object(
                namespace
                    .snapshot()
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            );
            return serde_json::from_value(map).map(Some).map_err(|error| {
                StoreError::StoredDataCorrupt {
                    record_kind: "SessionCheckpoint component",
                    message: format!("failed to decode `{key}`: {error}"),
                }
            });
        }
        rmp_serde::from_slice(&body)
            .map(Some)
            .map_err(|error| StoreError::StoredDataCorrupt {
                record_kind: "SessionCheckpoint component",
                message: format!("failed to decode `{key}`: {error}"),
            })
    }

    /// Projects the complete component set into its durable checkpoint manifest.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// call this only on a set assembled from full hydrated state. It derives
    /// addresses for changed bodies, validates hydrated address/body pairs and
    /// encoding versions, and preserves referenced descriptors. Before
    /// publishing the returned root, a backend must store every changed body
    /// and prove that every unchanged reference exists. Keys absent from the
    /// returned manifest are deletions.
    pub fn manifest(
        &self,
        fleet_format: crate::store::FleetFormat,
    ) -> Result<SessionCheckpoint, StoreError> {
        let components = self
            .components
            .iter()
            .map(|(key, component)| {
                component
                    .projected_descriptor(key, fleet_format)
                    .map(|descriptor| (key.clone(), descriptor))
            })
            .collect::<Result<_, StoreError>>()?;
        Ok(SessionCheckpoint::new(
            self.turn_state.clone(),
            components,
            self.execution.as_ref().map(CheckpointExecution::root_ref),
            fleet_format,
        ))
    }
}

/// Enforces agreement between a backend's content address and the checkpoint
/// manifest projected by core before the root can be published.
pub fn ensure_checkpoint_component_hash_agreement(
    key: &str,
    stored_ref: &BlobRef,
    manifest_ref: &BlobRef,
) -> Result<(), StoreError> {
    if stored_ref == manifest_ref {
        Ok(())
    } else {
        Err(StoreError::StoredDataCorrupt {
            record_kind: "HydratedSessionCheckpoint",
            message: format!(
                "backend stored component `{key}` as `{stored_ref}` instead of manifest ref `{manifest_ref}`"
            ),
        })
    }
}

impl crate::store::DurableRecord for SessionCheckpoint {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::store::checkpoint::SESSION_CHECKPOINT_SCHEMA_VERSION);
}

impl crate::store::DurableRecord for HydratedCheckpointComponent {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::store::checkpoint::CHECKPOINT_COMPONENT_ENCODING_VERSION);
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;

    fn budget(
        limit: u64,
    ) -> (
        lash_protocol_types::allocation::AllocationBudget,
        Arc<std::sync::atomic::AtomicU64>,
    ) {
        let used = Arc::new(std::sync::atomic::AtomicU64::new(0));
        (
            lash_protocol_types::allocation::AllocationBudget::new(
                crate::execution_state::retention_tests::Ledger {
                    used: used.clone(),
                    limit,
                },
            ),
            used,
        )
    }

    /// LO-EVICT-ENCODED-OUTPUT: serialization output is admitted before
    /// allocation and its readers retain that admission.
    #[test]
    fn encoded_component_output_is_admitted_before_writing() {
        use std::sync::atomic::{AtomicU64, Ordering};
        struct Payload<'a> {
            text: &'a str,
            used: &'a AtomicU64,
            calls: std::cell::Cell<usize>,
        }
        impl serde::Serialize for Payload<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let call = self.calls.get() + 1;
                self.calls.set(call);
                if call > 1 {
                    assert!(self.used.load(Ordering::SeqCst) >= self.text.len() as u64);
                }
                serializer.serialize_str(self.text)
            }
        }
        let text = "checkpoint".repeat(8192);
        let (tiny, used) = budget(1);
        let payload = Payload {
            text: &text,
            used: &used,
            calls: std::cell::Cell::new(0),
        };
        assert!(matches!(
            crate::store::encode_checkpoint_component("output-law", &payload, &tiny),
            Err(StoreError::Allocation(_))
        ));
        assert_eq!(
            payload.calls.get(),
            1,
            "refusal precedes the output-writing pass"
        );
        let (budget, used) = budget(1 << 20);
        let payload = Payload {
            text: &text,
            used: &used,
            calls: std::cell::Cell::new(0),
        };
        let bytes =
            crate::store::encode_checkpoint_component("output-law", &payload, &budget).unwrap();
        assert_eq!(payload.calls.get(), 2);
        assert!(used.load(Ordering::SeqCst) >= bytes.len() as u64);
        assert_eq!(rmp_serde::from_slice::<String>(&bytes).unwrap(), text);
        let reader = bytes.clone();
        drop(bytes);
        assert!(used.load(Ordering::SeqCst) >= reader.len() as u64);
        drop(reader);
        assert_eq!(used.load(Ordering::SeqCst), 0);
    }

    /// LO-EVICT-COMPONENT: admission precedes an encoded body copy, and
    /// escaped component bytes retain the charge independently of the component.
    #[test]
    fn component_admission_follows_escaped_encoded_bytes() {
        use std::sync::atomic::Ordering;
        let component = HydratedCheckpointComponent::changed(vec![7; 8192]);
        let (tiny, used) = budget(1);
        assert!(
            component.clone_in(&tiny).is_err(),
            "component bytes must be admitted before copying"
        );
        assert_eq!(used.load(Ordering::SeqCst), 0);
        let (budget, used) = budget(1 << 20);
        let admitted = component.clone_in(&budget).unwrap();
        let charged = used.load(Ordering::SeqCst);
        assert!(charged >= 8192);
        let bytes = admitted.body_arc().unwrap();
        let clone = admitted.clone();
        drop(admitted);
        drop(clone);
        assert!(used.load(Ordering::SeqCst) >= 8192);
        assert_eq!(bytes.as_ref(), &[7; 8192]);
        drop(bytes);
        assert_eq!(used.load(Ordering::SeqCst), 0);
    }

    /// LO-EVICT-ENCODED-NODE: envelope clones retain their actual source
    /// admission, and a foreign owner must admit its independent copy.
    #[test]
    fn encoded_node_admission_follows_escaped_envelope_bytes() {
        use std::sync::atomic::Ordering;
        let bytes = vec![9; 8192];
        let node = crate::checkpoint_graph::EncodedNode {
            id: crate::checkpoint_graph::NodeId::of_envelope(&bytes),
            bytes: bytes.into(),
        };
        let (tiny, _) = budget(1);
        assert!(
            node.clone_in(&tiny).is_err(),
            "envelopes must be admitted before copying"
        );
        let (budget, used) = budget(1 << 20);
        let admitted = node.clone_in(&budget).unwrap();
        let charged = used.load(Ordering::SeqCst);
        assert!(charged >= 8192);
        let bytes = admitted.bytes.clone();
        let clone = admitted.clone_in(&budget).unwrap();
        assert_eq!(used.load(Ordering::SeqCst), charged);
        drop(admitted);
        drop(clone);
        assert_eq!(used.load(Ordering::SeqCst), charged);
        assert_eq!(bytes.len(), 8192);
        drop(bytes);
        assert_eq!(used.load(Ordering::SeqCst), 0);
        let mut decoded: SharedBytes =
            rmp_serde::from_slice(&rmp_serde::to_vec(&vec![4_u8; 1024]).unwrap()).unwrap();
        let hold = budget.reserve(4096).unwrap();
        assert!(decoded.decoded_in(hold, budget.clone()));
        let reader = decoded.clone();
        assert!(
            !decoded.decoded_in(Default::default(), Default::default()),
            "a shared decode cannot change ledgers"
        );
        drop(decoded);
        assert_eq!(used.load(Ordering::SeqCst), 4096);
        assert_eq!(reader.len(), 1024);
        drop(reader);
        assert_eq!(used.load(Ordering::SeqCst), 0);
    }

    /// LO-EVICT-DESCRIPTOR: copied durable addresses are charged before
    /// retention and descriptor clones share the admitted source allocation.
    #[test]
    fn descriptor_admission_retains_shared_address_storage() {
        use std::sync::atomic::Ordering;
        let descriptor = CheckpointComponentDescriptor {
            blob_ref: BlobRef::for_content(b"body").into(),
            encoding_version: CHECKPOINT_COMPONENT_ENCODING_VERSION,
        };
        let (tiny, _) = budget(1);
        assert!(
            descriptor.clone_in(&tiny).is_err(),
            "descriptor strings must be admitted before copying"
        );
        let (budget, used) = budget(1 << 20);
        let admitted = descriptor.clone_in(&budget).unwrap();
        let charged = used.load(Ordering::SeqCst);
        assert!(charged >= 64);
        let measurement = crate::history_growth_law::allocation_counter::Measurement::start();
        let clone = admitted.clone();
        assert_eq!(measurement.finish().allocated, 0);
        drop(admitted);
        assert_eq!(used.load(Ordering::SeqCst), charged);
        assert_eq!(clone.blob_ref, descriptor.blob_ref);
        drop(clone);
        assert_eq!(used.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn checked_component_body_validates_the_opaque_component_envelope() {
        let key = "opaque/owner";
        let checkpoint = HydratedSessionCheckpoint {
            components: [(
                key.to_string(),
                HydratedCheckpointComponent::Changed {
                    encoding_version: CHECKPOINT_COMPONENT_ENCODING_VERSION + 1,
                    body_ref: BlobRef::for_content(b"owner-defined").into(),
                    body: (&b"owner-defined"[..]).into(),
                },
            )]
            .into_iter()
            .collect(),
            ..HydratedSessionCheckpoint::default()
        };

        assert!(matches!(
            checkpoint
                .checked_component_body_for_fleet(key, crate::store::FleetFormat::current()),
            Err(StoreError::CheckpointComponentEncodingVersionMismatch {
                key: actual_key,
                actual,
                expected,
            }) if actual_key == key
                && actual == CHECKPOINT_COMPONENT_ENCODING_VERSION + 1
                && expected == CHECKPOINT_COMPONENT_ENCODING_VERSION
        ));
    }

    #[test]
    fn backend_component_hash_must_match_the_manifest() {
        let error = ensure_checkpoint_component_hash_agreement(
            "arbitrary/component",
            &BlobRef("backend-ref".to_string()),
            &BlobRef("manifest-ref".to_string()),
        )
        .expect_err("a divergent backend hash must abort before root publication");

        assert!(matches!(
            error,
            StoreError::StoredDataCorrupt {
                record_kind: "HydratedSessionCheckpoint",
                ref message,
            } if message.contains("arbitrary/component")
                && message.contains("backend-ref")
                && message.contains("manifest-ref")
        ));
    }
}
