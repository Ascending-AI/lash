//! Integrator seam: registry definitions stay typed for library construction
//! and linking; they are not the host document retention format.
//!
//! The native-function interface and the function registry (`K-LIB-005`).
//!
//! `lash-kernel-lib` and the `lash-ext-*` crates implement [`NativeFunction`];
//! the embedder registers each implementation beside its definition in a
//! [`FunctionRegistry`] at startup and hands the registry to the machine.

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use crate::function::FunctionDefinition;
use crate::heap::NativeFunction;
use crate::name::{FunctionId, FunctionName};
use crate::plan::HelperPlan;
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
    definition: Arc<FunctionDefinition>,
    /// The native implementation, when the embedder supplied one.
    native: Option<Arc<dyn NativeFunction>>,
    /// The body compiled ahead of time, when the build generated one.
    plan: Option<&'static HelperPlan>,
    function: FunctionId,
    callees: Arc<OnceLock<BTreeSet<FunctionId>>>,
    /// The definition's token body, encoded once for every holder that
    /// stores it.
    image: Arc<OnceLock<Arc<[u8]>>>,
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
            plan: None,
            function,
            callees: Arc::default(),
            image: Arc::default(),
            derived: Arc::default(),
        }
    }

    /// The immutable definition whose identity and derived facts were registered.
    pub fn definition(&self) -> &Arc<FunctionDefinition> {
        &self.definition
    }

    /// The registered native implementation, when one was supplied.
    pub fn native(&self) -> Option<&Arc<dyn NativeFunction>> {
        self.native.as_ref()
    }

    /// The body's generated plan, when one is installed.
    pub fn plan(&self) -> Option<&'static HelperPlan> {
        self.plan
    }

    pub fn identity(&self) -> FunctionId {
        self.function
    }

    /// The definition's token body ([`crate::graph_tokens::encode_body`]),
    /// the bytes its checkpoint graph node is encoded from, encoded once per
    /// registration and shared by the registry's clones.
    pub fn image(&self) -> Arc<[u8]> {
        Arc::clone(
            self.image
                .get_or_init(|| Arc::from(crate::graph_tokens::encode_body(&*self.definition))),
        )
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
                        crate::Node::Expr(crate::Expr::Act(act)) => match &act.kind {
                            crate::ActKind::Call(crate::Callee::Library(function))
                            | crate::ActKind::Spawn(crate::Callee::Library(function))
                            | crate::ActKind::Await(function) => {
                                callees.insert(*function);
                            }
                            _ => {}
                        },
                        crate::Node::Stmt(crate::Stmt::ReturnVia { policy, .. }) => {
                            callees.insert(*policy);
                        }
                        crate::Node::Stmt(crate::Stmt::For { via: Some(via), .. }) => {
                            callees.extend(via.functions());
                        }
                        _ => {}
                    }
                    pending.extend(node.children());
                }
            }
            callees
        })
    }
}

/// Internal memoization shared by admission consumers, outside the host vocabulary.
pub trait RegisteredFunctionCoreSupport {
    fn derive<T: Any + Send + Sync>(&self, derive: impl FnOnce() -> T) -> Arc<T>;
}

