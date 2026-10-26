//! Host tool contracts, registry identities, claims and publication records.
//! Kernel values are imported directly from lash-kernel-value.
pub mod claims;
pub mod identity;
pub mod package;
pub use package::*;
pub mod providers;
pub mod schema_contract;
mod token_values;
pub mod tokens;
mod tool_catalog_error;
pub mod tool_contract;
pub mod tool_declaration;
pub mod tool_intents;
pub use schema_contract::*;
pub use tool_catalog_error::ToolCatalogBuildError;
pub use tool_contract::*;
pub use tool_declaration::*;
pub use tool_intents::*;

/// BLAKE3 with the existing length-prefixed contract domain framing.
pub fn blake3_domain_hash_hex(domain: &str, bytes: impl AsRef<[u8]>) -> String {
    let mut h = blake3::Hasher::new();
    h.update(&(domain.len() as u64).to_be_bytes());
    h.update(domain.as_bytes());
    h.update(bytes.as_ref());
    h.finalize().to_hex().to_string()
}

pub use identity::*;

pub mod ordered_pairs;

/// Tool declarations and their neutral schema/intent data closure.
pub mod tools {
    pub use crate::schema_contract::*;
    pub use crate::tool_contract::*;
    pub use crate::tool_declaration::*;
    pub use crate::tool_intents::*;
}

mod json_value_match;
pub use json_value_match::matches as matches_json;
pub mod shape_names;
