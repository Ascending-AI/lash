//! Integrator seam: registry definitions stay typed for library construction
//! and linking; they are not the host document retention format.
//!
//! The native-function interface and the function registry (`K-LIB-005`).
//!
//! `lash-kernel-lib` and the `lash-ext-*` crates implement [`NativeFunction`];
//! the embedder registers each implementation beside its definition in a
//! [`FunctionRegistry`] at startup and hands the registry to the machine.

#[path = "native_frame.rs"]
mod frame;
use crate::FrameArray;
pub use frame::NativeCtor;
use frame::RegistryFrame;

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use crate::arena::native::NativeFunction;
use crate::function::FunctionDefinition;
use crate::plan::HelperPlan;
use crate::released::{BodyCensus, ReleasedFunctions};

use crate::validate::{Invalid, validate_identified_definition};

use lash_vocabulary::names::{FunctionId, FunctionName};

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
    /// Resolve an unambiguous authoring helper by its installed name.
    /// Execution and retained documents continue to use exact identities.
    fn named_helper(&self, _name: &str) -> Option<FunctionId> {
        None
    }

    /// The immutable build-owned helper universe, when this catalog is its owner.
    fn helper_library(&self) -> Option<&crate::BuildHelpers> {
        None
    }

    fn definition(&self, function: &FunctionId) -> Option<&FunctionDefinition>;

    /// Registration's immutable facts, when this catalog holds registered code.
    fn registered(&self, _function: &FunctionId) -> Option<&RegisteredFunction> {
        None
    }

    /// The definition's header, its body undecoded where the catalog holds
    /// it encoded (FIG-6931): what lowering, validation and a machine's
    /// preparation read.
    fn header(&self, function: &FunctionId) -> Option<Arc<crate::FunctionHeader>> {
        match self.registered(function) {
            Some(registered) => Some(Arc::clone(registered.header())),
            None => self
                .definition(function)
                .map(|definition| Arc::new(crate::FunctionHeader::of(definition))),
        }
    }

    /// Finds registration facts without identifying the definition again.
    /// Only the exact immutable definition held by the catalog may use them.
    fn registration(&self, _definition: &FunctionDefinition) -> Option<&RegisteredFunction> {
        None
    }
}

impl FunctionCatalog for BTreeMap<FunctionId, FunctionDefinition> {
    fn named_helper(&self, name: &str) -> Option<FunctionId> {
        let mut found = self
            .iter()
            .filter(|(_, definition)| definition.name.as_str() == name);
        let (id, _) = found.next()?;
        found.next().is_none().then_some(*id)
    }

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
    facts: FrameArray<OnceLock<Box<Facts>>>,
}

#[derive(Default)]
struct Facts {
    definition: OnceLock<Arc<FunctionDefinition>>,
    header: OnceLock<Arc<crate::FunctionHeader>>,
    callees: OnceLock<BTreeSet<FunctionId>>,
    /// The empty requirement set for a definition with no body.
    imports: OnceLock<BTreeMap<lash_vocabulary::names::ImportSlot, lash_vocabulary::names::Import>>,
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

