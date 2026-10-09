//! Native functions and definitions for the dialect-free kernel library.
mod text_json;
pub use text_json::{decode_number, parse_json, register_text_json, stringify_json, text_json};
