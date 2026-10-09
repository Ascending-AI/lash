//! The functions the Python dialect ships.

use lash_kernel_dialect::{NamedLibrary, SourceError, define_functions};
use lash_kernel_doc::FunctionDefinition;

/// The helper sources, in the order their definitions depend on one
/// another.
const SOURCES: &[&str] = &[
    include_str!("helpers/core.kernel"),
    include_str!("helpers/builtins.kernel"),
    include_str!("helpers/methods.kernel"),
    include_str!("helpers/format.kernel"),
    include_str!("helpers/async.kernel"),
];

/// Defines the dialect's helpers against `library`, which holds the kernel
/// library, adds them to it, and returns them in the order an embedder
/// registers them: each after every function its body calls.
pub fn define_helpers(library: &mut NamedLibrary) -> Result<Vec<FunctionDefinition>, SourceError> {
    let mut definitions = Vec::new();
    for source in SOURCES {
        definitions.extend(define_functions(source, library)?);
    }
    Ok(definitions)
}
