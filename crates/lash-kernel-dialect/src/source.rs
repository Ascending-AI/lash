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
//! Owners supply typed operation declarations with stable document-local slots.
//! Named performs resolve against those declarations before identity is assigned.

use lash_kernel_doc::{
    FunctionCatalog, FunctionDefinition, Implementation, Invalid, Node, ParseError,
    validate_definition,
};

use crate::library::{Library, LibraryError, NamedLibrary};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// Text other than comments and `use <name>` lines stands before the
    /// first definition.
    #[error("line {line}: expected `use <name>` or `function`, found `{found}`")]
    Header { line: usize, found: String },
    #[error("`{function}`, line {line} column {column} of its definition: {reason}")]
    Parse {
        function: String,
        line: u32,
        column: u32,
        reason: lash_kernel_doc::ParseErrorReason,
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
    operations: &[crate::authoring::HelperOperation],
) -> Result<Vec<FunctionDefinition>, SourceError> {
    define_functions_with(source, library, operations, &mut |_, _| Ok(()))
}

/// [`define_functions`], with `complete` given each definition as read,
/// before it is checked and joins `library`: a package builder adds the
/// argument checks a member's body begins with there.
pub fn define_functions_with(
    source: &str,
    library: &mut NamedLibrary,
    operations: &[crate::authoring::HelperOperation],
    complete: &mut dyn FnMut(&mut FunctionDefinition, &NamedLibrary) -> Result<(), String>,
) -> Result<Vec<FunctionDefinition>, SourceError> {
    let (header, chunks) = split(source)?;
    let imports = crate::authoring::operation_imports(operations).map_err(|reason| {
        SourceError::Complete {
            function: "<operations>".into(),
            reason,
        }
    })?;
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
        for name in &header.uses {
            if let Some(function) = library.resolve(name) {
                text.push_str(&format!("\nuse {name} = @{function}"));
            }
        }
        text.push('\n');
        let (mut definition, locals) = lash_kernel_doc::core_support::parse_definition_over(
            &text,
            imports.clone(),
            lash_vocabulary::data_limits::KernelDataLimits::standard(),
        )
        .map_err(|error: ParseError| SourceError::Parse {
            function: label.clone(),
            line: error.line,
            column: error.column,
            reason: error.reason,
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
