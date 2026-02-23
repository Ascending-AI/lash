use std::path::{Path, PathBuf};
use std::process::Command;

const PYTHON_VERSION: &str = "3.14.3";
const PYTHON_MAJOR_MINOR: &str = "3.14";
const PBS_RELEASE: &str = "20260211";

fn main() {
    println!("cargo:rerun-if-changed=python/repl.py");

    // ── Model info: download models.dev and generate context window lookup ──
    generate_model_info();

    // ── Python standalone: download, bundle, configure PyO3 ──
    let target = std::env::var("TARGET").unwrap();
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let target_dir = manifest_dir.parent().unwrap().join("target");
    let python_dir = target_dir.join("python-standalone");

    // 1. Ensure python-standalone is downloaded and extracted
    let install_dir = ensure_python_standalone(&python_dir, &target);

    // 2. Install dill into the stdlib when the target binary can be executed.
    let host = std::env::var("HOST").unwrap_or_default();
    if target == host {
        install_dill(&install_dir);
    } else {
        eprintln!("Cross-compiling for {target} (host: {host}), skipping dill install");
    }

    // 3. Generate pyo3-config.txt
    // PYO3_CONFIG_FILE must be set as a real env var for pyo3-ffi's build script.
    // build.rs `cargo:rustc-env` only affects our crate's compilation, not dependencies.
    // The user/CI must set PYO3_CONFIG_FILE env var, or we generate the config and
    // print its path so the next build with that env var works.
    let config_path = out_dir.join("pyo3-config.txt");
    generate_pyo3_config(&install_dir, &config_path, &target);

    // Also write to a well-known location for convenience
    let stable_config = python_dir.join("pyo3-config.txt");
    let _ = std::fs::copy(&config_path, &stable_config);

    if std::env::var("PYO3_CONFIG_FILE").is_err() {
        eprintln!(
            "NOTE: Set PYO3_CONFIG_FILE={} to use the standalone Python for static linking.",
            stable_config.display()
        );
        eprintln!("      Without it, PyO3 will link to your system Python.");
    }

    // 4. Bundle stdlib as tar.gz
    let stdlib_archive = out_dir.join("stdlib.tar.gz");
    bundle_stdlib(&install_dir, &stdlib_archive);

    // 5. Compute content hash for cache invalidation
    let hash = hash_file(&stdlib_archive);
    println!("cargo:rustc-env=LASH_PYTHON_STDLIB_HASH={}", hash);
    println!(
        "cargo:rustc-env=LASH_PYTHON_STDLIB_PATH={}",
        stdlib_archive.display()
    );

    // 6. Platform-specific linker flags
    if target.contains("linux") {
        // Static libpython deps — all C libraries that CPython's builtin modules need
        for lib in &[
            "z", "m", "pthread", "dl", "util", "readline", "expat", "zstd", "uuid", "sqlite3",
            "ncursesw", "panelw", "ffi", "lzma", "bz2", "mpdec", "ssl", "crypto",
        ] {
            println!("cargo:rustc-link-lib={lib}");
        }
    } else if target.contains("apple") {
        for lib in &[
            "z", "m", "pthread", "dl", "util", "readline", "expat", "zstd", "sqlite3", "ncurses",
            "panel", "ffi", "lzma", "bz2", "mpdec", "ssl", "crypto",
        ] {
            println!("cargo:rustc-link-lib={lib}");
        }
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
        println!("cargo:rustc-link-lib=framework=SystemConfiguration");
    }
}

