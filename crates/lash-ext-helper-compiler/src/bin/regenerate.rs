//! Writes each dialect's compiled plans and manifest where they are checked
//! in. Run from the repository root, or give it as the one argument.

#![allow(clippy::disallowed_methods, reason = "explicit source-data recorder")]

use std::path::PathBuf;
use std::process::ExitCode;

use lash_ext_helper_compiler::library::Dialect;

fn main() -> ExitCode {
    let root = std::env::args_os()
        .nth(1)
        .or_else(|| std::env::var_os("BUILD_WORKSPACE_DIRECTORY"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    for dialect in Dialect::ALL {
        let generated = match lash_ext_helper_compiler::generate(dialect) {
            Ok(generated) => generated,
            Err(error) => {
                eprintln!("{}: {error}", dialect.name());
                return ExitCode::FAILURE;
            }
        };
        let (rust, manifest) = lash_ext_helper_compiler::paths(dialect);
        for (path, text) in [(rust, &generated.rust), (manifest, &generated.manifest)] {
            if let Err(error) = std::fs::write(root.join(path), text) {
                eprintln!("{path}: {error}");
                return ExitCode::FAILURE;
            }
        }
        println!("{}: {} plans", dialect.name(), generated.planned);
    }
    ExitCode::SUCCESS
}
