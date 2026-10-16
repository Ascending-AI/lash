//! The library declarations a document uses, checked against it.
//!
//! A document's manifest lists every library function its code reaches, by
//! identity and name, through helper bodies too (`K-DOC-002`). A reader,
//! a store or a publisher that holds those definitions apart from any
//! registry holds exactly that set: each definition the function its
//! identity names, under the name the document uses it by, whose body calls
//! only functions the document lists as well.

use crate::{Document, FunctionCatalog, FunctionDeclarations, FunctionId, FunctionName};

/// Why a set of declarations is not the one a document uses.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DeclarationRefusal {
    #[error("the document uses `{name}` ({function}), and no declaration of it is held")]
    Missing {
        function: FunctionId,
        name: FunctionName,
    },
    #[error("declaration {function} is not one the document uses")]
    Unused { function: FunctionId },
    #[error("declaration {function} is the function {found}")]
    Identity {
        function: FunctionId,
        found: FunctionId,
    },
    #[error("declaration {function} is `{found}`, which the document uses as `{expected}`")]
    Name {
        function: FunctionId,
        expected: FunctionName,
        found: FunctionName,
    },
    #[error("declaration {function} calls {dependency}, which the document does not list")]
    Dependency {
        function: FunctionId,
        dependency: FunctionId,
    },
}

/// Checks that `declarations` are exactly the library functions `document`
/// lists: the same identities, each definition hashing to its own, named
/// as the document names it, its body's calls listed too.
///
/// # Errors
///
/// The first [`DeclarationRefusal`], in identity order.
pub fn check_declarations(
    document: &Document,
    declarations: &FunctionDeclarations,
) -> Result<(), DeclarationRefusal> {
    let listed = &document.manifest.functions;
    if let Some((function, name)) = listed
        .iter()
        .find(|(function, _)| !declarations.contains_key(function))
    {
        return Err(DeclarationRefusal::Missing {
            function: *function,
            name: name.clone(),
        });
    }
    for (function, definition) in declarations {
        let Some(expected) = listed.get(function) else {
            return Err(DeclarationRefusal::Unused {
                function: *function,
            });
        };
        let found = definition.identity();
        if found != *function {
            return Err(DeclarationRefusal::Identity {
                function: *function,
                found,
            });
        }
        if definition.name != *expected {
            return Err(DeclarationRefusal::Name {
                function: *function,
                expected: expected.clone(),
                found: definition.name.clone(),
            });
        }
        if let Some(dependency) = definition
            .body()
            .into_iter()
            .flat_map(|body| body.functions.keys())
            .find(|dependency| !listed.contains_key(dependency))
        {
            return Err(DeclarationRefusal::Dependency {
                function: *function,
                dependency: *dependency,
            });
        }
    }
    Ok(())
}

/// The declarations of the library functions `document` lists, taken from
/// `catalog` and checked as [`check_declarations`] checks them: the subset of
/// a registry's or a shipped export's definitions a holder keeps.
///
/// # Errors
///
/// The first [`DeclarationRefusal`].
pub fn required_declarations(
    document: &Document,
    catalog: &dyn FunctionCatalog,
) -> Result<FunctionDeclarations, DeclarationRefusal> {
    let mut held = FunctionDeclarations::new();
    for (function, name) in &document.manifest.functions {
        let Some(definition) = catalog.definition(function) else {
            return Err(DeclarationRefusal::Missing {
                function: *function,
                name: name.clone(),
            });
        };
        held.insert(*function, definition.clone());
    }
    check_declarations(document, &held)?;
    Ok(held)
}
