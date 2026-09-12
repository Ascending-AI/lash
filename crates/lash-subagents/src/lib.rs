mod capability;
mod rlm;
mod rlm_support;

use std::sync::Arc;

pub use capability::{
    Capability, CapabilityRegistry, StaticCapability, SubagentSpawnContext, TierCapability,
    TierPluginSource, default_explore_plugin_source, default_registry,
};
pub use lash_rlm_types::RlmFinalAnswerFormat;

use lash_core::plugin::{PluginError, PluginFactory, PluginSessionContext};
use lash_core::{
    SessionToolAccess, ToolProvider, facade_support::PluginSpec, facade_support::PluginSpecFactory,
    facade_support::SessionSpec,
};

pub use rlm::spawn_agent_tool_definition;

/// Builds the session-scoped plugin that authors built-in subagent requests.
///
/// # Child tool access
///
/// Built-in capabilities start child requests with this factory's configured
/// [`SessionToolAccess`], which defaults to ambient access. The factory does not
/// automatically copy access from the session being built, so a child may have
/// broader or narrower access than its parent.
///
/// A host that wants built-in child requests to copy each parent's actual access
/// can delegate through a host factory and configure this factory from
/// [`PluginSessionContext::tool_access`]:
///
/// ```ignore
/// impl PluginFactory for HostSubagentsFactory {
///     fn build(
///         &self,
///         ctx: &PluginSessionContext,
///     ) -> Result<Arc<dyn SessionPlugin>, PluginError> {
///         SubagentsPluginFactory::new(Arc::clone(&self.registry))
///             .with_tool_access(ctx.tool_access.clone())
///             .build(ctx)
///     }
/// }
/// ```
///
/// On rematerialization, `ctx.tool_access` is restored from that session's
/// durable authority. The recipe therefore continues to copy the rematerialized
/// parent's recorded access rather than recomputing it from a live parent.
///
/// Copying this access input is not effective-catalog containment. Ambient
/// access is resolved against the child session's own plugin source and catalog
/// contributions, so the resulting child catalog need not be a subset of the
/// parent's catalog. Registered custom [`Capability`] implementations are also
/// trusted to author a complete request and may choose different access.
pub struct SubagentsPluginFactory {
    session_spec: SessionSpec,
    tool_access: SessionToolAccess,
    registry: Arc<CapabilityRegistry>,
    final_answer_format: RlmFinalAnswerFormat,
}

impl SubagentsPluginFactory {
    pub fn new(registry: Arc<CapabilityRegistry>) -> Self {
        Self {
            session_spec: SessionSpec::inherit(),
            tool_access: SessionToolAccess::default(),
            registry,
            final_answer_format: RlmFinalAnswerFormat::RawFinalValue,
        }
    }

    pub fn with_session_spec(mut self, spec: SessionSpec) -> Self {
        self.session_spec = spec;
        self
    }

    /// Sets the access input copied into child requests made by built-in
    /// capabilities.
    ///
    /// This value is factory configuration, not automatic inheritance from the
    /// parent session. See [`SubagentsPluginFactory`] for the host-factory recipe
    /// that copies [`PluginSessionContext::tool_access`] when that is desired.
    pub fn with_tool_access(mut self, access: SessionToolAccess) -> Self {
        self.tool_access = access;
        self
    }

    pub fn with_final_answer_format(mut self, format: RlmFinalAnswerFormat) -> Self {
        self.final_answer_format = format;
        self
    }

    /// Hides resident tools by exact manifest name in built-in child requests.
    ///
    /// Use the callable name, not a dialect-facing display spelling. For
    /// example, the shell input tool is named `write_stdin`; `shell.write` is
    /// only its Lashlang-facing spelling.
    pub fn with_hidden_tools<I, S>(
        mut self,
        tools: I,
    ) -> Result<Self, lash_core::SessionToolAccessError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tool_access = self.tool_access.with_hidden_tools(tools)?;
        Ok(self)
    }
}

impl PluginFactory for SubagentsPluginFactory {
    fn id(&self) -> &'static str {
        "subagents"
    }

    fn build(
        &self,
        ctx: &PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::facade_support::SessionPlugin>, PluginError> {
        let registry = Arc::clone(&self.registry);
        let session_spec = self.session_spec.clone();
        let tool_access = self.tool_access.clone();
        let final_answer_format = self.final_answer_format.clone();
        let parent_subagent = ctx.subagent.clone();

        let implementation = Arc::new(rlm::RlmSubagentToolsProvider {
            registry: Arc::clone(&registry),
            session_spec: session_spec.clone(),
            tool_access,
            final_answer_format,
            parent_subagent,
            include_submit_error: ctx.subagent.is_some(),
        });
        let orchestrating_tool = rlm::spawn_agent_orchestrating_tool(Arc::clone(&implementation));
        let leaf_provider: Option<Arc<dyn ToolProvider>> =
            implementation.include_submit_error.then(|| {
                Arc::new(
                    rlm::RlmSubagentToolsProvider {
                        registry: Arc::clone(&implementation.registry),
                        session_spec: implementation.session_spec.clone(),
                        tool_access: implementation.tool_access.clone(),
                        final_answer_format: implementation.final_answer_format.clone(),
                        parent_subagent: implementation.parent_subagent.clone(),
                        include_submit_error: true,
                    }
                    .into_leaf_provider(),
                ) as Arc<dyn ToolProvider>
            });

        let subagent_authority = ctx.subagent.clone();
        PluginSpecFactory::new(
            "subagents",
            Arc::new(move |_ctx| {
                let mut spec =
                    PluginSpec::new().with_orchestrating_tool(orchestrating_tool.clone());
                if let Some(provider) = leaf_provider.as_ref() {
                    spec = spec.with_tool_provider(Arc::clone(provider));
                }
                if let Some(authority) = subagent_authority.clone() {
                    let note = rlm_support::subagent_capability_note(&authority);
                    spec = spec.with_prompt_contributor(Arc::new(move |_ctx| {
                        let note = note.clone();
                        Box::pin(async move {
                            Ok(vec![lash_core::PromptContribution::execution(
                                "Subagent", note,
                            )])
                        })
                    }));
                }
                Ok(spec)
            }),
        )
        .build(ctx)
    }
}

#[cfg(test)]
mod tests;
