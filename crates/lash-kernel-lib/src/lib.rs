//! Dialect-free library definitions and deterministic native implementations.
//!
//! Numbers obey `K-NUM`, equality obeys `K-VAL-020` through `K-VAL-033`,
//! and map/set keys use the heap's canonical normal form (`K-KEY-001/002`).
//! The registry is assembled by the embedder; functions do no I/O and mutate
//! no existing object. See `docs/kernel/numbers.md` for the pinned edges,
//! domains, charges, and native work guard.

mod arithmetic;
mod comparison;
mod math;
mod numbers;
mod numeric;
mod text_json;

pub use comparison::{compare, equal, same};
pub use numbers::{numbers, register_numbers};
pub use numeric::{float_to_integer, integer_to_float};
pub use text_json::{decode_number, parse_json, register_text_json, stringify_json, text_json};

#[cfg(test)]
mod memory_laws;
#[cfg(test)]
mod testsupport;

pub(crate) fn raised(kind: &str, message: &str) -> lash_kernel_doc::NativeError {
    lash_kernel_doc::NativeError::Raised(std::sync::Arc::new(lash_kernel_doc::heap::ErrorRef::new(
        kind, message,
    )))
}

#[cfg(test)]
mod corpus_tests;
#[cfg(test)]
mod prepared_tests;

mod collections;
pub use collections::{CollectionError, register_collections};
