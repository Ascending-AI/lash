//! Compile the patched module's own laws without enabling unrelated SDK test features.
mod model {
    pub type JsonObject = serde_json::Map<String, serde_json::Value>;
}

#[path = "rmcp-3.5.1.crate/src/transport/common/http_header.rs"]
mod http_header;
#[path = "rmcp-3.5.1.crate/src/transport/common/mcp_headers.rs"]
mod mcp_headers;
