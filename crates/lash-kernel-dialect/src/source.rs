//! A dialect's helpers, written as kernel text with names in place of
//! identities.
//!
//! A helper source holds any number of definitions in kernel text. Before
//! the first `function` line it lists the functions its bodies call, one
//! `use <name>` per line and no identity. Each definition is read with the
//! identities the library gives those names at that point, keeps only the
//! ones its body calls, and joins the library, so a later definition may
//! call an earlier one. A function never calls itself: its identity covers
//! its body.
//!
//! The header may also state operations, one `operation <name>(<slots>) -> T` per line: a package's bodies perform them under that signature. Each definition lists only
//! the ones its body performs.

use lash_kernel_doc::{
    FunctionCatalog, FunctionDefinition, Implementation, Invalid, Node, ParseError,
    parse_definition_with_locals, validate_definition,
};

use crate::library::{Library, LibraryError, NamedLibrary};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// Text other than comments and `use <name>` lines stands before the
    /// first definition.
    #[error(
        "line {line}: expected `use <name>`, `operation <operation>` or `function`, found `{found}`"
    )]
    Header { line: usize, found: String },
    #[error("`{function}`, line {line} column {column} of its definition: {reason}")]
    Parse {
        function: String,
        line: u32,
        column: u32,
        reason: String,
    },
    #[error("`{function}` is invalid: {source}")]
    Invalid {
        function: String,
        #[source]
        source: Invalid,
    },
    /// What the caller adds to a definition could not be added.
    #[error("`{function}`: {reason}")]
    Complete { function: String, reason: String },
    #[error(transparent)]
    Library(#[from] LibraryError),
}

/// Reads every definition of `source`, adds each to `library` and returns
/// them in source order.
pub fn define_functions(
    source: &str,
    library: &mut NamedLibrary,
) -> Result<Vec<FunctionDefinition>, SourceError> {
    define_functions_with(source, library, &mut |_, _| Ok(()))
}

/// [`define_functions`], with `complete` given each definition as read,
/// before it is checked and joins `library`: a package builder adds the
/// argument checks a member's body begins with there.
pub fn define_functions_with(
    source: &str,
    library: &mut NamedLibrary,
    complete: &mut dyn FnMut(&mut FunctionDefinition, &NamedLibrary) -> Result<(), String>,
) -> Result<Vec<FunctionDefinition>, SourceError> {
    let (header, chunks) = split(source)?;
    let mut definitions = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let label = chunk
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("function "))
            .and_then(|rest| rest.split('(').next())
            .unwrap_or_default()
            .trim()
            .to_string();
        let mut text = chunk;
        for operation in &header.operations {
            let at = operation.find('(').ok_or_else(|| SourceError::Complete {
                function: label.clone(),
                reason: format!("operation `{operation}` has no signature"),
            })?;
            let path = &operation[..at];
            let operation_name = lash_vocabulary::operation_text::parse(path).map_err(|error| {
                SourceError::Complete {
                    function: label.clone(),
                    reason: error.to_string(),
                }
            })?;
            let _signature =
                lash_kernel_doc::parse_signature(&operation[at..]).map_err(|error| {
                    SourceError::Complete {
                        function: label.clone(),
                        reason: error.to_string(),
                    }
                })?;
            let name = lash_vocabulary::QualifiedName {
                package: operation_name.package,
                member: operation_name.name,
                type_member: None,
            };
            let slot =
                crate::authoring::service_slot(&name).ok_or_else(|| SourceError::Complete {
                    function: label.clone(),
                    reason: format!("{path} is not a shipped dialect service"),
                })?;
            text = text.replace(&format!("perform {path}("), &format!("perform {}(", slot.0));
            text.push_str(&format!("\nimport {} operation {operation}", slot.0));
        }
        for name in &header.uses {
            if let Some(function) = library.resolve(name) {
                text.push_str(&format!("\nuse {name} = @{function}"));
            }
        }
        text.push('\n');
        let (mut definition, locals) =
            parse_definition_with_locals(&text).map_err(|error: ParseError| {
                SourceError::Parse {
                    function: label.clone(),
                    line: error.line,
                    column: error.column,
                    reason: error.reason.to_string(),
                }
            })?;
        for local in locals {
            if library.definition(&local.identity()).is_none() {
                library.insert_local(local.clone());
                definitions.push(local);
            }
        }
        keep_called(&mut definition);
        keep_performed(&mut definition);
        complete(&mut definition, library).map_err(|reason| SourceError::Complete {
            function: label.clone(),
            reason,
        })?;
        validate_definition(&definition, library).map_err(|source| SourceError::Invalid {
            function: label.clone(),
            source,
        })?;
        library.insert(definition.clone())?;
        definitions.push(definition);
    }
    Ok(definitions)
}

/// What a source's header states before its first definition.
#[derive(Default)]
struct Header {
    /// The functions its bodies call, by name.
    uses: std::collections::BTreeSet<String>,
    /// The operations its bodies perform, each as its `operation` line states
    /// it after the keyword.
    operations: Vec<String>,
}

/// The header and the text of each definition.
fn split(source: &str) -> Result<(Header, Vec<String>), SourceError> {
    let mut header = Header::default();
    let mut chunks: Vec<String> = Vec::new();
    for (index, line) in source.lines().enumerate() {
        if line.starts_with("function ") {
            chunks.push(line.to_string());
        } else if let Some(chunk) = chunks.last_mut() {
            chunk.push('\n');
            chunk.push_str(line);
        } else {
            let text = line.split('#').next().unwrap_or_default().trim();
            if let Some(operation) = text.strip_prefix("operation ") {
                header.operations.push(operation.trim().to_string());
                continue;
            }
            match text.strip_prefix("use ") {
                Some(name) => {
                    header.uses.insert(name.trim().to_string());
                }
                None if text.is_empty() => {}
                None => {
                    return Err(SourceError::Header {
                        line: index + 1,
                        found: text.to_string(),
                    });
                }
            }
        }
    }
    Ok((header, chunks))
}

/// Drops from a body's operation list every operation the body does not
/// perform: a header states the operations of every definition it heads.
fn keep_performed(definition: &mut FunctionDefinition) {
    let (Implementation::Guest { body, .. } | Implementation::Both { body, .. }) =
        &mut definition.implementation
    else {
        return;
    };
    let used = crate::authoring::used_imports(&body.block);
    body.imports.retain(|slot, _| used.contains(slot));
}

/// Drops from a body's function list every function the body does not
/// hold: the list is part of the identity and of every document that
/// retains it.
fn keep_called(definition: &mut FunctionDefinition) {
    let (Implementation::Guest { body, .. } | Implementation::Both { body, .. }) =
        &mut definition.implementation
    else {
        return;
    };
    let held = Node::Block(&body.block).functions_within();
    body.functions.retain(|function, _| held.contains(function));
}
