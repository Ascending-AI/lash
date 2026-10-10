//! Defines the standard embedding's TypeScript helpers once, when the crate
//! is built (FIG-5796).
//!
//! Every worker is a process of its own, and defining the helpers (reading
//! their kernel text, validating and identifying each) took most of a
//! worker's startup. The script defines them against the library lash
//! ships, validates them in a registry holding it, and writes the result to
//! `OUT_DIR`. A worker registers them without defining them again.

#[path = "src/library.rs"]
mod library;

use lash_kernel_dialect::NamedLibrary;
use lash_kernel_doc::FunctionRegistry;

#[expect(
    clippy::disallowed_methods,
    reason = "a build script reads OUT_DIR and writes its one output there"
)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut registry = FunctionRegistry::new();
    library::register_kernel(&mut registry)?;
    library::register_extensions(&mut registry)?;
    let mut named = NamedLibrary::from_registry(&registry)?;
    let helpers = lash_dialect_typescript::define_helpers(&mut named)?;
    let validated = registry.validate_functions(helpers)?;
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is unset")?);
    std::fs::write(out.join("typescript_helpers.json"), validated.to_json()?)?;
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/library.rs");
    Ok(())
}