/// Download and extract python-build-standalone if not cached.
fn ensure_python_standalone(python_dir: &Path, target: &str) -> PathBuf {
    let install_dir = python_dir.join("python").join("install");
    let marker = python_dir.join(".version");
    let flavor = pbs_flavor(target);

    let expected_version = format!("{PYTHON_VERSION}+{PBS_RELEASE}+{flavor}");
    if marker.exists()
        && let Ok(v) = std::fs::read_to_string(&marker)
        && v.trim() == expected_version
    {
        return install_dir;
    }

    eprintln!("Downloading python-build-standalone {PYTHON_VERSION}+{PBS_RELEASE} for {target}...");

    // Map Rust target triple to PBS triple
    let pbs_triple = map_target_triple(target);
    let filename = format!("cpython-{PYTHON_VERSION}+{PBS_RELEASE}-{pbs_triple}-{flavor}.tar.zst");
    let url = format!(
        "https://github.com/astral-sh/python-build-standalone/releases/download/{PBS_RELEASE}/{filename}"
    );

    // Clean and recreate
    let _ = std::fs::remove_dir_all(python_dir);
    std::fs::create_dir_all(python_dir).expect("Failed to create python-standalone dir");

    // Download
    let archive_path = python_dir.join(&filename);
    let status = Command::new("curl")
        .args(["-fSL", "--retry", "3", "-o"])
        .arg(&archive_path)
        .arg(&url)
        .status()
        .expect("Failed to run curl");
    assert!(
        status.success(),
        "Failed to download python-build-standalone from {url}"
    );

    // Decompress zstd
    let tar_path = python_dir.join("python.tar");
    let status = Command::new("zstd")
        .args(["-d", "-f"])
        .arg(&archive_path)
        .arg("-o")
        .arg(&tar_path)
        .status()
        .expect("Failed to run zstd — install zstd: apt install zstd / brew install zstd");
    assert!(status.success(), "zstd decompression failed");

    // Extract
    let status = Command::new("tar")
        .args(["xf"])
        .arg(&tar_path)
        .arg("-C")
        .arg(python_dir)
        .status()
        .expect("Failed to run tar");
    assert!(status.success(), "tar extraction failed");

    // Clean up archives
    let _ = std::fs::remove_file(&archive_path);
    let _ = std::fs::remove_file(&tar_path);

    // Remove shared libraries so static linking is used
    let lib_dir = install_dir.join("lib");
    for entry in std::fs::read_dir(&lib_dir)
        .expect("Failed to read lib dir")
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains("libpython") && name.contains(".so") {
            let _ = std::fs::remove_file(entry.path());
        }
    }

    // Write version marker
    std::fs::write(&marker, &expected_version).expect("Failed to write version marker");

    assert!(
        install_dir.exists(),
        "Expected install dir at {}",
        install_dir.display()
    );
    install_dir
}

/// Install dill into the standalone Python's site-packages.
fn install_dill(install_dir: &Path) {
    let site_packages = install_dir
        .join("lib")
        .join(format!("python{PYTHON_MAJOR_MINOR}"))
        .join("site-packages");
    let dill_marker = site_packages.join("dill");

    if dill_marker.exists() {
        return; // Already installed
    }

    eprintln!("Installing dill into standalone Python...");

    let python_bin = install_dir.join("bin").join("python3");
    let status = Command::new(&python_bin)
        .args(["-m", "pip", "install", "--target"])
        .arg(&site_packages)
        .args(["--no-deps", "--quiet", "dill"])
        .status()
        .unwrap_or_else(|e| panic!("Failed to run pip: {e}"));
    assert!(status.success(), "pip install dill failed");
}

