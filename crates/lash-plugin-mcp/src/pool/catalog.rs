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
