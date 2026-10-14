//! The native-function interface and the function registry (`K-LIB-005`).
//!
//! `lash-kernel-lib` and the `lash-ext-*` crates implement [`NativeFunction`];
//! the embedder registers each implementation beside its definition in a
//! [`FunctionRegistry`] at startup and hands the registry to the machine.

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::canonical::EncodeError;
use crate::document::DecodeError;
use crate::function::FunctionDefinition;
use crate::heap::NativeFunction;
use crate::name::{FunctionId, FunctionName};
use crate::validate::{Invalid, validate_identified_definition};

/// The counter a native function is handed: it counts the function's guard
/// unit and refuses the unit that passes the limit.
///
/// The count depends only on the arguments. An implementation spends the
/// same units for the same arguments whether its caches are cold or warm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkCounter {
    limit: Option<u64>,
    spent: u64,
}

/// A native function spent more work than its guard allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the call passed its work limit of {limit}")]
pub struct GuardExceeded {
    pub limit: u64,
}

impl WorkCounter {
    /// A counter for one call. `None` is a function with no guard: it
    /// counts and never refuses.
    pub fn new(limit: Option<u64>) -> Self {
        Self { limit, spent: 0 }
    }

    /// Spends `units`, or refuses if that passes the limit.
    pub fn spend(&mut self, units: u64) -> Result<(), GuardExceeded> {
        let spent = self.spent.saturating_add(units);
        match self.limit {
            Some(limit) if spent > limit => Err(GuardExceeded { limit }),
            _ => {
                self.spent = spent;
                Ok(())
            }
        }
    }

    pub fn spent(&self) -> u64 {
        self.spent
    }

    pub fn limit(&self) -> Option<u64> {
        self.limit
    }
}

/// Where a library function's definition is looked up by identity.
pub trait FunctionCatalog: Send + Sync {
    fn definition(&self, function: &FunctionId) -> Option<&FunctionDefinition>;

    /// Registration's immutable facts, when this catalog holds registered code.
    fn registered(&self, _function: &FunctionId) -> Option<&RegisteredFunction> {
        None
    }

    /// Finds registration facts without identifying the definition again.
    /// Only the exact immutable definition held by the catalog may use them.
    fn registration(&self, _definition: &FunctionDefinition) -> Option<&RegisteredFunction> {
        None
    }
}

impl FunctionCatalog for BTreeMap<FunctionId, FunctionDefinition> {
    fn definition(&self, function: &FunctionId) -> Option<&FunctionDefinition> {
        self.get(function)
    }
}

/// A function as an embedder registered it.
#[derive(Clone)]
pub struct RegisteredFunction {
    pub definition: Arc<FunctionDefinition>,
    /// The native implementation, when the embedder supplied one.
    pub native: Option<Arc<dyn NativeFunction>>,
    function: FunctionId,
    callees: Arc<OnceLock<BTreeSet<FunctionId>>>,
    // Consumers own their derived types. Erasure keeps the document vocabulary
    // independent of the checker while registry clones share each result.
    derived: Arc<Mutex<DerivedFacts>>,
}

type DerivedFacts = BTreeMap<TypeId, Arc<dyn Any + Send + Sync>>;

impl RegisteredFunction {
    fn new(
        function: FunctionId,
        definition: FunctionDefinition,
        native: Option<Arc<dyn NativeFunction>>,
    ) -> Self {
        Self {
            definition: Arc::new(definition),
            native,
            function,
            callees: Arc::default(),
            derived: Arc::default(),
        }
    }

    pub fn identity(&self) -> FunctionId {
        self.function
    }

    /// The functions the body actually calls or spawns, including nested expressions.
    pub fn callees(&self) -> &BTreeSet<FunctionId> {
        self.callees.get_or_init(|| {
            let mut callees = BTreeSet::new();
            if let Some(body) = self.definition.body() {
                let mut pending = vec![crate::Node::Block(&body.block)];
                while let Some(node) = pending.pop() {
                    match node {
                        crate::Node::Expr(crate::Expr::Call { function, .. })
                        | crate::Node::Action(
                            crate::Action::Call {
                                callee: crate::Callee::Library(function),
                                ..
                            }
                            | crate::Action::Spawn {
                                callee: crate::Callee::Library(function),
                                ..
                            },
                        ) => {
                            callees.insert(*function);
                        }
                        _ => {}
                    }
                    pending.extend(node.children());
                }
            }
            callees
        })
    }

