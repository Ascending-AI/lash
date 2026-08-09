pub const SESSION_CHECKPOINT_SCHEMA_VERSION: u32 = 2;

/// Encoding implemented for checkpoint-component logical bytes in this build.
pub const CHECKPOINT_COMPONENT_ENCODING_VERSION: u32 = 1;
/// Well-known component key used by the runtime's tool registry snapshot.
pub const TOOL_STATE_CHECKPOINT_COMPONENT: &str = "tool_state";
/// Well-known component key used by the runtime's plugin-session snapshot.
pub const PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT: &str = "plugin_snapshot";
/// Well-known component key used by protocol-owned execution state.
pub const EXECUTION_STATE_CHECKPOINT_COMPONENT: &str = "execution_state";

/// Complete durable root for a session checkpoint.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionCheckpoint {
    pub schema_version: u32,
    pub turn_state: crate::PersistedTurnState,
    /// Complete keyed component listing. A key absent here is deleted.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub components: std::collections::BTreeMap<String, CheckpointComponentDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_snapshot_revision: Option<u64>,
}

impl Default for SessionCheckpoint {
    fn default() -> Self {
        Self {
            schema_version: SESSION_CHECKPOINT_SCHEMA_VERSION,
            turn_state: crate::PersistedTurnState::default(),
            components: std::collections::BTreeMap::new(),
            plugin_snapshot_revision: None,
        }
    }
}

impl SessionCheckpoint {
    pub fn new(
        turn_state: crate::PersistedTurnState,
        components: std::collections::BTreeMap<String, CheckpointComponentDescriptor>,
        plugin_snapshot_revision: Option<u64>,
    ) -> Self {
        Self {
            schema_version: SESSION_CHECKPOINT_SCHEMA_VERSION,
            turn_state,
            components,
            plugin_snapshot_revision,
        }
    }

    pub fn component_ref(&self, key: &str) -> Option<&BlobRef> {
        self.components
            .get(key)
            .map(|component| &component.blob_ref)
    }

    pub fn validate_component_encoding_versions(&self) -> Result<(), StoreError> {
        for (key, descriptor) in &self.components {
            ensure_checkpoint_component_encoding_version(key, descriptor.encoding_version)?;
        }
        Ok(())
    }
}

/// Durable address and codec identity for one checkpoint component.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CheckpointComponentDescriptor {
    pub blob_ref: BlobRef,
    pub encoding_version: u32,
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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HydratedCheckpointComponent {
    Changed {
        encoding_version: u32,
        #[serde(with = "serde_bytes")]
        body: Vec<u8>,
    },
    Unchanged {
        descriptor: CheckpointComponentDescriptor,
    },
    Hydrated {
        descriptor: CheckpointComponentDescriptor,
        #[serde(with = "serde_bytes")]
        body: Vec<u8>,
    },
}

impl HydratedCheckpointComponent {
    /// Marks logical component bytes as changed and in need of durable storage.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// must persist `body` under its content-derived [`BlobRef`] before
    /// publishing the checkpoint root. This constructor records the component
    /// encoding implemented by the current Lash build; callers must not use it
    /// to smuggle bytes encoded for a different version.
    pub fn changed(body: Vec<u8>) -> Self {
        Self::Changed {
            encoding_version: CHECKPOINT_COMPONENT_ENCODING_VERSION,
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
    pub fn hydrated(descriptor: CheckpointComponentDescriptor, body: Vec<u8>) -> Self {
        Self::Hydrated { descriptor, body }
    }

    /// Returns the codec version that governs this component's logical bytes.
    ///
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
    ) -> Result<CheckpointComponentDescriptor, StoreError> {
        ensure_checkpoint_component_encoding_version(key, self.encoding_version())?;
        match self {
            Self::Changed {
                encoding_version,
                body,
            } => Ok(CheckpointComponentDescriptor {
                blob_ref: BlobRef(crate::stable_hash::sha256_hex(body)),
                encoding_version: *encoding_version,
            }),
            Self::Unchanged { descriptor } => Ok(descriptor.clone()),
            Self::Hydrated { descriptor, body } => {
                let actual_ref = BlobRef(crate::stable_hash::sha256_hex(body));
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

pub fn ensure_checkpoint_component_encoding_version(
    key: &str,
    actual: u32,
) -> Result<(), StoreError> {
    if actual == CHECKPOINT_COMPONENT_ENCODING_VERSION {
        Ok(())
    } else {
        Err(StoreError::CheckpointComponentEncodingVersionMismatch {
            key: key.to_string(),
            actual,
            expected: CHECKPOINT_COMPONENT_ENCODING_VERSION,
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
    /// Complete keyed component listing. A key absent here is deleted.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub components: std::collections::BTreeMap<String, HydratedCheckpointComponent>,
    pub plugin_snapshot_revision: Option<u64>,
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
        self.component(key).and_then(HydratedCheckpointComponent::blob_ref)
    }

    /// Returns logical bytes for a changed or store-hydrated component entry.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// must distinguish a reference-only [`HydratedCheckpointComponent::Unchanged`]
    /// entry from an absent key. Only the latter means deletion in this complete
    /// component set.
    pub fn component_body(&self, key: &str) -> Option<&[u8]> {
        self.component(key).and_then(HydratedCheckpointComponent::body)
    }

    /// Returns a hydrated component body after enforcing the component codec.
    ///
    /// Opaque owners such as protocol execution state use this instead of
    /// [`Self::decode_component`]: Lash validates the envelope but does not
    /// interpret owner-defined body bytes.
    pub(crate) fn checked_component_body(&self, key: &str) -> Result<Option<&[u8]>, StoreError> {
        let Some(component) = self.component(key) else {
            return Ok(None);
        };
        ensure_checkpoint_component_encoding_version(key, component.encoding_version())?;
        component
            .body()
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
    pub fn decode_component<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, StoreError> {
        let Some(body) = self.checked_component_body(key)? else {
            return Ok(None);
        };
        rmp_serde::from_slice(body)
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
    pub fn manifest(&self) -> Result<SessionCheckpoint, StoreError> {
        let components = self
            .components
            .iter()
            .map(|(key, component)| {
                component
                    .projected_descriptor(key)
                    .map(|descriptor| (key.clone(), descriptor))
            })
            .collect::<Result<_, StoreError>>()?;
        Ok(SessionCheckpoint::new(
            self.turn_state.clone(),
            components,
            self.plugin_snapshot_revision,
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

#[cfg(test)]
mod checkpoint_tests {
    use super::*;

    #[test]
    fn checked_component_body_validates_the_opaque_component_envelope() {
        let key = EXECUTION_STATE_CHECKPOINT_COMPONENT;
        let checkpoint = HydratedSessionCheckpoint {
            components: [(
                key.to_string(),
                HydratedCheckpointComponent::Changed {
                    encoding_version: CHECKPOINT_COMPONENT_ENCODING_VERSION + 1,
                    body: b"owner-defined".to_vec(),
                },
            )]
            .into_iter()
            .collect(),
            ..HydratedSessionCheckpoint::default()
        };

        assert!(matches!(
            checkpoint.checked_component_body(key),
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