#[doc(hidden)]
impl RegisteredFunctionCoreSupport for RegisteredFunction {
    /// Computes a consumer's facts once, against the immutable definitions
    /// registration resolved. Results (including refusals) survive registry clones.
    fn derive<T: Any + Send + Sync>(&self, derive: impl FnOnce() -> T) -> Arc<T> {
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
            .field("plan", &self.plan.is_some())
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
    /// A native implementation was offered for a function the registry
    /// does not hold.
    #[error("{function} is not registered, so no native implementation can be added to it")]
    NotRegistered { function: FunctionId },
    /// A native implementation was offered for a function that has one.
    #[error("`{name}` ({function}) already has a native implementation")]
    AlreadyImplemented {
        name: FunctionName,
        function: FunctionId,
    },
    /// A plan was offered for a function whose definition has no kernel
    /// body.
    #[error("`{name}` has no kernel body a plan could perform")]
    PlanWithoutBody { name: FunctionName },
    /// A plan was offered for a function that has one.
    #[error("`{name}` ({function}) already has a plan")]
    AlreadyPlanned {
        name: FunctionName,
        function: FunctionId,
    },
    /// A plan calls a function the registry does not hold, or one that
    /// would run its body in a frame, outside a callback site.
    #[error("the plan of `{name}` calls {callee}, which neither a plan nor a native answers")]
    PlanCalleeUnanswered {
        name: FunctionName,
        callee: FunctionId,
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
            None => definition.identity(),
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

    /// Adds the native implementation of a registered function whose
    /// definition states one beside its kernel body: an operation's
    /// partial native, which answers the inputs it can and declines the
    /// rest to the body (`NativeError::Decline`). A dialect's helpers are
    /// registered as their build validated them, and the embedder adds
    /// their natives afterwards.
    ///
    /// # Errors
    ///
    /// [`RegistryError::NotRegistered`], [`RegistryError::NativeNotStated`]
    /// and [`RegistryError::AlreadyImplemented`].
    pub fn implement(
        &mut self,
        function: FunctionId,
        native: Arc<dyn NativeFunction>,
    ) -> Result<(), RegistryError> {
        let registered = self
            .functions
            .get_mut(&function)
            .ok_or(RegistryError::NotRegistered { function })?;
        if !registered.definition.has_native() {
            return Err(RegistryError::NativeNotStated {
                name: registered.definition.name.clone(),
            });
        }
        if registered.native.is_some() {
            return Err(RegistryError::AlreadyImplemented {
                name: registered.definition.name.clone(),
                function,
            });
        }
        registered.native = Some(native);
        Ok(())
    }

    /// Installs the plans a build generated, each beside the definition
    /// it was generated from, by identity. A plan whose function is not
    /// registered is skipped: the registry holds another dialect's or
    /// another release's helpers. Every callee a plan calls outside its
    /// callback sites must answer at once: a native without a body, a
    /// machine function, or an installed plan that has no callback site.
    /// When a native beside a plan declines, that plan executes the
    /// callee's body without offering the native the call again.
    ///
    /// # Errors
    ///
    /// [`RegistryError::PlanWithoutBody`], [`RegistryError::AlreadyPlanned`]
    /// and [`RegistryError::PlanCalleeUnanswered`].
    pub fn install_plans(&mut self, plans: &[&'static HelperPlan]) -> Result<usize, RegistryError> {
        let mut installing = BTreeMap::new();
        for plan in plans {
            let Some(registered) = self.functions.get(&plan.function) else {
                continue;
            };
            if registered.definition.body().is_none() {
                return Err(RegistryError::PlanWithoutBody {
                    name: registered.definition.name.clone(),
                });
            }
            if registered.plan.is_some() || installing.insert(plan.function, *plan).is_some() {
                return Err(RegistryError::AlreadyPlanned {
                    name: registered.definition.name.clone(),
                    function: plan.function,
                });
            }
        }
        for plan in installing.values() {
            for (index, callee) in plan.callees.iter().enumerate() {
                // A callback site's callee may run its body in a frame.
                let invoked = plan
                    .sites
                    .iter()
                    .any(|site| site.call == crate::plan::SiteCall::Invoke(index as u32));
                if invoked {
                    continue;
                }
                // A native beside a body may decline: that body needs its
                // own plan too, before it can answer a closed caller; a
                // segment may yield.
                let closed = |plan: &HelperPlan| plan.sites.is_empty();
                let answers = installing.get(callee).is_some_and(|plan| closed(plan))
                    || self.functions.get(callee).is_some_and(|registered| {
                        registered.plan.is_some_and(closed)
                            || registered.definition.body().is_none()
                    });
                if !answers {
                    return Err(RegistryError::PlanCalleeUnanswered {
                        name: self.functions[&plan.function].definition.name.clone(),
                        callee: *callee,
                    });
                }
            }
        }
        let installed = installing.len();
        for (function, plan) in installing {
            if let Some(registered) = self.functions.get_mut(&function) {
                registered.plan = Some(plan);
            }
        }
        Ok(installed)
    }

    pub fn get(&self, function: &FunctionId) -> Option<&RegisteredFunction> {
        self.functions.get(function)
    }

    /// The native implementation registered for a function named `name`,
    /// when one is: what a partial native delegates to, so that it answers
    /// exactly as the body's own call does.
    pub fn native_named(&self, name: &str) -> Option<Arc<dyn NativeFunction>> {
        self.functions
            .values()
            .filter(|registered| registered.definition.name.as_str() == name)
            .find_map(|registered| registered.native.clone())
    }

    /// Export the complete library as typed, serialisable declarations.
    ///
    /// The result implements [`FunctionCatalog`]. A host can decode it and
    /// derive a workflow view without this registry's native implementations
    /// or the engine that owns it. Identities, names, signatures and bodies
    /// are preserved, including transitive library dependencies.
    pub fn export_declarations(&self) -> crate::FunctionDeclarations {
        self.functions
            .iter()
            .map(|(id, registered)| (*id, registered.definition.as_ref().clone()))
            .collect()
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
#[derive(Clone, Debug, PartialEq, Eq)]
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

    /// The token body a build keeps them as: each definition in its
    /// canonical token encoding, the body of its helper-definition graph
    /// node, beside its identity. It is read back only by
    /// [`ValidatedFunctions::from_tokens`] in the same build.
    #[must_use]
    pub fn to_tokens(&self) -> Vec<u8> {
        crate::graph_tokens::encode_body(self)
    }

    /// Reads what [`ValidatedFunctions::to_tokens`] wrote.
    ///
    /// # Errors
    ///
    /// Any token the encoding does not hold.
    pub fn from_tokens(body: &[u8]) -> Result<Self, crate::graph_tokens::TokenError> {
        crate::graph_tokens::decode_body(body)
    }
}

impl crate::graph_tokens::TokenCodec for ValidatedFunctions {
    fn encode(&self, out: &mut crate::graph_tokens::TokenWriter) {
        out.seq(2);
        self.basis.encode(out);
        self.functions.encode(out);
    }

    fn decode(
        input: &mut crate::graph_tokens::TokenReader<'_>,
    ) -> Result<Self, crate::graph_tokens::TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            basis: Vec::decode(input)?,
            functions: Vec::decode(input)?,
        })
    }
}

impl crate::graph_tokens::TokenCodec for ValidatedFunction {
    fn encode(&self, out: &mut crate::graph_tokens::TokenWriter) {
        out.seq(2);
        self.function.encode(out);
        self.definition.encode(out);
    }

    fn decode(
        input: &mut crate::graph_tokens::TokenReader<'_>,
    ) -> Result<Self, crate::graph_tokens::TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            function: FunctionId::decode(input)?,
            definition: FunctionDefinition::decode(input)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
