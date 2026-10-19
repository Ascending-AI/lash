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
//! The header may also state operations, one `effect <name>(<slots>) -> T
//! contract "<id>"` per line: a package's bodies perform them under that
//! requirement. Each definition lists only the ones its body performs.

use lash_kernel_doc::{
    FunctionDefinition, Implementation, Invalid, Node, ParseError, parse_definition,
    validate_definition,
};

use crate::library::{Library, LibraryError, NamedLibrary};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// Text other than comments and `use <name>` lines stands before the
    /// first definition.
    #[error(
        "line {line}: expected `use <name>`, `effect <operation>` or `function`, found `{found}`"
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
        for effect in &header.effects {
            text.push_str(&format!("\neffect {effect}"));
        }
        for name in &header.uses {
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
    uses: Vec<String>,
    /// The operations its bodies perform, each as its `effect` line states
    /// it after the keyword.
    effects: Vec<String>,
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
            if let Some(effect) = text.strip_prefix("effect ") {
                header.effects.push(effect.trim().to_string());
                continue;
            }
            match text.strip_prefix("use ") {
                Some(name) => header.uses.push(name.trim().to_string()),
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
    let (Implementation::Body(body) | Implementation::Both(body)) = &mut definition.implementation
    else {
        return;
    };
    let mut performed = std::collections::BTreeSet::new();
    let mut pending = vec![Node::Block(&body.block)];
    while let Some(node) = pending.pop() {
        match node {
            Node::Action(lash_kernel_doc::Action::Perform { effect, .. }) => {
                performed.insert(effect.clone());
            }
            Node::Expr(lash_kernel_doc::Expr::Act(act)) => {
                if let lash_kernel_doc::ActKind::Perform { effect, .. } = &act.kind {
                    performed.insert(effect.clone());
                }
            }
            _ => {}
        }
        pending.extend(node.children());
    }
    body.effects.retain(|effect, _| performed.contains(effect));
}

/// Drops from a body's function list every function the body does not
/// hold: the list is part of the identity and of every manifest that
/// reaches it.
fn keep_called(definition: &mut FunctionDefinition) {
    let (Implementation::Body(body) | Implementation::Both(body)) = &mut definition.implementation
    else {
        return;
    };
    let held = Node::Block(&body.block).functions_within();
    body.functions.retain(|function, _| held.contains(function));
}
