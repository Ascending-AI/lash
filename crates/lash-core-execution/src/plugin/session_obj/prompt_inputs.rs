//! What the session gives a prompt cut and a turn's request history: the
//! committed plugin namespaces and the attachments the request omits.

use std::collections::{BTreeMap, BTreeSet};

use lash_sansio::sync::MutexExt;

use super::super::prompt::CommittedPluginNamespace;
use super::super::{AttachmentOmissionContext, HistoryPartId};
use super::*;

impl PluginSession {
    /// The attachments of `history` the turn's request omits: the union of
    /// every registered attachment-omission policy's decisions (ADR 0133).
    pub fn attachment_omissions(
        &self,
        ctx: &AttachmentOmissionContext,
        history: &[crate::Message],
    ) -> Result<BTreeSet<HistoryPartId>, ContextError> {
        self.validate_recorded_admission()?;
        let mut omissions = BTreeSet::new();
        for (_, registered) in &self
            .capabilities()
            .contributions
            .attachment_omission_policies
        {
            omissions.extend(registered.hook.omissions(ctx, history)?);
        }
        Ok(omissions)
    }

    /// Every plugin namespace, each frozen at its published generation, read
    /// under one lock: the namespaces of one prompt cut.
    pub fn committed_namespaces(&self) -> BTreeMap<String, CommittedPluginNamespace> {
        let registry = self.state.lock_recover();
        registry
            .data
            .plugins
            .iter()
            .map(|(plugin, namespace)| {
                (
                    plugin.clone(),
                    CommittedPluginNamespace::new(namespace.generation, namespace.values.clone()),
                )
            })
            .collect()
    }
}
