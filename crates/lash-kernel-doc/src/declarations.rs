//! The library declarations a document uses, checked against it.
//!
//! A document's manifest lists every library function its code reaches, by
//! identity and name, through helper bodies too (`K-DOC-002`). A reader,
//! a store or a publisher that holds those definitions apart from any
//! registry holds exactly that set: each definition the function its
//! identity names, under the name the document uses it by, whose body calls
//! only functions the document lists as well.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::Document;
use crate::FunctionCatalog;
use crate::FunctionDeclarations;
use crate::FunctionDefinition;
use lash_vocabulary::names::FunctionId;
use lash_vocabulary::names::FunctionName;

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
    check_held(document, declarations, |_, definition| (definition, None))
}

/// [`check_declarations`] over `held`, where `declaration` answers each
/// held definition and, when its holder already took it, its identity: a
/// registry's identity is the one registration took, so a definition is
/// identified once however often a document draws it.
fn check_held<'a, D>(
    document: &Document,
    held: &'a BTreeMap<FunctionId, D>,
    declaration: impl Fn(&FunctionId, &'a D) -> (&'a FunctionDefinition, Option<FunctionId>),
) -> Result<(), DeclarationRefusal> {
    let listed: BTreeMap<_, _> = document
        .functions
        .iter()
        .map(|(id, f)| (*id, f.name.clone()))
        .collect();
    if let Some((function, name)) = listed
        .iter()
        .find(|(function, _)| !held.contains_key(function))
    {
        return Err(DeclarationRefusal::Missing {
            function: *function,
            name: name.clone(),
        });
    }
    for (function, held) in held {
        let Some(expected) = listed.get(function) else {
            return Err(DeclarationRefusal::Unused {
                function: *function,
            });
        };
        let (definition, identified) = declaration(function, held);
        let found = identified.unwrap_or_else(|| definition.identity());
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

/// Each library function `document` lists, as `catalog` holds it: a
/// registered definition with the identity registration took.
fn drawn<'a>(
    document: &'a Document,
    _catalog: &'a dyn FunctionCatalog,
) -> Result<BTreeMap<FunctionId, (&'a FunctionDefinition, Option<FunctionId>)>, DeclarationRefusal>
{
    let mut held = BTreeMap::new();
    for (function, local) in &document.functions {
        let declaration = (local, None);
        held.insert(*function, declaration);
    }
    check_held(document, &held, |_, declaration| *declaration)?;
    Ok(held)
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
    Ok(drawn(document, catalog)?
        .into_iter()
        .map(|(function, (definition, _))| (function, definition.clone()))
        .collect())
}

/// The declarations of the library functions a document lists, checked
/// against it, each with the token body it is stored as. A registered
/// definition's identity and body are the ones its registration holds, so
/// a holder that retains a document's helpers for every cell identifies
/// and encodes each of them once (FIG-6457).
#[derive(Clone, Debug)]
pub struct CheckedDeclarations {
    held: BTreeMap<FunctionId, CheckedDeclaration>,
}

#[derive(Clone, Debug)]
struct CheckedDeclaration {
    definition: Arc<FunctionDefinition>,
    image: Arc<[u8]>,
}

impl CheckedDeclarations {
    /// The declarations [`required_declarations`] takes from `catalog`, as
    /// it checks them, with their images.
    ///
    /// # Errors
    ///
    /// The first [`DeclarationRefusal`].
    pub fn required(
        document: &Document,
        catalog: &dyn FunctionCatalog,
    ) -> Result<Self, DeclarationRefusal> {
        let drawn = drawn(document, catalog)?;
        let mut held = BTreeMap::new();
        for (function, (definition, _)) in drawn {
            let declaration = match catalog.registered(&function) {
                Some(registered) => CheckedDeclaration {
                    definition: Arc::clone(registered.definition()),
                    image: registered.image(),
                },
                None => CheckedDeclaration {
                    definition: Arc::new(definition.clone()),
                    image: Arc::from(lash_vocabulary::tokens::encode_body(definition)),
                },
            };
            held.insert(function, declaration);
        }
        Ok(Self { held })
    }

    /// Checks that these are `document`'s declarations, by the identities
    /// they were checked under.
    ///
    /// # Errors
    ///
    /// The first [`DeclarationRefusal`], as [`check_declarations`] orders
    /// them.
    pub fn check(&self, document: &Document) -> Result<(), DeclarationRefusal> {
        check_held(document, &self.held, |function, declaration| {
            (declaration.definition.as_ref(), Some(*function))
        })
    }

    /// What running `document`, whose declarations these are, requires
    /// of a node by its code alone (FIG-6985): the native of each function
    /// it reaches that only a native implements, and its kernel version.
    /// A document lists every function it reaches, so these are its
    /// closure's.
    #[must_use]
    pub fn requirements(&self, document: &Document) -> lash_vocabulary::claims::NodeRequirements {
        lash_vocabulary::claims::NodeRequirements {
            natives: self
                .held
                .iter()
                .filter(|(_, declaration)| {
                    declaration.definition.has_native() && declaration.definition.body().is_none()
                })
                .map(|(function, _)| *function)
                .collect(),
            kernels: document.requires.kernels.clone(),
        }
    }

    /// Each declaration's function and the token body it is stored as, in
    /// identity order.
    #[must_use]
    pub fn images(&self) -> Vec<(FunctionId, Arc<[u8]>)> {
        self.held
            .iter()
            .map(|(function, declaration)| (*function, Arc::clone(&declaration.image)))
            .collect()
    }
}
