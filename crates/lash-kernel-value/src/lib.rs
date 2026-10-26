//! Names, types, immutable links and canonical value encodings shared by the kernel.
pub mod claims;
pub mod content_ids;
pub mod data_limits;
pub mod entry_schema;
pub mod identity;
pub mod json_schema;
pub mod names;
pub mod observation;
pub mod operation_keys;
pub mod operation_text;
pub mod operations;
pub mod package;
pub mod providers;
pub mod schema;
pub mod sites;
mod token_values;
pub mod tokens;
pub mod tool_contract;
pub mod tool_declaration;
#[path = "tool_shape/lib.rs"]
pub mod tool_shape;
pub mod type_relations;
pub mod type_text;
pub mod types;
pub use content_ids::*;
pub use data_limits::*;
pub use entry_schema::*;
pub use identity::*;
pub use json_schema::*;
pub use names::*;
pub use operations::*;
pub use package::*;
pub use tool_contract::*;
pub use tool_declaration::*;
pub use type_text::print_type;
pub use types::*;

/// Kernel-only access to the records opaque embedding identities retain.
#[doc(hidden)]
pub mod kernel_internals;
