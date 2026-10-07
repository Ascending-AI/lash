//! MCP server guidance as an admitted catalog resource (ADR 0133).
//!
//! Every manifest imported from a service pins the server's initialize
//! instructions under [`MCP_GUIDANCE_KEY`], so the recorded catalog carries
//! its own guidance. A call renders the guidance of exactly the catalog it
//! was offered, a reopened session the guidance it recorded, and rendering
//! reads neither the server nor the pool.

use super::*;
use serde::{Deserialize, Serialize};

/// The manifest binding that pins a server's guidance.
pub(crate) const MCP_GUIDANCE_KEY: &str = "lash.mcp.guidance";

/// The guidance an imported manifest pins: its server and the server's
/// initialize instructions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PinnedGuidance {
    pub server: String,
    pub text: String,
}

impl PinnedGuidance {
    /// The guidance `manifest` pins, if its server gave any.
    pub(crate) fn of(manifest: &lash_core::ToolManifest) -> Option<Self> {
        serde_json::from_value(manifest.bindings.get(MCP_GUIDANCE_KEY)?.clone()).ok()
    }
}

/// Pin `peer`'s initialize instructions in every manifest of `tools`. A
/// server without instructions pins nothing.
pub(super) fn pin_guidance(
    tools: &mut BTreeMap<String, ImportedTool>,
    entry: &McpEntry,
    peer: &Peer<RoleClient>,
) -> Result<(), McpError> {
    let Some(instructions) = peer.peer_info().and_then(|info| info.instructions.clone()) else {
        return Ok(());
    };
    let text = instructions.trim();
    if text.is_empty() {
        return Ok(());
    }
    let pinned = serde_json::to_value(PinnedGuidance {
        server: entry.server_name.clone(),
        text: text.to_owned(),
    })
    .map_err(|error| McpError::Config(format!("cannot record MCP guidance: {error}")))?;
    for tool in tools.values_mut() {
        tool.definition
            .manifest
            .bindings
            .insert(MCP_GUIDANCE_KEY.into(), pinned.clone());
    }
    Ok(())
}

/// The `mcp` plugin's `server` section family: one section per offered
/// server that gave guidance, keyed `server.<server prefix>`, rendering the
/// guidance the offered manifests pin.
pub(crate) struct McpGuidanceSections;

impl McpGuidanceSections {
    pub(crate) const FAMILY: &str = "server";
}

impl lash_core::plugin::prompt::PromptSectionSource for McpGuidanceSections {
    fn sections(
        &self,
        offered: &lash_core::plugin::prompt::OfferedTools,
    ) -> Vec<lash_core::plugin::prompt::PromptFamilySection> {
        use lash_core::plugin::prompt::{
            PromptFamilySection, PromptInput, PromptSectionKey, SectionText,
        };
        let mut servers = BTreeMap::new();
        for pinned in offered.manifests().filter_map(PinnedGuidance::of) {
            servers.entry(pinned.server.clone()).or_insert(pinned);
        }
        servers
            .into_values()
            .filter_map(|pinned| {
                let suffix = PromptSectionKey::new(naming::server_prefix(&pinned.server)).ok()?;
                let text = format!("#### {}\n\n{}", pinned.server, pinned.text);
                Some(PromptFamilySection {
                    suffix,
                    renderer: Arc::new(move |_: &PromptInput<'_>| {
                        Ok(SectionText::Text(text.clone()))
                    }),
                })
            })
            .collect()
    }
}
