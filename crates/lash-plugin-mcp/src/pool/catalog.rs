//! Bounds apply to a complete catalog, including empty pages and cursors.

use std::collections::BTreeSet;
use std::io::{self, Write};

use rmcp::model::{PaginatedRequestParams, Tool};
use rmcp::service::{Peer, RoleClient};

use crate::error::McpError;

const MAX_CATALOG_PAGES: usize = 64;
const MAX_CATALOG_ITEMS: usize = 4096;
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;

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

pub(super) async fn discover_tools(peer: &Peer<RoleClient>) -> Result<Vec<Tool>, McpError> {
    let mut tools = Vec::new();
    let mut cursor = None;
    let mut cursors = BTreeSet::new();
    let mut bytes = CatalogByteBudget {
        remaining: MAX_CATALOG_BYTES,
    };
    for _ in 0..MAX_CATALOG_PAGES {
        let params = PaginatedRequestParams::default().with_cursor(cursor);
        let page = peer
            .list_tools(Some(params))
            .await
            .map_err(|error| McpError::Protocol(format!("MCP tools/list failed: {error}")))?;
        if page.tools.len() > MAX_CATALOG_ITEMS - tools.len() {
            return Err(McpError::Protocol(
                "MCP catalog item limit exceeded".to_string(),
            ));
        }
        // Count serialized bytes without allocating a second catalog copy.
        // Metadata and cursors consume the same aggregate budget as tools.
        serde_json::to_writer(&mut bytes, &page).map_err(|error| {
            McpError::Protocol(format!("MCP catalog byte limit exceeded: {error}"))
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
    Err(McpError::Protocol(
        "MCP catalog page limit exceeded".to_string(),
    ))
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
