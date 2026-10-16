//! What the session gives a prompt cut and a turn's request history: the
//! committed plugin namespaces and the attachments the request omits.

use std::collections::BTreeMap;

use lash_sansio::sync::MutexExt;

use super::super::prompt::CommittedPluginNamespace;
use super::super::{AttachmentOmissionContext, AttachmentOmissions};
use super::*;

impl PluginSession {
    /// The host-selected pool used for this session's prompt composition.
    pub fn prompt_render_pool(&self) -> &super::super::prompt::PromptRenderPool {
        self.host.prompt_render_pool()
    }

    /// The attachments of `history` the turn's request omits: the union of
    /// every registered attachment-omission policy's decisions (ADR 0133).
    /// The first decision in contribution order supplies text for a repeated part.
    pub fn attachment_omissions(
        &self,
        ctx: &AttachmentOmissionContext,
        history: &[crate::Message],
    ) -> Result<AttachmentOmissions, ContextError> {
        let mut omissions = AttachmentOmissions::new();
        for (_, registered) in &self
            .capabilities()
            .contributions
            .attachment_omission_policies
        {
            for (part, text) in registered.hook.omissions(ctx, history)? {
                omissions.entry(part).or_insert(text);
            }
        }
        Ok(omissions)
    }

    /// Every plugin namespace, each frozen at its published generation, read
    /// under one lock: the namespaces of one prompt cut. Plugin-facing
    /// handles expose no namespaces, as with [`Self::export_state`].
    pub fn committed_namespaces(&self) -> BTreeMap<String, CommittedPluginNamespace> {
        if !self.host.export_plugin_namespaces {
            return BTreeMap::new();
        }
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
