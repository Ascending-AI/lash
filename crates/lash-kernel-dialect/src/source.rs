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

use std::collections::BTreeSet;

use lash_kernel_doc::{
    Action, Callee, Expr, FunctionDefinition, FunctionId, Implementation, Invalid, Node,
    ParseError, parse_definition, validate_definition,
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
        reason: String,
    },
    #[error("`{function}` is invalid: {source}")]
    Invalid {
        function: String,
        #[source]
        source: Invalid,
    },
    #[error(transparent)]
    Library(#[from] LibraryError),
}

/// Reads every definition of `source`, adds each to `library` and returns
/// them in source order.
pub fn define_functions(
    source: &str,
    library: &mut NamedLibrary,
) -> Result<Vec<FunctionDefinition>, SourceError> {
    let (uses, chunks) = split(source)?;
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
        for name in &uses {
            // A name the library does not hold yet is one a later definition
            // gives; a body that calls it here fails to parse, naming it.
            if let Some(function) = library.resolve(name) {
                text.push_str(&format!("\nuse {name} = @{function}"));
            }
        }
        text.push('\n');
        let mut definition =
            parse_definition(&text).map_err(|error: ParseError| SourceError::Parse {
                function: label.clone(),
                line: error.line,
                column: error.column,
                reason: error.reason.to_string(),
            })?;
        keep_called(&mut definition);
        validate_definition(&definition, library).map_err(|source| SourceError::Invalid {
            function: label.clone(),
            source,
        })?;
        library.insert(definition.clone())?;
        definitions.push(definition);
    }
    Ok(definitions)
}

/// The names the header lists and the text of each definition.
fn split(source: &str) -> Result<(Vec<String>, Vec<String>), SourceError> {
    let mut uses = Vec::new();
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
                Some(name) => uses.push(name.trim().to_string()),
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
    Ok((uses, chunks))
}

/// Drops from a body's function list every function the body does not call:
/// the list is part of the identity and of every manifest that reaches it.
fn keep_called(definition: &mut FunctionDefinition) {
    let (Implementation::Body(body) | Implementation::Both(body)) = &mut definition.implementation
    else {
        return;
    };
    let mut called = BTreeSet::new();
    collect_calls(Node::Block(&body.block), &mut called);
    body.functions
        .retain(|function, _| called.contains(function));
}

fn collect_calls(node: Node<'_>, called: &mut BTreeSet<FunctionId>) {
    match node {
        Node::Expr(Expr::Call { function, .. }) => {
            called.insert(*function);
        }
        Node::Action(
            Action::Call {
                callee: Callee::Library(function),
                ..
            }
            | Action::Spawn {
                callee: Callee::Library(function),
                ..
            },
        ) => {
            called.insert(*function);
        }
        _ => {}
    }
    for child in node.children() {
        collect_calls(child, called);
    }
}
