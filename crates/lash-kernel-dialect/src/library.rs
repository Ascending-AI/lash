//! Library functions by name.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc::{
    FunctionCatalog, FunctionDefinition, FunctionId, FunctionName, FunctionRegistry, Manifest,
    RegisteredFunction, performed_by,
};

/// One function per name: the choice of identities a front end lowers
/// against.
///
/// The kernel names a function by identity and lets two definitions share a
/// name. A front end writes names, so it needs one answer per name; a
/// library is that answer, and replacing a function is replacing its entry.
pub trait Library: FunctionCatalog + Send + Sync {
    /// The function `name` stands for.
    fn resolve(&self, name: &str) -> Option<FunctionId>;
}

/// Lists in `manifest` what the bodies of the functions it lists perform,
/// each under what the body requires of it (`K-DOC-002`). An effect the
/// document's own code performs keeps the manifest's requirement: admission
/// refuses one name under two (`ADM-REQUIREMENT`).
pub fn require_through_bodies(manifest: &mut Manifest, catalog: &dyn FunctionCatalog) {
    for (effect, bodies) in performed_by(catalog, manifest.functions.keys().copied()) {
        if let Some(requirement) = bodies.into_values().next() {
            manifest
                .effects
                .entry(effect)
                .or_insert_with(|| requirement.clone());
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LibraryError {
    #[error("`{name}` is already defined as {function}")]
    NameTaken {
        name: FunctionName,
        function: FunctionId,
    },
}

/// A [`Library`] held in memory: a registry's functions by identity, the
/// names a front end resolves against, and the functions inserted beside
/// them. It shares the registry's definitions, decoding each only when it
/// is first read, and its name index (FIG-6843).
#[derive(Clone, Debug, Default)]
pub struct NamedLibrary {
    /// The functions held by identity.
    registry: FunctionRegistry,
    /// Which of the registry's names resolve.
    named: Named,
    /// The functions inserted by name beyond the registry's.
    inserted: BTreeMap<FunctionName, FunctionId>,
    definitions: BTreeMap<FunctionId, Arc<FunctionDefinition>>,
}

/// The registry functions a library resolves names against.
#[derive(Clone, Debug, Default)]
enum Named {
    /// None: the library holds no registry.
    #[default]
    None,
    /// Every one, each under a name no other has.
    All,
    /// Those admitted, by name: each one's place in the registry's name
    /// index ([`FunctionRegistry::names`]).
    Admitted(Vec<u32>),
}

impl NamedLibrary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every function of a registry, by the name its definition carries,
    /// under the identity the registry holds it by.
    pub fn from_registry(registry: &FunctionRegistry) -> Result<Self, LibraryError> {
        unique(registry, registry.names())?;
        Ok(Self {
            registry: registry.clone(),
            named: Named::All,
            ..Self::default()
        })
    }

    /// The functions of `registry` that `named` admits, each by the name
    /// its definition carries, over every function `registry` holds as the
    /// catalog. A front end resolves a name against the named functions
    /// alone, and still finds any function the registry holds by identity:
    /// a build that retains an earlier release's functions writes its own,
    /// while a saved function or a document written against the earlier
    /// ones is read as written (FIG-5799).
    pub fn resolving(
        registry: &FunctionRegistry,
        named: impl Fn(&FunctionId) -> bool,
    ) -> Result<Self, LibraryError> {
        let admitted: Vec<u32> = registry
            .names()
            .enumerate()
            .filter(|(_, (_, function))| named(function))
            .map(|(at, _)| at as u32)
            .collect();
        unique(
            registry,
            admitted
                .iter()
                .filter_map(|at| registry.name_at(*at as usize)),
        )?;
        Ok(Self {
            registry: registry.clone(),
            named: Named::Admitted(admitted),
            ..Self::default()
        })
    }

    /// Adds a function under its definition's name, which must be free.
    pub fn insert(
        &mut self,
        definition: impl Into<Arc<FunctionDefinition>>,
    ) -> Result<FunctionId, LibraryError> {
        let definition = definition.into();
        let function = definition.identity();
        if let Some(taken) = self.resolve(definition.name.as_str()) {
            return Err(LibraryError::NameTaken {
                name: definition.name.clone(),
                function: taken,
            });
        }
        self.inserted.insert(definition.name.clone(), function);
        self.definitions.insert(function, definition);
        Ok(function)
    }

    /// Every function a name resolves to, ordered by name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, FunctionId)> {
        let mut names: Vec<(&str, FunctionId)> = match &self.named {
            Named::None => Vec::new(),
            Named::All => self.registry.names().collect(),
            Named::Admitted(admitted) => admitted
                .iter()
                .filter_map(|at| self.registry.name_at(*at as usize))
                .collect(),
        };
        names.extend(
            self.inserted
                .iter()
                .map(|(name, function)| (name.as_str(), *function)),
        );
        names.sort_unstable();
        names.into_iter()
    }

    /// The registry function `name` resolves to.
    fn named(&self, name: &str) -> Option<FunctionId> {
        match &self.named {
            Named::None => None,
            Named::All => self.registry.named(name).next(),
            Named::Admitted(admitted) => {
                let entry = |at: &u32| self.registry.name_at(*at as usize);
                let start =
                    admitted.partition_point(|at| entry(at).is_some_and(|(found, _)| found < name));
                admitted
                    .get(start)
                    .and_then(entry)
                    .and_then(|(found, function)| (found == name).then_some(function))
            }
        }
    }
}

/// Refuses two of `names`, ordered by name, that share one: the first by
/// identity is taken.
fn unique<'a>(
    registry: &FunctionRegistry,
    names: impl Iterator<Item = (&'a str, FunctionId)>,
) -> Result<(), LibraryError> {
    let mut last: Option<(&str, FunctionId)> = None;
    for (name, function) in names {
        if let Some((previous, taken)) = last
            && previous == name
        {
            let name = registry
                .get(&function)
                .map(|registered| registered.definition().name.clone())
                .unwrap_or_else(|| unreachable!("a named function is registered"));
            return Err(LibraryError::NameTaken {
                name,
                function: taken,
            });
        }
        last = Some((name, function));
    }
    Ok(())
}

impl FunctionCatalog for NamedLibrary {
    fn registered(&self, function: &FunctionId) -> Option<&RegisteredFunction> {
        self.registry.get(function)
    }

    fn registration(&self, definition: &FunctionDefinition) -> Option<&RegisteredFunction> {
        self.registry.registration(definition)
    }

    fn definition(&self, function: &FunctionId) -> Option<&FunctionDefinition> {
        match self.definitions.get(function) {
            Some(definition) => Some(definition),
            None => self.registry.definition(function),
        }
    }
}

impl Library for NamedLibrary {
    fn resolve(&self, name: &str) -> Option<FunctionId> {
        // A name that is not a qualified name is no function's name: none
        // is registered or inserted under it.
        self.named(name).or_else(|| {
            if self.inserted.is_empty() {
                return None;
            }
            self.inserted.get(&FunctionName::new(name).ok()?).copied()
        })
    }
}
