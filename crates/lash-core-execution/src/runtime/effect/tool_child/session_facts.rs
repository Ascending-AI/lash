//! The session facts a tool child records at group open (FIG-3712), so the
//! child runs under the same authority wherever it runs.
//!
//! A group tool child borrows its opener's live context when the opener is
//! live where it runs, and otherwise the deployment builds one. Either way,
//! what decides which commands the child may issue must come from the same
//! recorded facts, never from whichever context happened to serve it: the
//! tool surface nested calls are admitted against, the session's tool access,
//! and its subagent context (which caps recursive spawning). The rebind binds
//! these from the request on both paths.
//!
//! Some of what a live opener lends has no recorded form: tools a turn's
//! context overlay added, plugin factories or a provider a particular open
//! supplied, plugins forked from a parent session, and plugin state the
//! child's plugins may read and mutate. The request records only that such a
//! source was present. A deployment-built context cannot reproduce it, so a
//! child that records one is refused on that path and waits for its live
//! opener.

use serde::{Deserialize, Serialize};

use crate::{SessionToolAccess, SubagentSessionContext, ToolDefinition};

/// What a tool child's session looked like to its opener at group open.
///
/// The default is an ordinary root session with nothing to call beyond the
/// child's own tool: ambient access, no subagent context, nothing unrecorded.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolChildSessionFacts {
    /// The tool surface the opener's calls were admitted against: the
    /// catalog the child's nested calls are admitted against on either path.
    pub tool_surface: Vec<ToolDefinition>,
    /// The session's tool access.
    pub tool_access: SessionToolAccess,
    /// The session's subagent context, when it is a subagent: its depth caps
    /// recursive spawning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentSessionContext>,
    /// The sources the opener's context had that no deployment can rebuild.
    #[serde(default)]
    pub unrecorded: UnrecordedSessionSources,
}

/// Sources of an opener's context that have no recorded form. Each is a
/// presence flag: the source itself is live code or a live handle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnrecordedSessionSources {
    /// The turn's context overlay contributed tool providers.
    #[serde(default)]
    pub context_overlay_tools: bool,
    /// The session was opened with plugin factories of its own.
    #[serde(default)]
    pub open_plugins: bool,
    /// The session's plugins were forked from a parent session.
    #[serde(default)]
    pub fork_plugins: bool,
    /// The session was opened with a provider of its own.
    #[serde(default)]
    pub open_provider: bool,
    /// The session was opened with its own tool-source policy or tool-surface
    /// open mode.
    #[serde(default)]
    pub open_tool_policy: bool,
    /// The session's plugins hold mutable session state.
    #[serde(default)]
    pub plugin_state: bool,
}

impl UnrecordedSessionSources {
    /// Every source present in either.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        Self {
            context_overlay_tools: self.context_overlay_tools || other.context_overlay_tools,
            open_plugins: self.open_plugins || other.open_plugins,
            fork_plugins: self.fork_plugins || other.fork_plugins,
            open_provider: self.open_provider || other.open_provider,
            open_tool_policy: self.open_tool_policy || other.open_tool_policy,
            plugin_state: self.plugin_state || other.plugin_state,
        }
    }

    /// The first source a deployment-built context cannot reproduce.
    #[must_use]
    pub fn rebuild_refusal(&self) -> Option<ToolChildRebuildRefusal> {
        [
            (
                self.context_overlay_tools,
                ToolChildRebuildRefusal::ContextOverlayTools,
            ),
            (self.open_plugins, ToolChildRebuildRefusal::OpenPlugins),
            (self.fork_plugins, ToolChildRebuildRefusal::ForkPlugins),
            (self.open_provider, ToolChildRebuildRefusal::OpenProvider),
            (
                self.open_tool_policy,
                ToolChildRebuildRefusal::OpenToolPolicy,
            ),
            (self.plugin_state, ToolChildRebuildRefusal::PluginState),
        ]
        .into_iter()
        .find_map(|(present, refusal)| present.then_some(refusal))
    }
}

/// Why a tool child is refused the context that would serve it: something
/// the child could depend on exists only in its live opener, or the serving
/// context disagrees with what the opener recorded. The child is not run on
/// that context and not settled; it waits for its opener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolChildRebuildRefusal {
    ContextOverlayTools,
    OpenPlugins,
    ForkPlugins,
    OpenProvider,
    OpenToolPolicy,
    PluginState,
    /// The child wrote the session graph or read session state, which only
    /// its live opener's turn can serve.
    SessionServices,
    /// The context serving the child runs under another subagent context
    /// than its opener recorded, so its spawns would recurse to another
    /// depth.
    SubagentContext,
    /// More than one deployment's context source is live on this host, so
    /// which wiring would build the child's context is not determined.
    AmbiguousDeployment,
}

impl std::fmt::Display for ToolChildRebuildRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ContextOverlayTools => "its turn's context overlay contributed tools",
            Self::OpenPlugins => "its session was opened with plugin factories of its own",
            Self::ForkPlugins => "its session's plugins were forked from a parent session",
            Self::OpenProvider => "its session was opened with a provider of its own",
            Self::OpenToolPolicy => {
                "its session was opened with its own tool-source policy or open mode"
            }
            Self::PluginState => "its session's plugins hold mutable session state",
            Self::SessionServices => {
                "it reached the session's state or graph, which only its opener's turn serves"
            }
            Self::AmbiguousDeployment => {
                "more than one deployment's context source is live here, so none builds its context"
            }
            Self::SubagentContext => {
                "the context serving it runs under another subagent context than its opener recorded"
            }
        })
    }
}

impl ToolChildRebuildRefusal {
    /// The live fault a refused child ends its attempt with: retryable and
    /// never settled, so the engine runs it again, where its opener may be
    /// live.
    #[must_use]
    pub fn into_error(self, call_id: &str) -> super::super::executor::RuntimeEffectControllerError {
        super::super::executor::RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::PluginSessionManager,
            format!(
                "tool child `{call_id}` needs its live opener and waits for it: {self} \
                 (FIG-3712 rebuild refusal)"
            ),
        )
    }
}
