//! Neutral package, operation, identity and encoding vocabulary.
//! This crate has no first-party dependencies. Runtime executable objects and
//! registries stay above it. Old import paths are temporary branch re-exports.
pub mod claims;
pub mod content_ids;
pub mod data_limits;
pub mod identity;
pub mod json_schema;
pub mod legacy_capabilities;
pub mod legacy_names;
pub mod legacy_package_ids;
pub mod names;
pub mod operation_keys;
pub mod operation_text;
pub mod operations;
pub mod package;
pub mod providers;
pub mod schema;
pub mod schema_contract;
pub mod sites;
mod token_values;
pub mod tokens;
mod tool_catalog_error;
pub mod tool_contract;
pub mod tool_declaration;
pub mod tool_intents;
#[path = "tool_shape/lib.rs"]
pub mod tool_shape;
pub mod type_relations;
#[doc(hidden)]
pub mod type_text;
pub mod types;
pub use data_limits::KernelDataLimits;
pub use json_schema::*;
pub use legacy_names::{EffectName, InvalidIdentity, InvalidQualifiedName};
pub use names::*;
pub use operations::*;
pub use package::*;
pub use schema_contract::*;
pub use tool_catalog_error::ToolCatalogBuildError;
pub use tool_contract::*;
pub use tool_declaration::*;
pub use tool_intents::*;
pub use type_text::print_type;
pub use types::*;
/// BLAKE3 with the existing length-prefixed contract domain framing.
pub fn blake3_domain_hash_hex(domain: &str, bytes: impl AsRef<[u8]>) -> String {
    let mut h = blake3::Hasher::new();
    h.update(&(domain.len() as u64).to_be_bytes());
    h.update(domain.as_bytes());
    h.update(bytes.as_ref());
    h.finalize().to_hex().to_string()
}

#[doc(hidden)]
pub use legacy_names as name;

#[doc(hidden)]
pub use tokens as graph_tokens;

pub use identity::*;

pub mod ordered_pairs;

/// Tool declarations and their neutral schema/intent data closure.
pub mod tools {
    pub use crate::json_schema::*;
    pub use crate::schema_contract::*;
    pub use crate::tool_contract::*;
    pub use crate::tool_declaration::*;
    pub use crate::tool_intents::*;
}
