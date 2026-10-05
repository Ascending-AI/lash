//! Recorded plugin admission and native view in the existing checkpoint set.
use super::{PendingCheckpointComponentBody, ResidentCheckpointComponent, RuntimeSessionState};

impl RuntimeSessionState {
    /// The recorded native plugin view published with the current checkpoint.
    pub fn plugin_admission_snapshot(&self) -> Option<std::sync::Arc<[u8]>> {
        self.checkpoint_components
            .component(crate::store::PLUGIN_ADMISSION_CHECKPOINT_COMPONENT)
            .and_then(ResidentCheckpointComponent::opaque_body)
    }

    /// A new owner must record its own admission before constructing capabilities.
    pub fn clear_plugin_admission_snapshot(&mut self) {
        self.checkpoint_components
            .entries
            .remove(crate::store::PLUGIN_ADMISSION_CHECKPOINT_COMPONENT);
    }

    pub fn set_plugin_admission_snapshot(&mut self, bytes: std::sync::Arc<[u8]>) {
        let key = crate::store::PLUGIN_ADMISSION_CHECKPOINT_COMPONENT.to_owned();
        let previous = self.checkpoint_components.entries.get(&key);
        if previous
            .and_then(ResidentCheckpointComponent::opaque_body)
            .as_deref()
            == Some(bytes.as_ref())
        {
            return;
        }
        let descriptor = previous.and_then(|entry| entry.descriptor().cloned());
        if descriptor
            .as_ref()
            .is_some_and(|descriptor| descriptor.blob_ref == crate::BlobRef::for_content(&bytes))
        {
            return;
        }
        self.checkpoint_components.entries.insert(
            key,
            ResidentCheckpointComponent::Changed {
                descriptor,
                body: PendingCheckpointComponentBody::Opaque(bytes),
            },
        );
    }
}
