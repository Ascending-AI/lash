//! A stand-in for the kernel library, until `lash-kernel-lib` exists.
//!
//! The helpers in `helpers/` call kernel library functions by name. The
//! library crate is not written yet, so this module states the names and
//! arities the helpers assume, as native definitions with no implementation
//! behind them. It lets the helpers be parsed and validated and a document
//! be lowered and admitted; nothing built on it runs. When the library
//! lands, its registry replaces this module and a renamed function is one
//! `use` line in a helper source.
//!
//! The lowerer calls some of these directly where it knows an operand's
//! type (`crate::types`). That is sound only while each takes exactly the
//! kinds its name says and raises `type_error` for any other, and while
//! `list.get(list, index)` reads an integral index from `0` up to the
//! list's length and raises `index_out_of_range` for any other, with no
//! counting from the end.

use lash_kernel_dialect::NamedLibrary;
use lash_kernel_doc::{FunctionDefinition, parse_definition};

/// The kernel library functions the dialect's helpers and lowerer call,
/// with the number of arguments each takes.
pub const KERNEL_FUNCTIONS: &[(&str, usize)] = &[
    ("kind", 1),
    ("same", 2),
    ("eq", 2),
    ("bool.not", 1),
    ("error.new", 3),
    ("num.add", 2),
    ("num.sub", 2),
    ("num.mul", 2),
    ("num.div", 2),
    ("num.rem_trunc", 2),
    ("num.pow", 2),
    ("num.neg", 1),
    ("num.lt", 2),
    ("num.le", 2),
    ("num.floor", 1),
    ("num.is_nan", 1),
    ("num.is_finite", 1),
    ("num.to_float", 1),
    ("num.to_text", 1),
    ("float.to_int", 1),
    ("int.rem_floor", 2),
    ("int.bit_and", 2),
    ("int.bit_or", 2),
    ("int.bit_xor", 2),
    ("int.shl", 2),
    ("int.shr", 2),
    ("text.concat", 2),
    ("text.len", 1),
    ("text.slice", 3),
    ("text.trim", 1),
    ("text.ends_with", 2),
    ("text.to_num", 1),
    ("text.lt_utf16", 2),
    ("text.code_points", 1),
    ("list.len", 1),
    ("list.get", 2),
    ("record.get", 2),
    ("record.has", 2),
    ("record.keys", 1),
    ("json.stringify", 1),
];

/// [`KERNEL_FUNCTIONS`] as native definitions, each taking and giving any
/// value.
#[expect(
    clippy::expect_used,
    reason = "the table is a constant, and every law of this crate builds the library from it"
)]
pub fn kernel_definitions() -> Vec<FunctionDefinition> {
    KERNEL_FUNCTIONS
        .iter()
        .map(|(name, arity)| {
            let params: Vec<String> = (0..*arity).map(|index| format!("p{index}: Any")).collect();
            let text = format!(
                "function {name}({}) -> Any\nkernel 1\ncharge 1\nnative\n",
                params.join(", ")
            );
            parse_definition(&text).expect("a stand-in definition is kernel text")
        })
        .collect()
}

/// A library that holds [`kernel_definitions`].
#[expect(
    clippy::expect_used,
    reason = "the table is a constant, and every law of this crate builds the library from it"
)]
pub fn kernel_library() -> NamedLibrary {
    let mut library = NamedLibrary::new();
    for definition in kernel_definitions() {
        library
            .insert(definition)
            .expect("each stand-in name is listed once");
    }
    library
}
