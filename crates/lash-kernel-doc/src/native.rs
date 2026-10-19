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

use crate::arena::native::NativeFunction;
use crate::function::FunctionDefinition;
use crate::name::{FunctionId, FunctionName};
use crate::plan::HelperPlan;
use crate::released::{BodyCensus, ReleasedFunctions};
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
    function: FunctionId,
    /// The native implementation, when the embedder supplied one.
    native: Option<Arc<dyn NativeFunction>>,
    /// The body compiled ahead of time, when the build generated one.
    plan: Option<&'static HelperPlan>,
    held: Held,
}

/// Where a registered function's definition and the facts derived from it
/// live, shared by the registry's clones.
#[derive(Clone)]
enum Held {
    /// Registered one by one, with its definition in hand.
    Defined(Arc<Facts>),
    /// One function of a build's release image, decoded when first read.
    Released(Arc<ReleasedSet>, u32),
}

/// A release image registered at once, and the facts of each of its
/// functions a process has read.
struct ReleasedSet {
    image: ReleasedFunctions,
    facts: Box<[OnceLock<Box<Facts>>]>,
}

#[derive(Default)]
struct Facts {
    definition: OnceLock<Arc<FunctionDefinition>>,
    callees: OnceLock<BTreeSet<FunctionId>>,
    /// The definition's token body, encoded once for every holder that
    /// stores it.
    image: OnceLock<Arc<[u8]>>,
    // Consumers own their derived types. Erasure keeps the document vocabulary
    // independent of the checker while registry clones share each result.
    derived: Mutex<DerivedFacts>,
}

type DerivedFacts = BTreeMap<TypeId, Arc<dyn Any + Send + Sync>>;

impl RegisteredFunction {
    fn new(
        function: FunctionId,
        definition: FunctionDefinition,
        native: Option<Arc<dyn NativeFunction>>,
    ) -> Self {
        Self {
            function,
            native,
            plan: None,
            held: Held::Defined(Arc::new(Facts {
                definition: OnceLock::from(Arc::new(definition)),
                ..Facts::default()
            })),
        }
    }

    fn from_image(set: &Arc<ReleasedSet>, at: usize) -> Self {
        Self {
            function: set.image.id(at),
            native: None,
            plan: None,
            held: Held::Released(Arc::clone(set), at as u32),
        }
    }

    fn facts(&self) -> &Facts {
        match &self.held {
            Held::Defined(facts) => facts,
            Held::Released(set, at) => set.facts[*at as usize].get_or_init(Box::default),
        }
    }

    /// The immutable definition whose identity and derived facts were
    /// registered. A released function's is decoded when first read.
    pub fn definition(&self) -> &Arc<FunctionDefinition> {
        self.facts().definition.get_or_init(|| match &self.held {
            Held::Released(set, at) => match set.image.definition(*at as usize) {
                Ok(definition) => Arc::new(definition),
                Err(error) => unreachable!("a build's release image decodes: {error}"),
            },
            Held::Defined(_) => unreachable!("a defined function holds its definition"),
        })
    }

    /// The definition's name, read without decoding it.
    pub fn name(&self) -> &str {
        match &self.held {
            Held::Released(set, at) => set.image.name(*at as usize),
            Held::Defined(_) => self.definition().name.as_str(),
        }
    }

    /// Whether the definition has a kernel body, read without decoding it.
    pub fn has_body(&self) -> bool {
        match &self.held {
            Held::Released(set, at) => set.image.has_body(*at as usize),
            Held::Defined(_) => self.definition().body().is_some(),
        }
    }

    /// The kernel version the definition is written for, read without
    /// decoding it.
    pub fn kernel(&self) -> u32 {
        match &self.held {
            Held::Released(set, at) => set.image.kernel(*at as usize),
            Held::Defined(_) => self.definition().kernel,
        }
    }

    /// Every function the body lists, by identity, with the name its
    /// definition carries ([`crate::FunctionBody::functions`]), read without
    /// decoding it: none for a function with no body.
    pub fn listed(&self) -> Vec<(FunctionId, FunctionName)> {
        match &self.held {
            Held::Released(set, at) => set
                .image
                .listed(*at as usize)
                .map(|(function, name)| (function, FunctionName::written(name)))
                .collect(),
            Held::Defined(_) => self
                .definition()
                .body()
                .iter()
                .flat_map(|body| &body.functions)
                .map(|(function, name)| (*function, name.clone()))
                .collect(),
        }
    }

    /// Whether the definition states a native implementation, read without
    /// decoding it.
    pub fn states_native(&self) -> bool {
        match &self.held {
            Held::Released(set, at) => set.image.states_native(*at as usize),
            Held::Defined(_) => self.definition().has_native(),
        }
    }

