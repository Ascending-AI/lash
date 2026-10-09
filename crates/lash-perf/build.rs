//! Capture the compiler that builds the harness, rather than a runtime PATH tool.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rustc = std::env::var_os("RUSTC").ok_or("build script requires RUSTC")?;
    let output = std::process::Command::new(rustc).arg("-vV").output()?;
    if !output.status.success() {
        return Err("compiler identity query failed".into());
    }
    let compiler = String::from_utf8(output.stdout)?.replace('\n', ";");
    println!("cargo:rustc-env=LASH_PERF_COMPILER={compiler}");
    let profile = ["PROFILE", "OPT_LEVEL", "DEBUG", "CARGO_ENCODED_RUSTFLAGS"]
        .map(|key| format!("{key}={}", std::env::var(key).unwrap_or_default()))
        .join(";");
    println!("cargo:rustc-env=LASH_PERF_BUILD_PROFILE={profile}");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
