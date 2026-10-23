//! Bounds apply to a complete catalog, including empty pages and cursors.

use std::collections::BTreeSet;
use std::io::{self, Write};

use rmcp::model::{PaginatedRequestParams, Tool};
use rmcp::service::{Peer, RoleClient};

use crate::error::McpError;

#[cfg(test)]
const MAX_CATALOG_BYTES: usize = crate::config::McpDataLimits::standard().catalog_bytes;

struct CatalogByteBudget {
    remaining: usize,
}

impl Write for CatalogByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("MCP catalog byte limit exceeded"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) async fn discover_tools(
    peer: &Peer<RoleClient>,
    limits: crate::config::McpDataLimits,
) -> Result<Vec<Tool>, McpError> {
    let mut tools = Vec::new();
    let mut cursor = None;
    let mut cursors = BTreeSet::new();
    let mut bytes = CatalogByteBudget {
        remaining: limits.catalog_bytes,
    };
    for _ in 0..limits.catalog_pages {
        let params = PaginatedRequestParams::default().with_cursor(cursor);
        let page = peer
            .list_tools(Some(params))
            .await
            .map_err(|error| McpError::Protocol(format!("MCP tools/list failed: {error}")))?;
        if page.tools.len() > limits.catalog_items.saturating_sub(tools.len()) {
            return Err(McpError::Protocol(format!(
                "MCP catalog item limit {} exceeded",
                limits.catalog_items
            )));
        }
        // Count serialized bytes without allocating a second catalog copy.
        // Metadata and cursors consume the same aggregate budget as tools.
        serde_json::to_writer(&mut bytes, &page).map_err(|error| {
            McpError::Protocol(format!(
                "MCP catalog byte limit {} exceeded: {error}",
                limits.catalog_bytes
            ))
        })?;
        tools.extend(page.tools);
        cursor = page.next_cursor;
        let Some(next) = &cursor else {
            return Ok(tools);
        };
        if !cursors.insert(next.clone()) {
            return Err(McpError::Protocol(
                "MCP catalog cursor cycle detected".to_string(),
            ));
        }
    }
    Err(McpError::Protocol(format!(
        "MCP catalog page limit {} exceeded",
        limits.catalog_pages
    )))
}