/// Generate pyo3-config.txt for static linking.
fn generate_pyo3_config(install_dir: &Path, config_path: &Path, target: &str) {
    let lib_dir = install_dir.join("lib");
    let include_dir = install_dir
        .join("include")
        .join(format!("python{PYTHON_MAJOR_MINOR}"));

    let pointer_width = if target.contains("x86_64") || target.contains("aarch64") {
        "64"
    } else {
        "32"
    };

    let (mut shared, mut lib_name, mut build_flags) = default_python_link_settings(target);

    // If CI/user already supplied PYO3_CONFIG_FILE, keep our manual link directive aligned
    // with whatever PyO3 is using.
    if let Ok(pyo3_config_path) = std::env::var("PYO3_CONFIG_FILE")
        && let Ok(existing_config) = std::fs::read_to_string(&pyo3_config_path)
    {
        for line in existing_config.lines() {
            if let Some(v) = line.strip_prefix("shared=") {
                shared = v.trim() == "true";
            } else if let Some(v) = line.strip_prefix("lib_name=") {
                let v = v.trim();
                if !v.is_empty() {
                    lib_name = v.to_owned();
                }
            } else if let Some(v) = line.strip_prefix("build_flags=") {
                build_flags = v.trim().to_owned();
            }
        }
    }

    // Fall back between static/dylib if the requested artifact is unavailable.
    let static_lib = lib_dir.join(format!("lib{lib_name}.a"));
    let shared_lib = if target.contains("apple") {
        lib_dir.join(format!("lib{lib_name}.dylib"))
    } else {
        lib_dir.join(format!("lib{lib_name}.so"))
    };
    if shared && !shared_lib.exists() && static_lib.exists() {
        shared = false;
    } else if !shared && !static_lib.exists() && shared_lib.exists() {
        shared = true;
    }

    let config = format!(
        "implementation=CPython\n\
         version={PYTHON_MAJOR_MINOR}\n\
         shared={shared}\n\
         lib_name={lib_name}\n\
         lib_dir={lib_dir}\n\
         pointer_width={pointer_width}\n\
         build_flags={build_flags}\n\
         suppress_build_script_link_lines=true\n",
        shared = shared,
        lib_name = lib_name,
        lib_dir = lib_dir.display(),
        build_flags = build_flags,
    );

    // We link libpython ourselves with --whole-archive so that all CPython
    // symbols (PyCapsule_New, etc.) are included — ctypes.pythonapi needs them
    // exported from the binary.
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    // python-build-standalone keeps some static dependency libs (e.g. libmpdec.a)
    // under python/build/lib rather than install/lib.
    if let Some(python_root) = install_dir.parent() {
        let pbs_build_lib_dir = python_root.join("build").join("lib");
        if pbs_build_lib_dir.exists() {
            println!(
                "cargo:rustc-link-search=native={}",
                pbs_build_lib_dir.display()
            );
        }
    }
    if shared {
        println!("cargo:rustc-link-lib={lib_name}");
    } else {
        println!("cargo:rustc-link-lib=static:+whole-archive={lib_name}");
    }

    // Also set the include dir via env var for PyO3
    println!(
        "cargo:rustc-env=PYO3_CROSS_INCLUDE_DIR={}",
        include_dir.display()
    );

    std::fs::write(config_path, config).expect("Failed to write pyo3-config.txt");
}

/// Bundle the stdlib (lib/python3.14/ including site-packages/dill) as tar.gz.
fn bundle_stdlib(install_dir: &Path, archive_path: &Path) {
    if archive_path.exists() {
        // Check if the install dir is newer than the archive
        let archive_mtime = std::fs::metadata(archive_path)
            .and_then(|m| m.modified())
            .ok();
        let marker_mtime = install_dir
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join(".version"))
            .and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());

        if let (Some(am), Some(mm)) = (archive_mtime, marker_mtime)
            && am > mm
        {
            return; // Archive is newer than install, skip
        }
    }

    eprintln!("Bundling Python stdlib...");

    let lib_python = install_dir
        .join("lib")
        .join(format!("python{PYTHON_MAJOR_MINOR}"));

    // Use tar + gzip to create the archive
    let status = Command::new("tar")
        .args(["czf"])
        .arg(archive_path)
        .arg("-C")
        .arg(install_dir.join("lib"))
        .arg(format!("python{PYTHON_MAJOR_MINOR}"))
        // Also include lib-dynload .so files and any needed shared libs
        .status()
        .expect("Failed to create stdlib archive");
    assert!(status.success(), "Failed to bundle stdlib");

    // Verify
    let size = std::fs::metadata(archive_path)
        .map(|m| m.len())
        .unwrap_or(0);
    assert!(
        lib_python.exists(),
        "stdlib dir not found at {}",
        lib_python.display()
    );
    eprintln!("Bundled stdlib: {:.1} MB", size as f64 / 1_000_000.0);
}

/// Compute a hash of a file for cache invalidation.
fn hash_file(path: &Path) -> String {
    use std::io::Read;
    let mut file = std::fs::File::open(path).expect("Failed to open file for hashing");
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf).expect("Failed to read file");
        if n == 0 {
            break;
        }
        use std::hash::Hash;
        buf[..n].hash(&mut hasher);
    }
    use std::hash::Hasher as _;
    format!("{:016x}", hasher.finish())
}

/// Map Rust target triple to python-build-standalone triple.
fn map_target_triple(target: &str) -> &str {
    match target {
        "x86_64-unknown-linux-gnu" => "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu" => "aarch64-unknown-linux-gnu",
        "x86_64-apple-darwin" => "x86_64-apple-darwin",
        "aarch64-apple-darwin" => "aarch64-apple-darwin",
        _ => {
            // Try to use the target as-is
            eprintln!("Warning: unmapped target triple '{target}', using as-is for PBS download");
            target
        }
    }
}

