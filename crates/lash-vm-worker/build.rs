//! Projects the source owners' native membership to static constructor tables.
use std::fmt::Write as _;

#[expect(
    clippy::disallowed_methods,
    reason = "the build writes its constructor tables to OUT_DIR"
)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut source = String::from(
        "static NATIVE_TABLES: &[&[(lash_kernel_doc::FunctionId, lash_kernel_doc::NativeCtor)]] = &[\n",
    );
    for package in lash_vm_releases::PACKAGES {
        if package.path != "kernel" {
            let feature = if package.path == "python" {
                "dialect-python"
            } else {
                "dialect-typescript"
            };
            writeln!(source, "#[cfg(feature = {feature:?})]")?;
        }
        source.push_str("&[\n");
        for (function, name) in package.natives {
            let mut callees = String::from("&[");
            for (callee, label) in package.native_callees(function)? {
                write!(
                    callees,
                    "(lash_kernel_doc::FunctionId::from_bytes({:?}), {:?}),",
                    callee.as_bytes(),
                    label
                )?;
            }
            callees.push(']');
            writeln!(
                source,
                "(lash_kernel_doc::FunctionId::from_bytes({:?}), |registry| crate::library::native({:?}, {:?}, lash_kernel_doc::ArtifactDigest::from_bytes({:?}), {callees}, registry)),",
                function.as_bytes(),
                package.path,
                name,
                package.built.as_bytes()
            )?;
        }
        source.push_str("],\n");
    }
    source.push_str("];\npub(crate) fn native_tables() -> &'static [&'static [(lash_kernel_doc::FunctionId, lash_kernel_doc::NativeCtor)]] { NATIVE_TABLES }\n");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is unset")?);
    std::fs::write(out.join("native_tables.rs"), source)?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