    /// Computes a consumer's facts once, against the immutable definitions
    /// registration resolved. Results (including refusals) survive registry clones.
    pub fn derive<T: Any + Send + Sync>(&self, derive: impl FnOnce() -> T) -> Arc<T> {
        let mut derived = self
            .derived
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = Arc::clone(
            derived
                .entry(TypeId::of::<T>())
                .or_insert_with(|| Arc::new(derive())),
        );
        // Each key is inserted only with its own concrete type.
        match result.downcast::<T>() {
            Ok(result) => result,
            Err(_) => unreachable!("derived facts keep their concrete type"),
        }
    }
}

impl fmt::Debug for RegisteredFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredFunction")
            .field("definition", &self.definition)
            .field("native", &self.native.is_some())
            .finish()
    }
}

/// A function the registry refuses.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("`{name}` is invalid: {source}")]
    Invalid {
        name: FunctionName,
        #[source]
        source: Invalid,
    },
    #[error(transparent)]
    Encode(#[from] EncodeError),
    /// The definition states no native implementation and one was supplied.
    #[error("`{name}` states no native implementation, so none can be registered for it")]
    NativeNotStated { name: FunctionName },
    /// The definition has only a native implementation and none was
    /// supplied.
    #[error("`{name}` has only a native implementation and none was supplied")]
    NativeMissing { name: FunctionName },
    #[error("`{name}` ({function}) is already registered")]
    AlreadyRegistered {
        name: FunctionName,
        function: FunctionId,
    },
    /// Validated functions were offered to a registry that does not hold
    /// exactly the functions they were validated against.
    #[error(
        "the functions were validated against {validated} functions, and this registry of {held} lacks some of them"
    )]
    OtherBasis { validated: usize, held: usize },
}

/// The library functions an embedder runs with: each definition by
/// identity, with its native implementation where it has one. The embedder
/// assembles it in Rust at startup; nothing in a document adds to it.
#[derive(Clone, Debug, Default)]
pub struct FunctionRegistry {
    functions: BTreeMap<FunctionId, RegisteredFunction>,
    /// The registry holds definitions alone ([`FunctionRegistry::declarations`]).
    declarations: bool,
}

impl FunctionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry of definitions alone: what an embedder that runs none of
    /// them links, admits and migrates documents against, such as a parent
    /// whose workers run its documents. A native definition is registered
    /// without its implementation, and a machine handed this registry
    /// cannot call it.
    pub fn declarations() -> Self {
        Self {
            functions: BTreeMap::new(),
            declarations: true,
        }
    }

    /// Registers a function and returns its identity.
    ///
    /// The definition is validated against what is already registered, so a
    /// function is registered after every function its body calls.
    pub fn register(
        &mut self,
        definition: FunctionDefinition,
        native: Option<Arc<dyn NativeFunction>>,
    ) -> Result<FunctionId, RegistryError> {
        let name = definition.name.clone();
        let identity =
            validate_identified_definition(&definition, self, crate::KernelDataLimits::standard())
                .map_err(|source| RegistryError::Invalid {
                    name: name.clone(),
                    source,
                })?;
        match (
            definition.has_native(),
            definition.body().is_some(),
            &native,
        ) {
            (false, _, Some(_)) => return Err(RegistryError::NativeNotStated { name }),
            (true, false, None) if !self.declarations => {
                return Err(RegistryError::NativeMissing { name });
            }
            _ => {}
        }
        let function = match identity {
            Some(function) => function,
            None => definition.identity()?,
        };
        if self.functions.contains_key(&function) {
            return Err(RegistryError::AlreadyRegistered { name, function });
        }
        self.functions.insert(
            function,
            RegisteredFunction::new(function, definition, native),
        );
        Ok(function)
    }

    /// Registers `definitions` in order, none with a native
    /// implementation, and returns them as validated against what the
    /// registry held before the first.
    ///
    /// # Errors
    ///
    /// The first definition [`FunctionRegistry::register`] refuses.
    pub fn validate_functions(
        &mut self,
        definitions: Vec<FunctionDefinition>,
    ) -> Result<ValidatedFunctions, RegistryError> {
        let basis = self.functions.keys().copied().collect();
        let mut functions = Vec::with_capacity(definitions.len());
        for definition in definitions {
            let function = self.register(definition.clone(), None)?;
            functions.push(ValidatedFunction {
                function,
                definition,
            });
        }
        Ok(ValidatedFunctions { basis, functions })
    }

    /// Registers functions [`FunctionRegistry::validate_functions`]
    /// validated against functions this registry holds, every one, without
    /// validating or identifying them again: validation and identity are
    /// functions of a definition and the functions it names, so a registry
    /// that holds more (another dialect's helpers) answers the same.
    ///
    /// The functions are trusted as given, so they must be a build's own
    /// work, as a worker takes the helpers its build assembled; a document
    /// or anything else from outside the build is registered with
    /// [`FunctionRegistry::register`].
    ///
    /// # Errors
    ///
    /// [`RegistryError::OtherBasis`] when the registry lacks a function
    /// they were validated against, and [`RegistryError::AlreadyRegistered`].
    pub fn register_validated(
        &mut self,
        validated: ValidatedFunctions,
    ) -> Result<(), RegistryError> {
        if !validated
            .basis
            .iter()
            .all(|function| self.functions.contains_key(function))
        {
            return Err(RegistryError::OtherBasis {
                validated: validated.basis.len(),
                held: self.functions.len(),
            });
        }
        for ValidatedFunction {
            function,
            definition,
        } in validated.functions
        {
            if self.functions.contains_key(&function) {
                return Err(RegistryError::AlreadyRegistered {
                    name: definition.name,
                    function,
                });
            }
            self.functions.insert(
                function,
                RegisteredFunction::new(function, definition, None),
            );
        }
        Ok(())
    }

    pub fn get(&self, function: &FunctionId) -> Option<&RegisteredFunction> {
        self.functions.get(function)
    }

    /// Every registered function, ordered by identity.
    pub fn iter(&self) -> impl Iterator<Item = (&FunctionId, &RegisteredFunction)> {
        self.functions.iter()
    }

    pub fn len(&self) -> usize {
        self.functions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.functions.is_empty()
    }
}

