//! Library functions by name.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc::{
    EncodeError, FunctionCatalog, FunctionDefinition, FunctionId, FunctionName, FunctionRegistry,
};

/// One function per name: the choice of identities a front end lowers
/// against.
///
/// The kernel names a function by identity and lets two definitions share a
/// name. A front end writes names, so it needs one answer per name; a
/// library is that answer, and replacing a function is replacing its entry.
pub trait Library: FunctionCatalog {
    /// The function `name` stands for.
    fn resolve(&self, name: &str) -> Option<FunctionId>;
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LibraryError {
    #[error("`{name}` is already defined as {function}")]
    NameTaken {
        name: FunctionName,
        function: FunctionId,
    },
    #[error(transparent)]
    Encode(#[from] EncodeError),
}

/// A [`Library`] held in memory.
#[derive(Clone, Debug, Default)]
pub struct NamedLibrary {
    by_name: BTreeMap<FunctionName, FunctionId>,
    definitions: BTreeMap<FunctionId, Arc<FunctionDefinition>>,
}

impl NamedLibrary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every function of a registry, by the name its definition carries,
    /// under the identity the registry holds it by.
    pub fn from_registry(registry: &FunctionRegistry) -> Result<Self, LibraryError> {
        let mut library = Self::new();
        for (function, registered) in registry.iter() {
            library.insert_identified(*function, Arc::clone(&registered.definition))?;
        }
        Ok(library)
    }

    /// Adds a function under its definition's name, which must be free.
    pub fn insert(
        &mut self,
        definition: impl Into<Arc<FunctionDefinition>>,
    ) -> Result<FunctionId, LibraryError> {
        let definition = definition.into();
        let function = definition.identity()?;
        self.insert_identified(function, definition)
    }

    fn insert_identified(
        &mut self,
        function: FunctionId,
        definition: Arc<FunctionDefinition>,
    ) -> Result<FunctionId, LibraryError> {
        if let Some(taken) = self.by_name.get(&definition.name) {
            return Err(LibraryError::NameTaken {
                name: definition.name.clone(),
                function: *taken,
            });
        }
        self.by_name.insert(definition.name.clone(), function);
        self.definitions.insert(function, definition);
        Ok(function)
    }

    /// Every function, ordered by name.
    pub fn iter(&self) -> impl Iterator<Item = (&FunctionName, &FunctionId)> {
        self.by_name.iter()
    }
}

impl FunctionCatalog for NamedLibrary {
    fn definition(&self, function: &FunctionId) -> Option<&FunctionDefinition> {
        self.definitions.get(function).map(Arc::as_ref)
    }
}

impl Library for NamedLibrary {
    fn resolve(&self, name: &str) -> Option<FunctionId> {
        // A name that is not a qualified name is no function's name.
        let name = FunctionName::new(name).ok()?;
        self.by_name.get(&name).copied()
    }
}
