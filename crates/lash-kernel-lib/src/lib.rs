//! Dialect-free library definitions and deterministic native implementations.
//!
//! Numbers obey `K-NUM`, equality obeys `K-VAL-020` through `K-VAL-033`,
//! and map/set keys use the heap's canonical normal form (`K-KEY-001/002`).
//! The registry is assembled by the embedder; functions do no I/O and mutate
//! no existing object. See `docs/kernel/numbers.md` for the pinned edges,
//! domains, charges, and native work guard.

mod arithmetic;
mod brand;
mod comparison;
mod helpers;
pub use helpers::{KERNEL_NAME_FAMILIES, KernelHelper};
mod math;
mod numbers;
mod numeric;
mod text_json;

pub use brand::{implement_brands, register_brands};
pub use comparison::{compare, equal, same};
pub use numbers::{implement_numbers, numbers, register_numbers};
pub use numeric::{float_to_integer, integer_to_float};
pub use text_json::{
    decode_number, implement_text_json, parse_json, register_text_json, stringify_json, text_json,
};

#[cfg(test)]
mod memory_laws;
#[cfg(test)]
mod testsupport;

pub(crate) fn raised(
    kind: &'static str,
    message: impl Into<String>,
) -> lash_kernel_doc::NativeError {
    lash_kernel_doc::NativeError::refused(kind, message)
}

/// What a call answers when a buffer it reserved cannot be allocated:
/// room past every bound, which ends the run at its memory bound.
pub(crate) fn exhausted() -> lash_kernel_doc::NativeError {
    lash_kernel_doc::Retry::Room {
        wanted: lash_kernel_doc::heap::LogicalBytes(u64::MAX),
    }
    .into()
}

/// A text of at most `ShortText::MAX` bytes, inline: a fixed name.
#[expect(
    clippy::expect_used,
    reason = "callers pass fixed names no longer than an inline text"
)]
pub(crate) fn short(text: &str) -> lash_kernel_doc::Value {
    lash_kernel_doc::Value::Text(
        lash_kernel_doc::arena::ShortText::new(text).expect("a fixed name is inline"),
    )
}

#[cfg(test)]
mod corpus_tests;
#[cfg(test)]
mod prepared_tests;

mod collections;
pub use collections::{CollectionError, implement_collections, register_collections};

/// A pure native constructor selected by a build's identity table. The
/// definition catalogues remain the sole inventory of implemented functions.
pub fn native(name: &str) -> Option<std::sync::Arc<dyn lash_kernel_doc::NativeFunction>> {
    numbers::native(name)
        .or_else(|| text_json::native(name))
        .or_else(|| collections::native(name))
        .or_else(|| brand::native(name))
}
