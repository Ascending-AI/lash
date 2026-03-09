use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask error: {err}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        print_help();
        return Ok(());
    };

    match command.as_str() {
        "build" => run_build(args.collect()),
        "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        other => Err(format!("Unknown xtask command '{other}'")),
    }
}

fn run_build(raw_args: Vec<String>) -> Result<(), String> {
    let cargo_args = parse_build_args(raw_args)?;

    if !has_package_selection(&cargo_args) {
        let mut cargo_args = cargo_args;
        cargo_args.push("-p".into());
        cargo_args.push("lash-cli".into());
        return run_cargo_build(cargo_args);
    }

    run_cargo_build(cargo_args)
}

fn parse_build_args(raw_args: Vec<String>) -> Result<Vec<String>, String> {
    let mut cargo_args = Vec::new();

    let mut i = 0usize;
    while i < raw_args.len() {
        let arg = &raw_args[i];
        if arg == "--" {
            cargo_args.extend(raw_args.into_iter().skip(i + 1));
            break;
        }
        cargo_args.push(arg.clone());
        i += 1;
    }

    Ok(cargo_args)
}

fn run_cargo_build(cargo_args: Vec<String>) -> Result<(), String> {
    let root = workspace_root()?;
    let status = Command::new("cargo")
        .current_dir(&root)
        .arg("build")
        .args(&cargo_args)
        .status()
        .map_err(|e| format!("Failed to run cargo build: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build failed with status {status}"));
    }
    Ok(())
}

fn has_package_selection(args: &[String]) -> bool {
    args.iter().any(|arg| {
        arg == "-p"
            || arg == "--package"
            || arg.starts_with("--package=")
            || arg == "--workspace"
            || arg == "--all"
    })
}

fn workspace_root() -> Result<PathBuf, String> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "Failed to determine workspace root".to_string())
}

fn print_help() {
    println!(
        "cargo xtask build [-- <cargo build args>]\n\
         \n\
         Examples:\n\
         cargo xtask build\n\
         cargo xtask build -- --release\n\
         cargo xtask build -- -p lash-cli --target x86_64-unknown-linux-gnu"
    );
}