fn pbs_flavor(target: &str) -> &str {
    match target {
        "x86_64-apple-darwin" | "aarch64-apple-darwin" => "debug-full",
        "x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu" => "pgo+lto-full",
        _ => {
            eprintln!("Warning: unmapped target triple '{target}', defaulting PBS flavor");
            "pgo+lto-full"
        }
    }
}

fn default_python_link_settings(target: &str) -> (bool, String, String) {
    if pbs_flavor(target) == "debug-full" {
        (
            true,
            format!("python{PYTHON_MAJOR_MINOR}d"),
            "Py_DEBUG".to_owned(),
        )
    } else {
        (
            false,
            format!("python{PYTHON_MAJOR_MINOR}"),
            "Py_GIL_DISABLED".to_owned(),
        )
    }
}

fn generate_model_info() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let cache_path = out_dir.join("models_dev_api.json");
    let generated_path = out_dir.join("model_info_generated.rs");

    // Skip download if cache is <24h old
    let need_download = if cache_path.exists() {
        std::fs::metadata(&cache_path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mtime| std::time::SystemTime::now().duration_since(mtime).ok())
            .map(|age| age.as_secs() > 86400)
            .unwrap_or(true)
    } else {
        true
    };

    if need_download {
        eprintln!("Downloading models.dev/api.json...");
        let status = Command::new("curl")
            .args(["-fsSL", "--retry", "2", "--max-time", "15", "-o"])
            .arg(&cache_path)
            .arg("https://models.dev/api.json")
            .status();
        match status {
            Ok(s) if s.success() => {}
            _ => {
                eprintln!("Warning: could not download models.dev/api.json, using fallback");
            }
        }
    }

    // Parse the JSON and build the match arms
    // Structure: { "provider_name": { "id": ..., "models": { "model_id": { "limit": { "context": N } } } } }
    let mut arms = Vec::new();
    if cache_path.exists()
        && let Ok(data) = std::fs::read_to_string(&cache_path)
        && let Ok(providers) = serde_json::from_str::<serde_json::Value>(&data)
        && let Some(obj) = providers.as_object()
    {
        for (provider, provider_info) in obj {
            let models = provider_info.get("models").and_then(|m| m.as_object());
            if let Some(models_obj) = models {
                for (model_id, info) in models_obj {
                    let ctx = info
                        .get("limit")
                        .and_then(|l| l.get("context"))
                        .and_then(|c| c.as_u64());
                    if let Some(context_limit) = ctx {
                        // Full qualified name: provider/model_id
                        let full_id = format!("{}/{}", provider, model_id);
                        arms.push(format!("        {:?} => Some({}),", full_id, context_limit));
                    }
                }
            }
        }
    }

    // If no arms from download, use hard-coded fallback
    if arms.is_empty() {
        arms = vec![
            r#"        "anthropic/claude-opus-4-6" => Some(200000),"#.to_string(),
            r#"        "anthropic/claude-sonnet-4-5-20250929" => Some(200000),"#.to_string(),
            r#"        "anthropic/claude-haiku-4-5-20251001" => Some(200000),"#.to_string(),
            r#"        "gpt-4o" => Some(128000),"#.to_string(),
            r#"        "openai/gpt-4o" => Some(128000),"#.to_string(),
            r#"        "gpt-4.1" => Some(1047576),"#.to_string(),
            r#"        "openai/gpt-4.1" => Some(1047576),"#.to_string(),
            r#"        "gemini-2.5-pro" => Some(1048576),"#.to_string(),
            r#"        "google/gemini-2.5-pro" => Some(1048576),"#.to_string(),
        ];
    }

    let code = format!(
        "/// Auto-generated by build.rs from models.dev/api.json\n\
         /// Returns the context window size for a given model ID.\n\
         pub fn context_window(model_id: &str) -> Option<u64> {{\n\
         \x20   match model_id {{\n\
         {}\n\
         \x20       _ => None,\n\
         \x20   }}\n\
         }}\n",
        arms.join("\n")
    );

    std::fs::write(&generated_path, code).expect("Failed to write model_info_generated.rs");
}
