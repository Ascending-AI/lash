//! A session head's ownership of its last immutable phase configurations.

use super::{
    PendingCheckpointComponentBody, ResidentCheckpointComponent, RuntimeCheckpointComponents,
    RuntimeSessionState,
};
use crate::checkpoint_graph::{DecodeLimits, EncodedNode, GraphRefusal, NodeId};
use crate::phase_preparation::{COMPONENT, PhaseContentPreparations};
use crate::store::{BlobRef, HydratedSessionCheckpoint, SessionCheckpoint, StoreError};
use std::collections::BTreeMap;
use std::sync::Arc;

fn encoding(error: GraphRefusal) -> StoreError {
    StoreError::RecordEncodingFailed {
        record_kind: "phase content preparation".into(),
        message: error.to_string(),
    }
}

impl RuntimeCheckpointComponents {
    pub(super) fn load_phase_preparations(
        checkpoint: &HydratedSessionCheckpoint,
        manifest: &SessionCheckpoint,
    ) -> Result<Option<(BlobRef, PhaseContentPreparations)>, StoreError> {
        let Some(descriptor) = manifest.components.get(COMPONENT) else {
            return Ok(None);
        };
        let invalid = |message| StoreError::StoredDataCorrupt {
            record_kind: "phase content preparation",
            message,
        };
        let root = NodeId::from_hex(descriptor.blob_ref.as_str())
            .ok_or_else(|| invalid("the component root is not a node digest".into()))?;
        let prepared = PhaseContentPreparations::load(
            &checkpoint.component_nodes,
            root,
            DecodeLimits::within_total(1 << 32),
        )
        .map_err(|error| invalid(error.to_string()))?;
        Ok(Some((descriptor.blob_ref.clone(), prepared)))
    }

    pub(super) fn stage_phase_preparations(
        &self,
        nodes: &mut BTreeMap<NodeId, EncodedNode>,
    ) -> Result<(), StoreError> {
        if matches!(
            self.entries.get(COMPONENT),
            Some(ResidentCheckpointComponent::Changed { .. })
        ) && let Some((_, prepared)) = &self.phase_preparations
        {
            nodes.extend(prepared.to_graph().map_err(encoding)?.1);
        }
        Ok(())
    }

    pub(super) fn retain_phase_preparations(&mut self, manifest: &SessionCheckpoint) {
        if self.phase_preparations.as_ref().is_some_and(|(root, _)| {
            manifest
                .components
                .get(COMPONENT)
                .map(|value| &value.blob_ref)
                != Some(root)
        }) {
            self.phase_preparations = None;
        }
    }

    fn set_phase_preparations(
        &mut self,
        prepared: PhaseContentPreparations,
    ) -> Result<(), StoreError> {
        if prepared.is_empty() {
            self.entries.remove(COMPONENT);
            self.phase_preparations = None;
            return Ok(());
        }
        if self
            .phase_preparations
            .as_ref()
            .is_some_and(|(_, current)| *current == prepared)
        {
            return Ok(());
        }
        let (root, nodes) = prepared.to_graph().map_err(encoding)?;
        let body = nodes
            .get(&root)
            .map(|node| Arc::clone(&node.bytes))
            .ok_or_else(|| StoreError::RecordEncodingFailed {
                record_kind: "phase content preparation".into(),
                message: "the encoder omitted its root".into(),
            })?;
        let descriptor = self
            .entries
            .get(COMPONENT)
            .and_then(ResidentCheckpointComponent::descriptor)
            .cloned();
        self.entries.insert(
            COMPONENT.into(),
            ResidentCheckpointComponent::Changed {
                descriptor,
                body: PendingCheckpointComponentBody::Opaque(body),
            },
        );
        self.phase_preparations = Some((BlobRef(root.to_hex()), prepared));
        Ok(())
    }
}

impl RuntimeSessionState {
    /// Encoding pins authenticated by the acknowledged complete checkpoint.
    pub fn phase_content_preparations(&self) -> Option<&PhaseContentPreparations> {
        self.checkpoint_components
            .phase_preparations
            .as_ref()
            .map(|(_, prepared)| prepared)
    }

    /// Transfer the last phase's configuration roots to the terminal head.
    pub fn set_phase_content_preparations(
        &mut self,
        prepared: PhaseContentPreparations,
    ) -> Result<(), StoreError> {
        self.checkpoint_components.set_phase_preparations(prepared)
    }
}
