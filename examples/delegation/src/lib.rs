//! Delegation as host code, on the `lash` facade alone (ADR 0134).
//!
//! Lash ships no subagent implementation: the core knows sessions, an
//! optional parent link and forks, and nothing more. This crate shows that
//! the facade is enough to build delegation. Its plugin contributes one tool,
//! `spawn_agent` (`agents.spawn` in a cell), whose call:
//!
//! 1. builds the child's create request from what the host configured
//!    explicitly (the child's [`SessionSpec`], tool access, RLM termination
//!    and the plugin's own namespace) and from the call's own arguments. It
//!    reads nothing of the parent session: the parent link the request
//!    records is lineage for display and audit only;
//! 2. declares one `SessionTurn` process start that creates the child and
//!    runs its first turn, under the lifetime the host's policy chooses;
//! 3. parks the call on that start, so the child's final value resolves the
//!    parent's call and its next step reads it.
//!
//! What a delegated child is lives in this plugin's own config namespace
//! ([`DelegatedChild`]), stated in the child's create request. A session
//! the namespace marks as a delegated child is offered `submit_error`
//! (`task.fail`) and a prompt section instead of `spawn_agent`, so a child
//! cannot delegate again. The core never decides any of it.

mod tool;

use std::sync::Arc;

use lash::SessionSpec;
use lash::plugins::{
    CandidateFacts, ConfigOwner, ConfigRegistrar, ConfigRegistrationError, NoRunOptions,
    PluginDeclaration, PluginDefinition, PluginError, PluginFactory, PluginRegistrar,
    PluginSessionContext, PromptInput, PromptSectionSpec, SectionText, SessionPlugin,
    SessionToolAccess,
};
use lash::process::{Lifetime, LifetimePolicy, StartCx};

pub use tool::{
    SESSION_TURN_DEFINITION, SPAWN_HOST_ORIGINATED_PROCESS, spawn_agent_tool_definition,
    submit_error_tool_definition,
};

/// The plugin's id, and the key of its config namespace.
pub const DELEGATION_PLUGIN_ID: &str = "delegation";

/// The key of the prompt section a delegated child is shown.
pub const DELEGATED_CHILD_SECTION: &str = "delegated_child";

/// The text of that section.
pub const DELEGATED_CHILD_INSTRUCTIONS: &str = "You are a delegated child session. Complete the task in the user message and finish with its result. If you cannot, end the task with `task.fail` and a short reason.";

/// What this plugin records, in its own config namespace, for a session its
/// tool created: the session is a delegated child, created to answer `task`.
/// A session whose namespace records nothing is not one.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    lash::plugins::schemars::JsonSchema,
)]
#[schemars(crate = "lash::plugins::schemars")]
#[serde(deny_unknown_fields)]
pub struct DelegatedChild {
    /// The task the child was created to answer.
    pub task: String,
}

/// The owner of the plugin's namespace: it records exactly what the creator
/// states, and a recorded namespace never changes.
struct DelegationConfigOwner;

impl ConfigOwner for DelegationConfigOwner {
    type Create = DelegatedChild;
    type Recorded = DelegatedChild;
    type Refusal = String;
    type RunOptions = NoRunOptions;

    fn create(&self, input: Option<DelegatedChild>) -> Result<Option<DelegatedChild>, String> {
        Ok(input)
    }

    fn validate(
        &self,
        value: &DelegatedChild,
        base: Option<&DelegatedChild>,
        _facts: &CandidateFacts<'_>,
    ) -> Result<(), String> {
        match base {
            Some(base) if base != value => {
                Err("a delegated child's record never changes".to_string())
            }
            _ => Ok(()),
        }
    }

    fn apply_run_options(
        &self,
        recorded: &DelegatedChild,
        _options: NoRunOptions,
    ) -> Result<DelegatedChild, String> {
        Ok(recorded.clone())
    }
}

/// What the host states for every child the tool creates.
#[derive(Clone)]
pub(crate) struct ChildConfig {
    /// The child's whole session config: model, turn budget, tool-call
    /// limit and any plugin options. Nothing is taken from the parent.
    pub(crate) spec: SessionSpec,
    /// The child's tool access.
    pub(crate) tool_access: SessionToolAccess,
    /// The child's initial prompt plan; `None` is the neutral default.
    pub(crate) prompt_plan: Option<lash::prompt::PromptPlan>,
    /// Whether children run the RLM protocol.
    pub(crate) rlm: bool,
    /// How long a child's process lives, decided against the spawn's
    /// admitted start context, never by the model (FIG-3607).
    pub(crate) lifetime: LifetimePolicy,
}

