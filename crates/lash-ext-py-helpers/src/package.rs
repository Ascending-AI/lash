//! The functions the Python dialect ships.

use lash_kernel_dialect::{NamedLibrary, SourceError, define_function_sources};
use lash_kernel_doc::FunctionDefinition;

/// The helper source owners. Definitions are ordered by their call edges below.
const SOURCES: &[&str] = &[
    include_str!("helpers/unicode.kernel"),
    include_str!("helpers/core.kernel"),
    include_str!("helpers/statements.kernel"),
    include_str!("helpers/callables.kernel"),
    include_str!("helpers/iterators.kernel"),
    include_str!("helpers/builtins.kernel"),
    include_str!("helpers/text_data.kernel"),
    include_str!("helpers/text.kernel"),
    include_str!("helpers/methods.kernel"),
    include_str!("helpers/format.kernel"),
    include_str!("helpers/values.kernel"),
    include_str!("helpers/async.kernel"),
    include_str!("helpers/json.kernel"),
];

/// Defines the dialect's helpers against `library`, which holds the kernel
/// library, adds them to it, and returns them in the order an embedder
/// registers them: each after every function its body calls.
pub fn define_helpers(library: &mut NamedLibrary) -> Result<Vec<FunctionDefinition>, SourceError> {
    // Compose all owners before ordering: shared value operations may call
    // text formatting, and bound members project the method catalogue.
    let modules = crate::pure_modules::source();
    let attributes = crate::methods::attribute_helpers();
    let sources: Vec<&str> = SOURCES
        .iter()
        .copied()
        .chain([attributes.as_str(), modules.as_str()])
        .collect();
    define_function_sources(&sources, library, helper_operations())
}

/// Typed service declarations shared by helper loading and runtime publication.
/// Slots are local to documents assembled by this dialect; authored imports
/// reserve them before allocation.
pub fn helper_operations() -> &'static [lash_kernel_dialect::authoring::HelperOperation] {
    &lash_kernel_services::PYTHON
}
