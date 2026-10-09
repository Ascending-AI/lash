//! The native-function interface and the function registry (`K-LIB-005`).
//!
//! `lash-kernel-lib` and the `lash-ext-*` crates implement [`NativeFunction`];
//! the embedder registers each implementation beside its definition in a
//! [`FunctionRegistry`] at startup and hands the registry to the machine.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::ControlFlow;
use std::sync::Arc;

use crate::canonical::EncodeError;
use crate::function::FunctionDefinition;
use crate::name::{FunctionId, FunctionName};
use crate::validate::{Invalid, validate_definition};
use crate::value::{ErrorValue, Object, ObjectId, Value};

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

/// How a native call ends without a value.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum NativeError {
    /// The function raises this error value: one of its declared kinds, or
    /// a kernel kind.
    #[error("{}: {}", .0.kind, .0.message)]
    Raised(ErrorValue),
    #[error(transparent)]
    Guard(#[from] GuardExceeded),
    /// The heap refused an allocation: the run's memory bound.
    #[error("the run's memory bound refused an allocation")]
    Memory,
}

/// One element of a heap object, as [`NativeHeap::visit`] yields it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Element<'a> {
    /// A list element or a set member.
    Item(&'a Value),
    /// A map entry.
    Entry { key: &'a Value, value: &'a Value },
    /// A record field.
    Field { name: &'a str, value: &'a Value },
}

/// What a native function sees of the run's heap: it reads the objects its
/// arguments name, reserves the room its result needs and allocates the
/// objects of its result. It cannot change an object that exists
/// (`K-LIB-007`).
///
/// An identity the heap does not hold, or one of another kind than the
/// method asks for, reads as empty.
pub trait NativeHeap {
    /// How many elements a list, entries a map, members a set or fields a
    /// record has.
    fn len(&self, object: ObjectId) -> usize;
    fn list_get(&self, list: ObjectId, index: usize) -> Option<Value>;
    /// The value under a key equal to `key` (`K-KEY-002`).
    fn map_get(&self, map: ObjectId, key: &Value) -> Option<Value>;
    fn set_contains(&self, set: ObjectId, member: &Value) -> bool;
    fn record_get(&self, record: ObjectId, field: &str) -> Option<Value>;
    /// Visits a list's elements by index, or a map's, set's or record's
    /// contents in insertion order, until the visitor breaks.
    fn visit(&self, object: ObjectId, visitor: &mut dyn FnMut(Element<'_>) -> ControlFlow<()>);
    /// Allocates a fresh list, map, set or record. A closure or a variable
    /// is refused, as is a map key or set member that is not a legal key,
    /// with a [`NativeError::Raised`] of kind `type_error`.
    fn allocate(&mut self, object: Object) -> Result<ObjectId, NativeError>;
    /// Reserves room for what the call is about to hold, as its result or
    /// as a temporary, out of what the run's memory bound leaves: `values`
    /// values (the elements of a list, the pieces a search keeps) and
    /// `bytes` bytes of text, bytes or integer digits. The heap prices
    /// them; how memory is counted is its own (`K-BND-003`). A refusal is
    /// [`NativeError::Memory`]; the function returns it and allocates
    /// nothing.
    ///
    /// A function whose result or temporary can outgrow its arguments (it
    /// follows a count, a product of two sizes or structure that is shared)
    /// reserves the size before it allocates it. Reservations add up over
    /// the call and end with it, and [`NativeHeap::allocate`] draws on
    /// them.
    fn reserve(&mut self, values: u64, bytes: u64) -> Result<(), NativeError>;
}

/// One call of a native function: its arguments, the heap they live in and
/// its work counter. That is all it sees.
pub struct NativeCall<'a> {
    /// One value per parameter of the signature; an omitted optional
    /// argument is [`Value::Absent`].
    pub args: &'a [Value],
    pub heap: &'a mut dyn NativeHeap,
    pub counter: &'a mut WorkCounter,
}

/// A native implementation of a library function.
///
/// It is a function of its arguments: it does no I/O, reads no clock, keeps
/// no state a second call could observe, and calls no guest code. Called
/// again with equal arguments it returns an equal result, raises the same
/// error, or passes its guard at the same count (`K-LIB-006`).
pub trait NativeFunction: Send + Sync {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError>;
}

/// Where a library function's definition is looked up by identity.
pub trait FunctionCatalog {
    fn definition(&self, function: &FunctionId) -> Option<&FunctionDefinition>;
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
}

/// The library functions an embedder runs with: each definition by
/// identity, with its native implementation where it has one. The embedder
/// assembles it in Rust at startup; nothing in a document adds to it.
#[derive(Clone, Debug, Default)]
pub struct FunctionRegistry {
    functions: BTreeMap<FunctionId, RegisteredFunction>,
}

impl FunctionRegistry {
    pub fn new() -> Self {
        Self::default()
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
        validate_definition(&definition, self).map_err(|source| RegistryError::Invalid {
            name: name.clone(),
            source,
        })?;
        match (
            definition.has_native(),
            definition.body().is_some(),
            &native,
        ) {
            (false, _, Some(_)) => return Err(RegistryError::NativeNotStated { name }),
            (true, false, None) => return Err(RegistryError::NativeMissing { name }),
            _ => {}
        }
        let function = definition.identity()?;
        if self.functions.contains_key(&function) {
            return Err(RegistryError::AlreadyRegistered { name, function });
        }
        self.functions.insert(
            function,
            RegisteredFunction {
                definition: Arc::new(definition),
                native,
            },
        );
        Ok(function)
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

impl FunctionCatalog for FunctionRegistry {
    fn definition(&self, function: &FunctionId) -> Option<&FunctionDefinition> {
        self.functions
            .get(function)
            .map(|registered| registered.definition.as_ref())
    }
}