/// The delegation plugin. Install it on a [`lash::LashCoreBuilder`] with
/// [`plugin`](lash::LashCoreBuilder::plugin).
pub struct DelegationPluginFactory {
    child: Arc<ChildConfig>,
}

impl DelegationPluginFactory {
    /// Children created from `child_spec` under `child_tool_access`, whose
    /// processes take `lifetime`, for example
    /// [`lash::process::lifetime::starter`]. The spec must state a model, a
    /// turn budget and a tool-call limit, and the host states the children's
    /// tool authority (ambient access resolves against the child's own
    /// catalog): a child has no other source for them.
    pub fn new(
        child_tool_access: SessionToolAccess,
        child_spec: SessionSpec,
        lifetime: impl Fn(&StartCx) -> Lifetime + Send + Sync + 'static,
    ) -> Self {
        Self {
            child: Arc::new(ChildConfig {
                spec: child_spec,
                tool_access: child_tool_access,
                prompt_plan: None,
                rlm: false,
                lifetime: Arc::new(lifetime),
            }),
        }
    }

    /// The prompt plan every child starts with. Without one a child starts
    /// with the neutral default, whatever its parent's plan.
    #[must_use]
    pub fn with_child_prompt_plan(mut self, plan: lash::prompt::PromptPlan) -> Self {
        self.config_mut().prompt_plan = Some(plan);
        self
    }

    /// Children run the RLM protocol: each must finish through `control.finish`,
    /// with a value of the call's `output` shape when it states one, written
    /// by the program. Only an RLM child accepts a `seed`.
    #[must_use]
    pub fn with_rlm_children(mut self) -> Self {
        self.config_mut().rlm = true;
        self
    }

    fn config_mut(&mut self) -> &mut ChildConfig {
        Arc::make_mut(&mut self.child)
    }
}

impl PluginDefinition for DelegationPluginFactory {
    fn declaration() -> PluginDeclaration {
        PluginDeclaration::initial(DELEGATION_PLUGIN_ID)
    }
}

impl PluginFactory for DelegationPluginFactory {
    fn id(&self) -> &'static str {
        DELEGATION_PLUGIN_ID
    }

    fn register_config(
        &self,
        registrar: &mut ConfigRegistrar,
    ) -> Result<(), ConfigRegistrationError> {
        registrar.owner(DelegationConfigOwner)
    }

    /// A session's role is read from its own recorded namespace: one this
    /// plugin created as a delegated child answers its task; any other
    /// session, a process runtime included, may delegate.
    fn build(&self, ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        let delegated = match ctx.plugin_config.config.get(DELEGATION_PLUGIN_ID) {
            Some(recorded) => Some(
                serde_json::from_value::<DelegatedChild>(recorded.clone())
                    .map_err(|error| PluginError::Session(error.to_string()))?,
            ),
            None => None,
        };
        Ok(Arc::new(DelegationSessionPlugin {
            child: Arc::clone(&self.child),
            delegated: delegated.is_some(),
        }))
    }
}

struct DelegationSessionPlugin {
    child: Arc<ChildConfig>,
    delegated: bool,
}

impl SessionPlugin for DelegationSessionPlugin {
    fn id(&self) -> &'static str {
        DELEGATION_PLUGIN_ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        if !self.delegated {
            return reg
                .tools()
                .provider(tool::spawn_provider(Arc::clone(&self.child)));
        }
        reg.tools().provider(tool::submit_error_provider())?;
        reg.prompt().section(
            PromptSectionSpec::new(
                lash::prompt::PromptSectionKey::new(DELEGATED_CHILD_SECTION)
                    .map_err(|error| PluginError::Registration(error.to_string()))?,
                lash::prompt::PromptPlacement::InitialInstructions,
            ),
            Arc::new(|input: &PromptInput<'_>| {
                Ok(
                    match input
                        .config::<DelegatedChild>()
                        .map_err(|error| lash::plugins::PromptRenderError::new(error.to_string()))?
                    {
                        Some(_) => SectionText::text(DELEGATED_CHILD_INSTRUCTIONS),
                        None => SectionText::Omit,
                    },
                )
            }),
        )
    }
}

#[cfg(test)]
mod tests;
