//! TypeScript helper definitions and their built-in catalogue, without a source frontend.

pub mod builtins;
pub mod package;

pub fn numeric_name(name: &str) -> bool {
    let digits = name.strip_prefix('-').unwrap_or(name);
    matches!(name, "NaN" | "Infinity" | "-Infinity")
        || digits.as_bytes().first().is_some_and(u8::is_ascii_digit)
}

include!(concat!(env!("OUT_DIR"), "/primitive_facts.rs"));