impl super::McpConnectionPool {
    pub(crate) fn environment_mode(&self) -> lash_core_store::environment::SourceMode {
        // The pool itself notifies about membership and accepted reconnects,
        // even when individual servers only support explicit refresh.
        lash_core_store::environment::SourceMode::Notifying
    }
    pub fn server_source_descriptors(&self) -> Vec<lash_core_store::environment::SourceDescriptor> {
        use lash_core_store::environment::{SourceDescriptor, SourceId, SourceMode};
        use lash_protocol_types::sync::RwLockExt;
        self.catalog
            .read_recover()
            .servers
            .iter()
            .map(|(name, server)| SourceDescriptor {
                source: SourceId(format!("mcp:server:{name}")),
                mode: if server.notifying {
                    SourceMode::Notifying
                } else {
                    SourceMode::RefreshOnly
                },
            })
            .collect()
    }
    pub(crate) fn subscribe_environment(
        &self,
        source: &lash_core_store::environment::SourceId,
        sink: &lash_core::plugin::environment_sources::EnvironmentChangeSink,
    ) {
        use lash_protocol_types::sync::RwLockExt;
        let mut catalog = self.catalog.write_recover();
        catalog
            .subscribers
            .push((source.clone(), std::sync::Arc::downgrade(sink)));
    }
    /// Capture definitions, payloads and revision under their single owner.
    pub(crate) fn environment_snapshot(
        &self,
        key: &lash_vocabulary::providers::ToolProviderHandle,
        source: &lash_core_store::environment::SourceId,
        selection: &lash_tool_support::ToolSelection,
        namespace: Option<(
            &lash_vocabulary::package::PublicationDomain,
            &lash_vocabulary::package::PublicationScope,
        )>,
    ) -> lash_core_store::environment::SourceSnapshot {
        use lash_core_store::{
            environment::{EnvironmentFragment, SourceSnapshot},
            tool_surface::SurfaceEntry,
        };
        use lash_protocol_types::sync::RwLockExt;
        use lash_vocabulary::providers::ExecutionBinding;
        let catalog = self.catalog.read_recover();
        let tools = catalog.servers.iter().flat_map(|(server, installed)| {
            installed.tools.values().filter(|tool| selection.admits(&tool.definition.manifest.id)).map(move |tool| {
                let payload = serde_json::json!({"kind":"mcp", "server":server, "tool_id": tool.definition.manifest.id});
                SurfaceEntry::advertised(key.clone(), tool.definition.clone(), Some(ExecutionBinding {
                    codec: key.clone(), format: crate::plugin::MCP_BINDING_FORMAT, payload,
                }))
            })
        }).collect();
        let roots: std::collections::BTreeSet<_> = catalog
            .servers
            .keys()
            .map(|server| {
                lash_vocabulary::names::Segment::new(crate::naming::server_segment(server))
                    .unwrap_or_else(|_| unreachable!("canonical server root"))
            })
            .collect();
        let namespace_claims = namespace
            .map(|(domain, scope)| {
                roots
                    .into_iter()
                    .map(move |root| lash_vocabulary::package::PublicationContext {
                        domain: domain.clone(),
                        scope: scope.clone(),
                        publisher: lash_vocabulary::package::PublisherInstance(format!(
                            "{}/{}",
                            key.owner(),
                            key.key()
                        )),
                        root,
                    })
                    .collect()
            })
            .unwrap_or_default();
        SourceSnapshot {
            source: source.clone(),
            revision: catalog.revision.clone(),
            fragment: EnvironmentFragment {
                tools,
                namespace_claims,
                ..Default::default()
            },
        }
    }
    /// Explicit refresh for servers that cannot notify. This only requests work
    /// from their lifecycle owners; accepted catalogs publish normal revisions.
    pub fn refresh_catalogs(&self) {
        use lash_protocol_types::sync::RwLockExt;
        for entry in self.entries.read_recover().values() {
            if let Some(service) = entry.service_snapshot() {
                entry.request_tool_refresh(service.generation);
            }
        }
    }
}

#[cfg(test)]
type ObservedRevisions =
    std::sync::Arc<std::sync::Mutex<Vec<lash_core_store::environment::SourceRevision>>>;
#[cfg(test)]
pub(super) fn observe_environment(
    pool: &super::McpConnectionPool,
) -> (
    lash_core::plugin::environment_sources::EnvironmentChangeSink,
    ObservedRevisions,
) {
    use lash_protocol_types::sync::MutexExt;
    let revisions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = revisions.clone();
    let sink: lash_core::plugin::environment_sources::EnvironmentChangeSink =
        std::sync::Arc::new(move |change| {
            if let lash_core_store::environment::EnvironmentChange::SourceRevision {
                revision,
                ..
            } = change
            {
                recorded.lock_recover().push(revision);
            }
        });
    pool.subscribe_environment(&lash_core_store::environment::SourceId("mcp".into()), &sink);
    (sink, revisions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialized_catalog_byte_boundary_is_inclusive() {
        let mut page: rmcp::model::ListToolsResult = serde_json::from_value(serde_json::json!({
            "tools": [{"name":"boundary", "description":"", "inputSchema":{"type":"object"}}]
        }))
        .expect("page fixture");
        let overhead = serde_json::to_vec(&page).expect("serialized page").len();
        page.tools[0].description = Some("x".repeat(MAX_CATALOG_BYTES - overhead).into());
        let mut budget = CatalogByteBudget {
            remaining: MAX_CATALOG_BYTES,
        };
        serde_json::to_writer(&mut budget, &page).expect("exact byte boundary is accepted");
        assert_eq!(budget.remaining, 0);
        page.tools[0]
            .description
            .as_mut()
            .expect("description")
            .to_mut()
            .push('x');
        let mut budget = CatalogByteBudget {
            remaining: MAX_CATALOG_BYTES,
        };
        assert!(
            serde_json::to_writer(&mut budget, &page).is_err(),
            "one extra byte is refused"
        );
    }
}
