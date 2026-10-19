//! Capture the compiler that builds the harness, rather than a runtime PATH tool.
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
    reason = "build provenance queries the actual compiler and build-script environment before the measurement binary exists"
)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rustc = std::env::var_os("RUSTC").ok_or("build script requires RUSTC")?;
    let output = std::process::Command::new(rustc).arg("-vV").output()?;
    if !output.status.success() {
        return Err("compiler identity query failed".into());
    }
    let compiler = String::from_utf8(output.stdout)?.replace('\n', ";");
    println!("cargo:rustc-env=KERNEL_PERF_COMPILER={compiler}");
    let profile = ["PROFILE", "OPT_LEVEL", "DEBUG", "CARGO_ENCODED_RUSTFLAGS"]
        .map(|key| format!("{key}={}", std::env::var(key).unwrap_or_default()))
        .join(";");
    println!("cargo:rustc-env=KERNEL_PERF_BUILD_PROFILE={profile}");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is unset")?);
    std::fs::write(out.join("builtin_table.rs"), builtins::generate())?;
    println!("cargo:rerun-if-changed=src/builtins");
    println!("cargo:rerun-if-changed=build/builtins.rs");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
