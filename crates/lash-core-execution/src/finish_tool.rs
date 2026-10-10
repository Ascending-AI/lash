//! The finish tool: a control tool whose call ends its caller's turn with
//! its whole input as the turn's value.
//!
//! A turn's final value has the type of the tool that ended it. A tool
//! declares that type on its control ([`TurnControls::finish`]), and
//! settlement checks every Finish it emits against it. [`finish_tool`] is
//! the common case, where the value is the input: the input schema and the
//! value schema are the same schema. A host that wants a typed answer
//! offers one through [`FinishToolProvider`]; a tool whose body transforms
//! its input or does work before it finishes declares its own control
//! instead, with [`ToolDefinition::control`].
//!
//! The provider runs every tool it holds as the identity, by id, whatever
//! schema the call was admitted under. A run that offers the same tool
//! with another schema (`SendBuilder::tool_access` with the tool's
//! definition rebuilt by [`finish_tool`]) therefore changes the shape for
//! that run alone: admission records the run's definition, and settlement
//! checks against it.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    ExecutionPolicy, JsonSchema, ToolCall, ToolContract, ToolDefinition, ToolManifest, ToolOutcome,
    ToolProvider, TurnControls,
};

/// The id [`finish_tool`] gives the tool named `name`.
#[must_use]
pub fn finish_tool_id(name: &str) -> crate::ToolId {
    crate::ToolId::from(format!("tool:{name}"))
}

/// A finish tool named `name`: its call ends the turn with its whole input
/// as the turn's value, so its input and its value are both `value_schema`.
/// It returns nothing. Its body reads its argument and nothing else, so a
/// call a crash cut off runs again on the owner that resumes it; a value
/// its schema refuses is never run again.
#[must_use]
#[expect(
    clippy::expect_used,
    reason = "an admitted schema is an admitted input schema"
)]
pub fn finish_tool(name: impl Into<String>, value_schema: JsonSchema) -> ToolDefinition {
    let name = name.into();
    ToolDefinition::control(
        finish_tool_id(&name),
        name,
        "End the turn with this call's input as its answer. It returns nothing, and nothing after the call runs: call it once every other piece of work has finished.",
        value_schema.as_value().clone(),
        TurnControls::finish(value_schema),
    )
    .expect("an admitted schema is an admitted input schema")
    // No work of its own: the call is its control.
    .with_execution(std::time::Duration::from_secs(30))
    .with_execution_policy(ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(3).expect("three is non-zero"),
        100,
        1_000,
    ))
}

/// Provides finish tools ([`finish_tool`]): each call ends the turn with
/// its whole input. It answers by tool id, so the schema a run offers the
/// tool under is the one its call is admitted and settled under.
pub struct FinishToolProvider {
    tools: Vec<ToolDefinition>,
}

impl FinishToolProvider {
    /// A provider of `tools`, each a finish tool.
    #[must_use]
    pub fn new(tools: impl IntoIterator<Item = ToolDefinition>) -> Self {
        Self {
            tools: tools.into_iter().collect(),
        }
    }
}

#[async_trait]
impl ToolProvider for FinishToolProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        self.tools.iter().map(ToolDefinition::manifest).collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        self.tools
            .iter()
            .find(|tool| tool.manifest.name == name)
            .map(|tool| Arc::new(tool.contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        if !self
            .tools
            .iter()
            .any(|tool| &tool.manifest.id == call.tool_id())
        {
            return ToolOutcome::err_fmt(format_args!("Unknown tool: {}", call.name())).into();
        }
        // The value is the whole input: settlement checks it against the
        // schema the call was admitted under.
        ToolOutcome::finish(call.args.clone()).into()
    }
}

/// What a protocol's catalog leaves out so that a host's own finish tool
/// takes the place of the protocol's default one, `default`: the default,
/// when any other tool the surface offers declares a Finish. The surface is
/// the run's restricted definitions when its tool access restricts, less
/// the tools the access hides. A host that constrains a turn's answer with
/// a typed finish tool is never undercut by an untyped one beside it.
#[must_use]
pub fn suppress_default_finish(
    ctx: &crate::plugin::ToolCatalogContext,
    default: &crate::ToolId,
) -> crate::plugin::ToolCatalogContribution {
    let surface: Vec<ToolManifest> = match ctx.tool_access.restricted_tools() {
        Some(definitions) => definitions.iter().map(ToolDefinition::manifest).collect(),
        None => ctx.tools.clone(),
    };
    let offered = |tool: &&ToolManifest| !ctx.tool_access.hides(&tool.name);
    let Some(default_tool) = surface
        .iter()
        .filter(offered)
        .find(|tool| &tool.id == default)
    else {
        return crate::plugin::ToolCatalogContribution::default();
    };
    let host_finish = surface.iter().filter(offered).any(|tool| {
        &tool.id != default
            && tool
                .declaration()
                .controls
                .contains(crate::TurnControlKind::Finish)
    });
    if host_finish {
        crate::plugin::ToolCatalogContribution::remove_tools([default_tool.name.clone()])
    } else {
        crate::plugin::ToolCatalogContribution::default()
    }
}
