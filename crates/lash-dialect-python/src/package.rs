//! The functions the Python dialect ships.

use lash_kernel_dialect::{NamedLibrary, SourceError, define_functions};
use lash_kernel_doc::FunctionDefinition;

/// The helper source owners. Definitions are ordered by their call edges below.
const SOURCES: &[&str] = &[
    include_str!("helpers/unicode.kernel"),
    include_str!("helpers/core.kernel"),
    include_str!("helpers/statements.kernel"),
    include_str!("helpers/builtins.kernel"),
    include_str!("helpers/text_data.kernel"),
    include_str!("helpers/text.kernel"),
    include_str!("helpers/methods.kernel"),
    include_str!("helpers/format.kernel"),
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
    let attributes = crate::lower::methods::attribute_helpers();
    let sources: Vec<&str> = SOURCES
        .iter()
        .copied()
        .chain([attributes.as_str(), modules.as_str()])
        .collect();
    let mut uses = std::collections::BTreeSet::new();
    let mut pending = Vec::new();
    for source in sources {
        let mut chunks = source.split("\nfunction ");
        if let Some(header) = chunks.next() {
            uses.extend(
                header
                    .lines()
                    .filter(|line| line.starts_with("use "))
                    .map(str::to_owned),
            );
        }
        for chunk in chunks {
            let name = chunk
                .split('(')
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned();
            pending.push((name, format!("function {chunk}")));
        }
    }
    // Read edges once. Repeatedly rescanning source while sorting would make
    // the table-sized helpers turn library construction into cubic work.
    let names: Vec<_> = pending.iter().map(|(name, _)| name.clone()).collect();
    let mut pending: Vec<_> = pending
        .into_iter()
        .map(|(name, body)| {
            let dependencies: std::collections::BTreeSet<_> = names
                .iter()
                .filter(|dependency| {
                    **dependency != name && body.contains(&format!("{dependency}("))
                })
                .cloned()
                .collect();
            (name, body, dependencies)
        })
        .collect();
    let mut ready = std::collections::BTreeSet::new();
    let mut definitions = Vec::new();
    while !pending.is_empty() {
        let Some(position) = pending
            .iter()
            .position(|(_, _, dependencies)| dependencies.is_subset(&ready))
        else {
            return Err(SourceError::Complete {
                function: "<helpers>".into(),
                reason: "helper owners have a recursive dependency".into(),
            });
        };
        let (name, body, _) = pending.remove(position);
        let header = uses
            .iter()
            .filter(|line| body.contains(line.trim_start_matches("use ")))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        definitions.extend(define_functions(
            &format!("{header}\n{body}"),
            library,
            helper_operations(),
        )?);
        ready.insert(name);
    }
    Ok(definitions)
}

/// Typed service declarations shared by helper loading and runtime publication.
/// Slots are local to documents assembled by this dialect; authored imports
/// reserve them before allocation.
pub fn helper_operations() -> &'static [lash_kernel_dialect::authoring::HelperOperation] {
    &lash_dialect_services::PYTHON
}