    /// The definition's header, read without decoding its body: a
    /// released function's from its image, and once.
    pub fn header(&self) -> &Arc<crate::FunctionHeader> {
        let facts = self.facts();
        facts.header.get_or_init(|| {
            if let Some(definition) = facts.definition.get() {
                return Arc::new(crate::FunctionHeader::of(definition));
            }
            match &self.held {
                Held::Released(set, at) => {
                    match crate::graph_codec::decode_header(
                        set.image.definition_bytes(*at as usize),
                    ) {
                        Ok(header) => Arc::new(header),
                        Err(error) => unreachable!("a build's release image decodes: {error}"),
                    }
                }
                Held::Defined(_) => Arc::new(crate::FunctionHeader::of(self.definition())),
            }
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

    /// The definition's token body ([`lash_vocabulary::tokens::encode_body`]),
    /// the bytes its checkpoint graph node is encoded from, encoded once per
    /// registration and shared by the registry's clones.
    pub fn image(&self) -> Arc<[u8]> {
        Arc::clone(self.facts().image.get_or_init(|| match &self.held {
            Held::Released(set, at) => Arc::from(set.image.definition_bytes(*at as usize)),
            Held::Defined(_) => {
                Arc::from(lash_vocabulary::tokens::encode_body(&**self.definition()))
            }
        }))
    }

    /// Every library function the body holds, in any slot
    /// ([`crate::Node::functions_within`]): what a document reaching this
    /// function reaches through it (`K-DOC-002`).
    pub fn callees(&self) -> &BTreeSet<FunctionId> {
        self.facts()
            .callees
            .get_or_init(|| self.listed().into_iter().map(|(id, _)| id).collect())
    }

    /// Every operation the body performs itself, with what it requires of
    /// it ([`crate::FunctionBody::effects`]): none for a function with no
    /// body. A document reaching this function requires them through it
    /// (`K-DOC-002`); what its callees perform is theirs.
    pub fn imports(
        &self,
    ) -> &BTreeMap<lash_vocabulary::names::ImportSlot, lash_vocabulary::names::Import> {
        self.facts().imports.get_or_init(|| match &self.held {
            Held::Released(set, at) => set
                .image
                .imports(*at as usize)
                .unwrap_or_else(|error| unreachable!("validated release imports decode: {error}")),
            Held::Defined(_) => self
                .definition()
                .body()
                .map(|body| body.imports.clone())
                .unwrap_or_default(),
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
    #[error("the pool frame could not be opened: {message}")]
    Frame { message: String },
    #[error("native {function} (`{name}`) is missing from the worker binary")]
    FrameNativeMissing {
        function: FunctionId,
        name: FunctionName,
    },
    /// A generated plan has no matching body in the installed frame.
    #[error("generated plan {function} (`{name}`) has no registered body")]
    FramePlanMissing {
        function: FunctionId,
        name: FunctionName,
    },
    #[error("native {function} is not stated by any held definition")]
    FrameNativeUnused { function: FunctionId },
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
    frame: Option<Arc<RegistryFrame>>,
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

fn identity_buckets(functions: impl IntoIterator<Item = FunctionId>) -> Box<[u32]> {
    let mut starts = vec![0u32; (1 << BUCKET_BITS) + 1];
    for function in functions {
        starts[bucket(&function) + 1] += 1;
    }
    for at in 1..starts.len() {
        starts[at] += starts[at - 1];
    }
    starts.into_boxed_slice()
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
    /// Shares immutable declarations and all validated facts without decoding
    /// definitions or walking the library. Intended for parent-side admission.
    pub fn as_declarations(&self) -> Self {
        Self {
            declarations: true,
            ..self.clone()
        }
    }

    pub fn declarations() -> Self {
        Self {
            declarations: true,
            ..Self::default()
        }
    }

    /// Where `function` stands in identity order.
    fn find(&self, function: &FunctionId) -> Result<usize, usize> {
        if let Some(frame) = &self.frame {
            let image = frame.image();
            let (mut lo, mut hi) = frame.range(function);
            while lo < hi {
                let mid = (lo + hi) / 2;
                if image.id(mid) < *function {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            return if lo < image.len() && image.id(lo) == *function {
                Ok(lo)
            } else {
                Err(lo)
            };
        }
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
        self.buckets = Some(identity_buckets(
            self.functions.iter().map(|registered| registered.function),
        ));
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
        self.materialize();
        let name = definition.name.clone();
        let identity = validate_identified_definition(
            &definition,
            self,
            lash_vocabulary::data_limits::KernelDataLimits::standard(),
        )
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

    /// Freezes every registered definition in one self-contained release image.
    ///
    /// A parent sends this image to each worker at pool start. The registry
    /// already validated the definitions, so their identities and body census
    /// are retained without registering or identifying them again. Natives and
    /// generated plans belong to the worker binary and do not cross.
    #[must_use]
    pub fn to_image(&self) -> Vec<u8> {
        self.to_image_with_census(&|_| None)
    }

    /// Freezes parent-computed census facts for functions authored in memory.
    pub fn to_image_with_census(
        &self,
        census: &dyn Fn(&FunctionDefinition) -> Option<BodyCensus>,
    ) -> Vec<u8> {
        let validated = ValidatedFunctions {
            basis: Vec::new(),
            functions: self
                .iter()
                .map(|(function, registered)| ValidatedFunction {
                    function: *function,
                    definition: registered.definition().as_ref().clone(),
                })
                .collect(),
        };
        validated.to_image_with(&|function, definition| {
            self.get(function)
                .and_then(RegisteredFunction::census)
                .or_else(|| census(definition))
        })
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
        self.materialize();
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
        self.materialize();
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
        self.install(plans, true)
    }

    /// [`FunctionRegistry::install_plans`], skipping each plan that calls a
    /// callee nothing answers: a registry assembled from an execution's
    /// stored closure holds a plan's function without every callee the
    /// build generated it against, and runs that function's body instead.
    /// It returns how many plans it installed.
    ///
    /// # Errors
    ///
    /// [`RegistryError::PlanWithoutBody`] and
    /// [`RegistryError::AlreadyPlanned`].
    pub fn install_answered_plans(
        &mut self,
        plans: &[&'static HelperPlan],
    ) -> Result<usize, RegistryError> {
        self.install(plans, false)
    }

    fn install(
        &mut self,
        plans: &[&'static HelperPlan],
        strict: bool,
    ) -> Result<usize, RegistryError> {
        self.materialize();
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
        loop {
            let mut answers: Vec<bool> = self
                .functions
                .iter()
                .map(|registered| registered.plan.is_some_and(closed) || !registered.has_body())
                .collect();
            for (at, plan) in &installing {
                answers[*at] |= closed(plan);
            }
            let unanswered: Vec<usize> = installing
                .iter()
                .enumerate()
                .filter_map(|(index, (at, plan))| {
                    let callee = self.unanswered(plan, &answers)?;
                    Some((index, *at, callee))
                })
                .map(|(index, at, callee)| {
                    if strict {
                        Err(RegistryError::PlanCalleeUnanswered {
                            name: self.functions[at].definition().name.clone(),
                            callee,
                        })
                    } else {
                        Ok(index)
                    }
                })
                .collect::<Result<_, _>>()?;
            if unanswered.is_empty() {
                break;
            }
            // A skipped closed plan no longer answers its callers: check
            // the rest again without it.
            for index in unanswered.into_iter().rev() {
                installing.remove(index);
            }
        }
        let installed = installing.len();
        let functions = Arc::make_mut(&mut self.functions);
        for (at, plan) in installing {
            functions[at].plan = Some(plan);
        }
        Ok(installed)
    }

    /// The first callee `plan` calls outside its callback sites that
    /// `answers`, by registry position, does not answer.
    fn unanswered(&self, plan: &HelperPlan, answers: &[bool]) -> Option<FunctionId> {
        // Callback membership alone answers these callees. Project it once
        // before looking up the remaining callees in the registry; wide
        // dispatchers need no registry probe for every callback.
        let mut callbacks = Vec::new();
        if !plan.sites.is_empty() {
            callbacks.resize(plan.callees.len(), false);
            for site in plan.sites {
                if let crate::plan::SiteCall::Invoke(callee) = site.call
                    && let Some(invoked) = callbacks.get_mut(callee as usize)
                {
                    *invoked = true;
                }
            }
        }
        plan.callees
            .iter()
            .enumerate()
            .find(|(index, callee)| {
                !(callbacks.get(*index).copied().unwrap_or(false)
                    || self.find(callee).is_ok_and(|callee| answers[callee]))
            })
            .map(|(_, callee)| *callee)
    }

    pub fn get(&self, function: &FunctionId) -> Option<&RegisteredFunction> {
        self.find(function).ok().and_then(|at| self.at(at))
    }

    /// Where `function` stands among every registered function in identity
    /// order ([`FunctionRegistry::iter`]), when the registry holds it.
    pub fn position(&self, function: &FunctionId) -> Option<usize> {
        self.find(function).ok()
    }

    /// Whether every function a registered body names is registered: then
    /// no body reaches a function the registry lacks.
    pub fn holds_every_callee(&self) -> bool {
        self.frame.as_ref().map_or_else(
            || self.dangling.is_empty(),
            |frame| frame.image().dangling().len() == 0,
        )
    }

    /// The functions whose definitions are named `name`, in identity order.
    pub fn named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = FunctionId> + 'a {
        let mut lo = 0;
        let mut hi = self.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.name_at(mid).is_some_and(|(found, _)| found < name) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (lo..self.len()).map_while(move |at| {
            self.name_at(at)
                .filter(|(found, _)| *found == name)
                .map(|(_, function)| function)
        })
    }

    /// Every function in the frame's name index, without a registration allocation.
    pub fn names(&self) -> impl ExactSizeIterator<Item = (&str, FunctionId)> + '_ {
        (0..self.len()).map(|rank| {
            self.name_at(rank)
                .unwrap_or_else(|| unreachable!("name index"))
        })
    }

    /// The `rank`th entry of [`FunctionRegistry::names`].
    pub fn name_at(&self, rank: usize) -> Option<(&str, FunctionId)> {
        if let Some(frame) = &self.frame {
            let image = frame.image();
            if rank >= image.len() {
                return None;
            }
            let at = image.name_order(rank);
            return Some((image.name(at), image.id(at)));
        }
        let named = self.names.get(rank)?;
        Some((named.name(), named.function))
    }

    /// A function's name without materializing its registration or native.
    pub fn name_of(&self, function: &FunctionId) -> Option<&str> {
        let at = self.position(function)?;
        match &self.frame {
            Some(frame) => Some(frame.image().name(at)),
            None => self.functions.get(at).map(RegisteredFunction::name),
        }
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
        self.iter()
            .map(|(_, registered)| {
                (
                    registered.function,
                    registered.definition().as_ref().clone(),
                )
            })
            .collect()
    }

    /// Every registered function, ordered by identity.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&FunctionId, &RegisteredFunction)> {
        (0..self.len()).map(|at| {
            let registered = self
                .at(at)
                .unwrap_or_else(|| unreachable!("registry position"));
            (&registered.function, registered)
        })
    }

    pub fn len(&self) -> usize {
        self.frame
            .as_ref()
            .map_or_else(|| self.functions.len(), |frame| frame.image().len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
    fn named_helper(&self, name: &str) -> Option<FunctionId> {
        let mut found = self.named(name);
        let id = found.next()?;
        found.next().is_none().then_some(id)
    }

    fn registered(&self, function: &FunctionId) -> Option<&RegisteredFunction> {
        self.get(function)
    }

    fn registration(&self, definition: &FunctionDefinition) -> Option<&RegisteredFunction> {
        self.iter()
            .map(|(_, registered)| registered)
            .find(|registered| {
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
