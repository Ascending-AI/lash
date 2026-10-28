//! Project shared built-in rows once for helper generation and source lowering.
#[path = "build/builtins.rs"]
mod builtins;
#[expect(
    dead_code,
    reason = "the build projects indexes; helper sources and receiver tags are used by the library"
)]
#[path = "src/builtins/catalog.rs"]
mod catalog;

#[expect(
    clippy::disallowed_methods,
    reason = "the build writes its generated lookup table to OUT_DIR"
)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is unset")?);
    std::fs::write(out.join("builtin_table.rs"), builtins::generate())?;
    let primitive = include_str!("src/helpers/number_primitive.kernel");
    let (_, trim) = primitive
        .split_once("return text.trim_set(text, ")
        .ok_or("missing primitive trim declaration")?;
    let (literal, _) = trim
        .split_once(", leading, trailing)")
        .ok_or("missing primitive trim arguments")?;
    std::fs::write(
        out.join("primitive_facts.rs"),
        format!("pub const STRING_TRIM_SET: &str = {literal};\n"),
    )?;
    println!("cargo:rerun-if-changed=src/helpers/number_primitive.kernel");
    println!("cargo:rerun-if-changed=src/builtins");
    println!("cargo:rerun-if-changed=build/builtins.rs");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
