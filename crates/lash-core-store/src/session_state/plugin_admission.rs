//! Recorded plugin admission and native view in the existing checkpoint set.
use super::{
    PendingCheckpointComponentBody, ResidentCheckpointComponent, ResidentCheckpointComponentBody,
    RuntimeSessionState, SessionPluginStateSource,
};

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

    /// Give the released plugin-admission and plugin-state bodies back the
    /// bytes `plugins`, the live session they were captured from, captures
    /// now, when each capture is the body the head content-addresses
    /// (FIG-5137).
    ///
    /// A committed head keeps only these components' durable refs once their
    /// bodies are released. A resident at that head stands in for a window
    /// read only if its live plugins capture exactly what the head recorded:
    /// each capture is compared by content address with its recorded ref,
    /// and a component with no recorded ref matches only an empty capture.
    /// Answers whether both matched; on `false` the state is unchanged and
    /// the caller reads the head from the store.
    pub fn rehydrate_plugin_bodies(
        &mut self,
        plugins: &dyn SessionPluginStateSource,
        fleet: crate::store::FleetFormat,
    ) -> Result<bool, crate::RuntimeError> {
        let config = crate::store::persisted_session_config_from_state(self).plugin_config;
        let admission = plugins.capture_plugin_admission(&config, fleet)?;
        let captured = plugins.capture_plugin_state()?;
        let encoded = crate::store::encode_checkpoint_component(
            crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT,
            &captured,
        )
        .map_err(|error| {
            crate::RuntimeError::new(
                crate::RuntimeErrorCode::RuntimeStoreCorrupt,
                format!("failed to encode captured plugin checkpoint: {error}"),
            )
        })?;
        let entries = &mut self.checkpoint_components.entries;
        let admission_matches = match (
            entries.get(crate::store::PLUGIN_ADMISSION_CHECKPOINT_COMPONENT),
            &admission,
        ) {
            (None, None) => true,
            (
                Some(ResidentCheckpointComponent::Unchanged {
                    descriptor,
                    body: ResidentCheckpointComponentBody::Opaque(_),
                }),
                Some(bytes),
            ) => descriptor.blob_ref == crate::BlobRef::for_content(bytes),
            _ => false,
        };
        let state_matches = match entries.get(crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT) {
            None => captured == crate::PluginState::default(),
            Some(ResidentCheckpointComponent::Unchanged {
                descriptor,
                body: ResidentCheckpointComponentBody::PluginState { .. },
            }) => descriptor.blob_ref == crate::BlobRef::for_content(&encoded),
            Some(_) => false,
        };
        if !(admission_matches && state_matches) {
            return Ok(false);
        }
        if let (
            Some(ResidentCheckpointComponent::Unchanged {
                body: ResidentCheckpointComponentBody::Opaque(body),
                ..
            }),
            Some(bytes),
        ) = (
            entries.get_mut(crate::store::PLUGIN_ADMISSION_CHECKPOINT_COMPONENT),
            admission,
        ) {
            *body = Some(bytes);
        }
        if let Some(ResidentCheckpointComponent::Unchanged {
            body: ResidentCheckpointComponentBody::PluginState { snapshot, .. },
            ..
        }) = entries.get_mut(crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
        {
            *snapshot = Some(captured);
        }
        Ok(true)
    }
}