    /// Whether the function was registered from a build's release image
    /// ([`FunctionRegistry::register_released`]).
    pub fn released(&self) -> bool {
        matches!(self.held, Held::Released(..))
    }

    /// The body's census as the build wrote it into its release image:
    /// none for a function with no body or one the machine runs itself,
    /// and none for a function registered one by one.
    pub fn census(&self) -> Option<BodyCensus> {
        match &self.held {
            Held::Released(set, at) => set.image.census(*at as usize),
            Held::Defined(_) => None,
        }
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
        Arc::clone(self.facts().image.get_or_init(|| match &self.held {
            Held::Released(set, at) => Arc::from(set.image.definition_bytes(*at as usize)),
            Held::Defined(_) => Arc::from(crate::graph_tokens::encode_body(&**self.definition())),
        }))
    }

    /// The functions the body actually calls or spawns, including nested expressions.
    pub fn callees(&self) -> &BTreeSet<FunctionId> {
        self.facts().callees.get_or_init(|| {
            let mut callees = BTreeSet::new();
            if let Some(body) = self.definition().body() {
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
            .facts()
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
            .field("definition", self.definition())
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
    /// A native implementation was offered by name for a function no
    /// registered definition is named.
    #[error(
        "no registered function is named `{name}`, so no native implementation can be added to it"
    )]
    NameNotRegistered { name: String },
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
///
/// It is flat: its functions in a vector ordered by identity, and their
/// names in one ordered by name. A build's release image is registered at
/// once and in place ([`FunctionRegistry::register_released`]): nothing of
/// a function the process never reads is decoded, prepared or dropped
/// (FIG-6843).
#[derive(Clone, Debug, Default)]
pub struct FunctionRegistry {
    /// Every function, ordered by identity. Clones share it until one
    /// changes.
    functions: Arc<Vec<RegisteredFunction>>,
    /// Every function by its definition's name, ordered by name and then
    /// by identity: two definitions may share a name.
    names: Arc<Vec<Named>>,
    /// Each function a registered body names that the registry does not
    /// hold.
    dangling: BTreeSet<FunctionId>,
    /// Where each identity bucket ([`bucket`]) starts in `functions`, and
    /// where the last ends: a lookup compares within one bucket. Made when
    /// a release image is registered; a function registered one by one
    /// drops it, and lookups search the whole vector until the next image.
    buckets: Option<Box<[u32]>>,
    /// The registry holds definitions alone ([`FunctionRegistry::declarations`]).
    declarations: bool,
}

/// How many leading bits of an identity choose its bucket: about one
/// function per bucket for a full worker registry.
const BUCKET_BITS: u32 = 10;

/// The bucket of `function`'s identity: its leading bits. Identities are
/// digests, so they spread evenly.
fn bucket(function: &FunctionId) -> usize {
    let digest = function.as_bytes();
    usize::from(u16::from_be_bytes([digest[0], digest[1]]) >> (16 - BUCKET_BITS))
}

/// One entry of a registry's name index.
#[derive(Clone, Debug)]
struct Named {
    name: NameRef,
    function: FunctionId,
}

/// A name the index holds: in its release image, or in its definition.
#[derive(Clone, Debug)]
enum NameRef {
    Released(&'static str),
    Defined(Arc<FunctionDefinition>),
}

impl Named {
    fn name(&self) -> &str {
        match &self.name {
            NameRef::Released(name) => name,
            NameRef::Defined(definition) => definition.name.as_str(),
        }
    }

    fn key(&self) -> (&str, &FunctionId) {
        (self.name(), &self.function)
    }
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
            declarations: true,
            ..Self::default()
        }
    }

    /// Where `function` stands in identity order.
    fn find(&self, function: &FunctionId) -> Result<usize, usize> {
        let (start, end) = match &self.buckets {
            Some(buckets) => {
                let at = bucket(function);
                (buckets[at] as usize, buckets[at + 1] as usize)
            }
            None => (0, self.functions.len()),
        };
        match self.functions[start..end]
            .binary_search_by(|registered| registered.function.cmp(function))
        {
            Ok(at) => Ok(start + at),
            Err(at) => Err(start + at),
        }
    }

    /// Indexes the functions by identity bucket.
    fn index_buckets(&mut self) {
        let mut starts = vec![0u32; (1 << BUCKET_BITS) + 1];
        for registered in self.functions.iter() {
            starts[bucket(&registered.function) + 1] += 1;
        }
        for at in 1..starts.len() {
            starts[at] += starts[at - 1];
        }
        self.buckets = Some(starts.into_boxed_slice());
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
        let at = match self.find(&function) {
            Ok(_) => return Err(RegistryError::AlreadyRegistered { name, function }),
            Err(at) => at,
        };
        let registered = RegisteredFunction::new(function, definition, native);
        let definition = Arc::clone(registered.definition());
        self.dangling.remove(&function);
        if let Some(body) = definition.body() {
            for callee in body.functions.keys() {
                if *callee != function && self.find(callee).is_err() {
                    self.dangling.insert(*callee);
                }
            }
        }
        Arc::make_mut(&mut self.functions).insert(at, registered);
        self.buckets = None;
        let named = Named {
            name: NameRef::Defined(definition),
            function,
        };
        let place = self
            .names
            .partition_point(|other| other.key() < named.key());
        Arc::make_mut(&mut self.names).insert(place, named);
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
        let basis = self
            .functions
            .iter()
            .map(|registered| registered.function)
            .collect();
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

    /// Registers a build's release image of functions
    /// [`FunctionRegistry::validate_functions`] validated against functions
    /// this registry holds, every one, in place: nothing is validated,
    /// identified or decoded again. Validation and identity are functions of
    /// a definition and the functions it names, so a registry that holds
    /// more (another dialect's helpers) answers the same. Each definition is
    /// decoded when it is first read.
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
    /// A refused image registers nothing.
    pub fn register_released(&mut self, image: ReleasedFunctions) -> Result<(), RegistryError> {
        let mut held = self
            .functions
            .iter()
            .map(|registered| registered.function)
            .peekable();
        for function in image.basis() {
            while held.next_if(|other| *other < function).is_some() {}
            if held.next_if_eq(&function).is_none() {
                return Err(RegistryError::OtherBasis {
                    validated: image.basis().len(),
                    held: self.functions.len(),
                });
            }
        }
        let mut held = self.functions.iter().peekable();
        for function in image.ids() {
            while held.next_if(|other| other.function < function).is_some() {}
            if let Some(registered) = held.peek()
                && registered.function == function
            {
                return Err(RegistryError::AlreadyRegistered {
                    name: registered.definition().name.clone(),
                    function,
                });
            }
        }
        let set = Arc::new(ReleasedSet {
            image,
            facts: (0..image.len()).map(|_| OnceLock::new()).collect(),
        });
        let mut functions = Vec::with_capacity(self.functions.len() + image.len());
        let mut held = Arc::unwrap_or_clone(std::mem::take(&mut self.functions))
            .into_iter()
            .peekable();
        for at in 0..image.len() {
            let function = image.id(at);
            while let Some(registered) = held.next_if(|other| other.function < function) {
                functions.push(registered);
            }
            functions.push(RegisteredFunction::from_image(&set, at));
        }
        functions.extend(held);
        self.functions = Arc::new(functions);
        self.index_buckets();
        let mut names = Vec::with_capacity(self.names.len() + image.len());
        let mut held = Arc::unwrap_or_clone(std::mem::take(&mut self.names))
            .into_iter()
            .peekable();
        for rank in 0..image.len() {
            let at = image.name_order(rank);
            let named = Named {
                name: NameRef::Released(image.name(at)),
                function: image.id(at),
            };
            while let Some(other) = held.next_if(|other| other.key() < named.key()) {
                names.push(other);
            }
            names.push(named);
        }
        names.extend(held);
        self.names = Arc::new(names);
        for function in image.ids() {
            self.dangling.remove(&function);
        }
        for callee in image.dangling() {
            if self.find(&callee).is_err() {
                self.dangling.insert(callee);
            }
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
        let at = self
            .find(&function)
            .map_err(|_| RegistryError::NotRegistered { function })?;
        let registered = &mut Arc::make_mut(&mut self.functions)[at];
        if !registered.states_native() {
            return Err(RegistryError::NativeNotStated {
                name: registered.definition().name.clone(),
            });
        }
        if registered.native.is_some() {
            return Err(RegistryError::AlreadyImplemented {
                name: registered.definition().name.clone(),
                function,
            });
        }
        registered.native = Some(native);
        Ok(())
    }

    /// [`FunctionRegistry::implement`] for the function named `name`: how an
    /// embedder adds the natives of definitions its build's release image
    /// holds (FIG-6843). It returns the function's identity.
    ///
    /// # Errors
    ///
    /// [`RegistryError::NameNotRegistered`], and
    /// [`FunctionRegistry::implement`]'s.
    pub fn implement_named(
        &mut self,
        name: &str,
        native: Arc<dyn NativeFunction>,
    ) -> Result<FunctionId, RegistryError> {
        let function = self
            .named(name)
            .next()
            .ok_or_else(|| RegistryError::NameNotRegistered {
                name: name.to_owned(),
            })?;
        self.implement(function, native)?;
        Ok(function)
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
        let mut installing: Vec<(usize, &'static HelperPlan)> = Vec::with_capacity(plans.len());
        let mut seen = vec![false; self.functions.len()];
        for plan in plans {
            let Ok(at) = self.find(&plan.function) else {
                continue;
            };
            let registered = &self.functions[at];
            if !registered.has_body() {
                return Err(RegistryError::PlanWithoutBody {
                    name: registered.definition().name.clone(),
                });
            }
            if registered.plan.is_some() || std::mem::replace(&mut seen[at], true) {
                return Err(RegistryError::AlreadyPlanned {
                    name: registered.definition().name.clone(),
                    function: plan.function,
                });
            }
            installing.push((at, plan));
        }
        installing.sort_unstable_by_key(|(at, _)| *at);
        // A native beside a body may decline: that body needs its own plan
        // too, before it can answer a closed caller; a segment may yield.
        let closed = |plan: &HelperPlan| plan.sites.is_empty();
        let mut answers: Vec<bool> = self
            .functions
            .iter()
            .map(|registered| registered.plan.is_some_and(closed) || !registered.has_body())
            .collect();
        for (at, plan) in &installing {
            answers[*at] |= closed(plan);
        }
        for (at, plan) in &installing {
            for (index, callee) in plan.callees.iter().enumerate() {
                // A callback site's callee may run its body in a frame.
                let invoked = || {
                    plan.sites
                        .iter()
                        .any(|site| site.call == crate::plan::SiteCall::Invoke(index as u32))
                };
                if self.find(callee).is_ok_and(|callee| answers[callee]) || invoked() {
                    continue;
                }
                return Err(RegistryError::PlanCalleeUnanswered {
                    name: self.functions[*at].definition().name.clone(),
                    callee: *callee,
                });
            }
        }
        let installed = installing.len();
        let functions = Arc::make_mut(&mut self.functions);
        for (at, plan) in installing {
            functions[at].plan = Some(plan);
        }
        Ok(installed)
    }

    pub fn get(&self, function: &FunctionId) -> Option<&RegisteredFunction> {
        self.find(function).ok().map(|at| &self.functions[at])
    }

    /// Where `function` stands among every registered function in identity
    /// order ([`FunctionRegistry::iter`]), when the registry holds it.
    pub fn position(&self, function: &FunctionId) -> Option<usize> {
        self.find(function).ok()
    }

    /// Whether every function a registered body names is registered: then
    /// no body reaches a function the registry lacks.
    pub fn holds_every_callee(&self) -> bool {
        self.dangling.is_empty()
    }

    /// The functions whose definitions are named `name`, in identity order.
    pub fn named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = FunctionId> + 'a {
        let start = self.names.partition_point(|named| named.name() < name);
        self.names[start..]
            .iter()
            .take_while(move |named| named.name() == name)
            .map(|named| named.function)
    }

    /// Every function by its definition's name, ordered by name and then
    /// by identity.
    pub fn names(&self) -> impl ExactSizeIterator<Item = (&str, FunctionId)> + '_ {
        self.names
            .iter()
            .map(|named| (named.name(), named.function))
    }

    /// The `rank`th entry of [`FunctionRegistry::names`].
    pub fn name_at(&self, rank: usize) -> Option<(&str, FunctionId)> {
        let named = self.names.get(rank)?;
        Some((named.name(), named.function))
    }

    /// The native implementation registered for a function named `name`,
    /// when one is: what a partial native delegates to, so that it answers
    /// exactly as the body's own call does.
    pub fn native_named(&self, name: &str) -> Option<Arc<dyn NativeFunction>> {
        self.named(name)
            .find_map(|function| self.get(&function)?.native.clone())
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
            .map(|registered| {
                (
                    registered.function,
                    registered.definition().as_ref().clone(),
                )
            })
            .collect()
    }

    /// Every registered function, ordered by identity.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&FunctionId, &RegisteredFunction)> {
        self.functions
            .iter()
            .map(|registered| (&registered.function, registered))
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
/// again: through their build's image ([`ValidatedFunctions::to_image`],
/// [`FunctionRegistry::register_released`]).
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

    /// Every function the registry held when they were validated.
    pub(crate) fn basis(&self) -> &[FunctionId] {
        &self.basis
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
        self.functions.iter().find(|registered| {
            registered
                .facts()
                .definition
                .get()
                .is_some_and(|held| std::ptr::eq(held.as_ref(), definition))
        })
    }

    fn definition(&self, function: &FunctionId) -> Option<&FunctionDefinition> {
        self.get(function)
            .map(|registered| registered.definition().as_ref())
    }
}