/// Functions validated against the contents of a registry, for another
/// registry holding exactly those contents to take without validating them
/// again ([`FunctionRegistry::register_validated`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatedFunctions {
    /// Every function the registry held when they were validated.
    basis: Vec<FunctionId>,
    /// The functions in the order they were registered, each with its
    /// identity.
    functions: Vec<ValidatedFunction>,
}

impl ValidatedFunctions {
    /// Each function's identity and definition, in the order they were
    /// registered.
    pub fn iter(&self) -> impl Iterator<Item = (&FunctionId, &FunctionDefinition)> {
        self.functions
            .iter()
            .map(|validated| (&validated.function, &validated.definition))
    }

    /// The JSON a build keeps them as. It is read back only by
    /// [`ValidatedFunctions::from_json`] in the same build, so it is the
    /// derived encoding, not a stored spelling.
    ///
    /// # Errors
    ///
    /// [`EncodeError`], which no definition raises.
    pub fn to_json(&self) -> Result<Vec<u8>, EncodeError> {
        serde_json::to_vec(self).map_err(|error| EncodeError {
            message: error.to_string(),
        })
    }

    /// Reads what [`ValidatedFunctions::to_json`] wrote. A body nests as
    /// deep as its definition does.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Invalid`].
    pub fn from_json(json: &[u8]) -> Result<Self, DecodeError> {
        let mut decoder = serde_json::Deserializer::from_slice(json);
        decoder.disable_recursion_limit();
        let invalid = |error: serde_json::Error| DecodeError::Invalid {
            message: error.to_string(),
            line: error.line(),
            column: error.column(),
        };
        let validated = Self::deserialize(&mut decoder).map_err(invalid)?;
        decoder.end().map_err(invalid)?;
        Ok(validated)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidatedFunction {
    function: FunctionId,
    definition: FunctionDefinition,
}

impl FunctionCatalog for FunctionRegistry {
    fn registered(&self, function: &FunctionId) -> Option<&RegisteredFunction> {
        self.get(function)
    }

    fn registration(&self, definition: &FunctionDefinition) -> Option<&RegisteredFunction> {
        self.functions
            .values()
            .find(|registered| std::ptr::eq(registered.definition.as_ref(), definition))
    }

    fn definition(&self, function: &FunctionId) -> Option<&FunctionDefinition> {
        self.functions
            .get(function)
            .map(|registered| registered.definition.as_ref())
    }
}
