//! What a worker can run: the function registry it assembles when it
//! starts, and the dialects it lowers and prints.
//!
//! A document names every function it uses by content identity and carries
//! none of them (kernel spec §9 rule 2), so the registry is fixed before the
//! first document arrives: the kernel library, the machine's own functions,
//! each extension crate's, and each dialect's helpers. The standard
//! embedding's TypeScript helpers are defined once, when the crate is built
//! (`build.rs`); a worker registers them as built (FIG-5796).
//!
//! The standard embedding also holds every function of the helper releases
//! it retains that it does not define itself (FIG-5799, `lash-vm-library`): a
//! run parked under an earlier release's helpers, and a saved function
//! written against them, resume on exactly the functions they pin. Names
//! resolve against the embedding's own functions alone, or, for a writer the
//! fleet holds to an earlier release, against that release's.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};

use lash_kernel_dialect::{
    Diagnostic, Environment, FrontEnd, Library, Lowered, NamedLibrary, Package, Printer,
};
use lash_kernel_doc::{
    Document, FunctionDefinition, FunctionId, FunctionRegistry, ValidatedFunctions,
};
use lash_kernel_vm::PreparedLibrary;
use lash_vm_client::WorkerTuning;
use lash_vm_library::{HELPER_RELEASE, HelperReleaseIndex};

/// Why a worker could not assemble what it runs. The worker refuses to
/// start: it is a defect of the build, never of a document.
#[derive(Clone, Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("the function registry refused a definition: {0}")]
    Registry(String),
    #[error("the dialect `{dialect}` is installed twice")]
    DuplicateDialect { dialect: String },
    #[error("the dialect `{dialect}` could not be installed: {message}")]
    Dialect { dialect: String, message: String },
    /// A writer asked for the names of a helper release the embedding does
    /// not hold.
    #[error("helper release {release} is not held by this embedding")]
    HelperReleaseNotHeld { release: u32 },
}

fn cached<T>(
    slot: &OnceLock<Result<T, EmbedError>>,
    init: impl FnOnce() -> Result<T, EmbedError>,
) -> Result<&T, EmbedError> {
    slot.get_or_init(init).as_ref().map_err(Clone::clone)
}

/// Everything a worker registered at startup.
pub struct Embedding {
    /// The functions as registered, each written for the kernel version
    /// the dialects lower to.
    written: Arc<FunctionRegistry>,
    /// `written` and each function redeclared for every successor version
    /// the build interprets, made when first asked for: a worker that runs
    /// no successor's document never redeclares (FIG-5796).
    interpreted: OnceLock<Result<Arc<FunctionRegistry>, EmbedError>>,
    /// `written` and `interpreted` with every library body compiled, once
    /// for all the runs of the worker, made when a document first runs
    /// against each.
    prepared_written: OnceLock<PreparedLibrary>,
    prepared_interpreted: OnceLock<PreparedLibrary>,
    pub(crate) library: NamedLibrary,
    pub(crate) dialects: BTreeMap<String, Package>,
    /// The helper release the embedding's own names are.
    writes: u32,
    /// The released helper sets the embedding holds, each with the names a
    /// writer of it resolves against, made when first asked for.
    releases: Vec<(
        HelperReleaseIndex,
        OnceLock<Result<NamedLibrary, EmbedError>>,
    )>,
}

impl Embedding {
    /// Every registered function, by name: what a document's manifest
    /// resolves its names against.
    pub fn library(&self) -> &NamedLibrary {
        &self.library
    }

    /// The helper release the embedding's own names are.
    pub fn helper_release(&self) -> u32 {
        self.writes
    }

    /// The released helper sets the embedding holds besides its own
    /// functions, oldest first.
    pub fn helper_releases(&self) -> impl Iterator<Item = &HelperReleaseIndex> {
        self.releases.iter().map(|(release, _)| release)
    }

    /// The names a writer of helper release `release` resolves against:
    /// the embedding's own for its own release, and a retained release's
    /// as that release wrote them, over every function the embedding holds.
    ///
    /// # Errors
    ///
    /// [`EmbedError::HelperReleaseNotHeld`], and [`EmbedError::Registry`]
    /// when a release's names cannot be assembled: a defect of the build.
    pub fn library_for(&self, release: u32) -> Result<&NamedLibrary, EmbedError> {
        if release == self.writes {
            return Ok(&self.library);
        }
        let (index, library) = self
            .releases
            .iter()
            .find(|(index, _)| index.ordinal == release)
            .ok_or(EmbedError::HelperReleaseNotHeld { release })?;
        cached(library, || {
            NamedLibrary::resolving(&self.written, |function| index.writes.contains(function))
                .map_err(|error| EmbedError::Registry(error.to_string()))
        })
    }

    /// The registry a machine runs documents against: each function once
    /// for every kernel version this build interprets. A build that also
    /// interprets the successor of the version the dialects write holds
    /// every function redeclared for it, by identity, from the first call.
    ///
    /// # Errors
    ///
    /// [`EmbedError::Registry`] when a function cannot be redeclared: a
    /// defect of the build.
    pub fn registry(&self) -> Result<&Arc<FunctionRegistry>, EmbedError> {
        cached(&self.interpreted, || {
            let migrations: Vec<_> = lash_kernel_doc::KernelVersion::ALL
                .iter()
                .filter_map(|version| lash_kernel_migrate::migration_from(*version))
                .collect();
            if migrations.is_empty() {
                return Ok(Arc::clone(&self.written));
            }
            let mut registry = FunctionRegistry::clone(&self.written);
            for migration in migrations {
                lash_kernel_migrate::migrate_registry(&mut registry, migration)
                    .map_err(|error| EmbedError::Registry(error.to_string()))?;
            }
            Ok(Arc::new(registry))
        })
    }

    /// The registry a machine runs a document written for kernel version
    /// `kernel` against: the functions as registered when the dialects
    /// write that version, and [`Embedding::registry`] otherwise.
    ///
    /// # Errors
    ///
    /// [`Embedding::registry`]'s.
    pub(crate) fn registry_for(&self, kernel: u32) -> Result<&Arc<FunctionRegistry>, EmbedError> {
        if kernel == lash_kernel_doc::KERNEL_VERSION {
            return Ok(&self.written);
        }
        self.registry()
    }

    /// The prepared library a machine runs a document written for kernel
    /// version `kernel` over: [`Embedding::registry_for`]'s registry, its
    /// library bodies compiled when a document first runs against it.
    ///
    /// # Errors
    ///
    /// [`Embedding::registry`]'s.
    pub(crate) fn prepared_for(&self, kernel: u32) -> Result<&PreparedLibrary, EmbedError> {
        let prepared = if kernel == lash_kernel_doc::KERNEL_VERSION {
            &self.prepared_written
        } else {
            &self.prepared_interpreted
        };
        if let Some(library) = prepared.get() {
            return Ok(library);
        }
        let registry = self.registry_for(kernel)?;
        Ok(prepared.get_or_init(|| PreparedLibrary::new(Arc::clone(registry))))
    }

    /// Lowers `source` as the installed dialect `dialect` does for a cell:
    /// against `environment`'s library, offered effects and namespace roots,
    /// session bindings and saved functions.
    /// `None` when no such dialect is installed.
    ///
    /// A host calls it to learn whether text it shows or stores lowers,
    /// without a worker or a session.
    pub fn lower(
        &self,
        dialect: &str,
        source: &str,
        environment: &Environment<'_>,
    ) -> Option<Result<Lowered, Diagnostic>> {
        let package = self.dialects.get(dialect)?;
        Some(package.front_end.lower(source, environment))
    }
}

/// A worker's registry while it is assembled.
pub struct Embedder {
    registry: FunctionRegistry,
    dialects: BTreeMap<String, Package>,
    /// The functions registered as an earlier release's, which no name
    /// resolves to.
    retained: BTreeSet<FunctionId>,
    writes: u32,
    releases: Vec<HelperReleaseIndex>,
}

impl Embedder {
    /// The kernel library and the machine's own functions.
    ///
    /// # Errors
    ///
    /// [`EmbedError::Registry`].
    pub fn kernel() -> Result<Self, EmbedError> {
        let mut registry = FunctionRegistry::new();
        crate::library::register_kernel(&mut registry).map_err(EmbedError::Registry)?;
        Ok(Self {
            registry,
            dialects: BTreeMap::new(),
            retained: BTreeSet::new(),
            writes: HELPER_RELEASE,
            releases: Vec::new(),
        })
    }

    /// The registry, for an extension crate to register its functions in.
    pub fn registry(&mut self) -> &mut FunctionRegistry {
        &mut self.registry
    }

    /// Every function registered so far, by name: what a dialect defines
    /// its helpers against.
    ///
    /// # Errors
    ///
    /// [`EmbedError::Registry`] when two functions share a name.
    pub fn library(&self) -> Result<NamedLibrary, EmbedError> {
        NamedLibrary::resolving(&self.registry, |function| !self.retained.contains(function))
            .map_err(|error| EmbedError::Registry(error.to_string()))
    }

    /// Registers functions of the helper releases `releases` this embedding
    /// retains, validated against exactly what it holds now, under no name:
    /// they are found by identity alone (FIG-5799).
    ///
    /// # Errors
    ///
    /// [`EmbedError::Registry`].
    fn retain(
        &mut self,
        functions: ValidatedFunctions,
        releases: Vec<HelperReleaseIndex>,
    ) -> Result<(), EmbedError> {
        self.retained
            .extend(functions.iter().map(|(function, _)| *function));
        self.registry
            .register_validated(functions)
            .map_err(|error| EmbedError::Registry(error.to_string()))?;
        self.releases = releases;
        Ok(())
    }

    /// Registers the helpers this build changes over the release it
    /// retains under their names, keeping the functions they replace under
    /// none: the change its parent makes too
    /// ([`lash_vm_library::changed_helpers`]).
    ///
    /// # Errors
    ///
    /// [`EmbedError::Registry`]: a defect of the build.
    fn change_helpers(&mut self) -> Result<(), EmbedError> {
        let named = self
            .registry
            .iter()
            .map(|(function, _)| *function)
            .filter(|function| !self.retained.contains(function))
            .collect();
        let changed = lash_vm_library::changed_helpers(named, &self.registry)
            .map_err(|error| EmbedError::Registry(error.to_string()))?;
        for function in changed {
            self.registry
                .register(function.definition, None)
                .map_err(|error| EmbedError::Registry(error.to_string()))?;
            self.retained.insert(function.from);
        }
        Ok(())
    }

    /// Installs a dialect: registers its functions, in the order the
    /// package lists them, and keeps its front end and printer.
    ///
    /// # Errors
    ///
    /// [`EmbedError::DuplicateDialect`], or [`EmbedError::Registry`] when a
    /// function is refused.
    pub fn install(&mut self, mut package: Package) -> Result<(), EmbedError> {
        if self.dialects.contains_key(&package.dialect) {
            return Err(EmbedError::DuplicateDialect {
                dialect: package.dialect,
            });
        }
        for definition in std::mem::take(&mut package.functions) {
            self.registry
                .register(definition, None)
                .map_err(|error| EmbedError::Registry(error.to_string()))?;
        }
        self.dialects.insert(package.dialect.clone(), package);
        Ok(())
    }

    /// The finished embedding.
    ///
    /// # Errors
    ///
    /// [`EmbedError::Registry`].
    pub fn finish(self) -> Result<Embedding, EmbedError> {
        // The names a front end resolves are those of the version it
        // writes. A successor's functions are redeclared when a document
        // of it first runs (`Embedding::registry`).
        Ok(Embedding {
            library: self.library()?,
            written: Arc::new(self.registry),
            interpreted: OnceLock::new(),
            prepared_written: OnceLock::new(),
            prepared_interpreted: OnceLock::new(),
            dialects: self.dialects,
            writes: self.writes,
            releases: self
                .releases
                .into_iter()
                .map(|release| (release, OnceLock::new()))
                .collect(),
        })
    }
}

/// The TypeScript helpers as the build defined them against the kernel
/// library and lash's extensions, validated in a registry holding exactly
/// those (`build.rs`).
const TYPESCRIPT_HELPERS: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/typescript_helpers.json"));

/// The functions of the helper releases the build retains that it does not
/// define itself, validated in a registry holding exactly the kernel
/// library, lash's extensions and the TypeScript helpers (`build.rs`).
const RETAINED_FUNCTIONS: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/retained_functions.json"));

/// The helper releases the build retains, oldest first (`build.rs`).
const HELPER_RELEASES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/helper_releases.json"));

/// The embedding lash ships: the kernel library, lash's extensions and the
/// TypeScript dialect, whose helpers it registers as the build defined
/// them. A worker that starts defines none.
///
/// # Errors
///
/// [`EmbedError`].
pub fn standard(tuning: &WorkerTuning) -> Result<Embedding, EmbedError> {
    let mut embedder = Embedder::kernel()?;
    crate::library::register_extensions(embedder.registry()).map_err(EmbedError::Registry)?;
    let helpers = ValidatedFunctions::from_json(TYPESCRIPT_HELPERS)
        .map_err(|error| EmbedError::Registry(error.to_string()))?;
    embedder
        .registry()
        .register_validated(helpers)
        .map_err(|error| EmbedError::Registry(error.to_string()))?;
    let retained = ValidatedFunctions::from_json(RETAINED_FUNCTIONS)
        .map_err(|error| EmbedError::Registry(error.to_string()))?;
    embedder.retain(retained, helper_release_index()?)?;
    embedder.change_helpers()?;
    embedder.install(typescript_package(tuning, Vec::new()))?;
    embedder.finish()
}

/// The helper releases the standard embedding retains, oldest first, as
/// the build indexed them: the last is the release the tree builds
/// (FIG-5799).
///
/// # Errors
///
/// [`EmbedError::Registry`] when the build's index does not decode: a
/// defect of the build.
fn helper_release_index() -> Result<Vec<HelperReleaseIndex>, EmbedError> {
    serde_json::from_slice(HELPER_RELEASES).map_err(|error| EmbedError::Registry(error.to_string()))
}

/// The TypeScript dialect, its helpers defined against `library`.
///
/// # Errors
///
/// [`EmbedError::Dialect`] when a helper names a function `library` lacks.
pub fn typescript(library: &NamedLibrary, tuning: &WorkerTuning) -> Result<Package, EmbedError> {
    #[cfg(test)]
    tests::HELPER_DEFINITIONS.with(|defined| defined.set(defined.get() + 1));
    let mut library = library.clone();
    let functions = lash_dialect_typescript::define_helpers(&mut library).map_err(|error| {
        EmbedError::Dialect {
            dialect: TYPESCRIPT.to_owned(),
            message: error.to_string(),
        }
    })?;
    Ok(typescript_package(tuning, functions))
}

/// The TypeScript dialect with `functions` as its helpers.
fn typescript_package(tuning: &WorkerTuning, functions: Vec<FunctionDefinition>) -> Package {
    Package {
        dialect: TYPESCRIPT.to_owned(),
        front_end: Box::new(TypeScript {
            parser: Mutex::new(lash_dialect_typescript::Parser::with_stack(
                lash_dialect_typescript::ParserStack {
                    base_bytes: tuning.parser_stack_base_bytes,
                    bytes_per_source_byte: tuning.parser_stack_bytes_per_source_byte,
                },
            )),
        }),
        printer: Some(Box::new(TypeScriptPrinter)),
        function_values: Some(lash_dialect_typescript::function_values()),
        functions,
    }
}

const TYPESCRIPT: &str = "typescript";

/// The TypeScript front end on one reused parser thread, whose stack the
/// parent's working policy sizes.
struct TypeScript {
    parser: Mutex<lash_dialect_typescript::Parser>,
}

impl FrontEnd for TypeScript {
    fn lower(&self, source: &str, environment: &Environment<'_>) -> Result<Lowered, Diagnostic> {
        self.parser
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .lower(source, environment)
            .map_err(Into::into)
    }
}

struct TypeScriptPrinter;

impl Printer for TypeScriptPrinter {
    fn print(&self, document: &Document, _library: &dyn Library) -> Result<String, Diagnostic> {
        lash_dialect_typescript::print(document)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// How many times this thread defined the TypeScript helpers.
        pub(super) static HELPER_DEFINITIONS: std::cell::Cell<usize> =
            const { std::cell::Cell::new(0) };
    }

    /// V25: a fixed assembly failure is initialized once, just like a successful catalog.
    #[test]
    fn worker_catalog_initialization_retains_a_build_failure() {
        let mut embedding = standard(&WorkerTuning::standard()).expect("the standard embedding");
        let mut writes = BTreeSet::new();
        // An invalid released name table is a fixed build defect: resolving
        // it must fail once and leave that failure in the release's slot.
        for result in [1, 2] {
            let definition = lash_kernel_doc::parse_definition(&format!(
                "function helper() -> Int\nkernel 1\ncharge 1\nbody {{ return {result} }}\n"
            ))
            .expect("a helper");
            writes.insert(
                Arc::make_mut(&mut embedding.written)
                    .register(definition, None)
                    .expect("distinct identities"),
            );
        }
        embedding.releases.push((
            HelperReleaseIndex {
                release: "invalid-names".into(),
                ordinal: 99,
                functions: writes.clone(),
                writes,
            },
            OnceLock::new(),
        ));
        for _ in 0..2 {
            assert!(matches!(
                embedding.library_for(99),
                Err(EmbedError::Registry(_))
            ));
            assert!(
                embedding
                    .releases
                    .last()
                    .expect("the release")
                    .1
                    .get()
                    .is_some(),
                "a fixed name-resolution failure is retained inside the slot"
            );
        }
    }

    /// A worker's startup defines no helper: the standard embedding
    /// registers the TypeScript helpers as the build defined them, so a
    /// worker reads no helper source and validates and identifies no helper
    /// before it is ready (FIG-5796).
    #[test]
    fn assembling_the_standard_embedding_defines_no_helper() {
        let defined = HELPER_DEFINITIONS.with(std::cell::Cell::get);
        let embedding = standard(&WorkerTuning::standard()).expect("the standard embedding");
        assert!(embedding.dialects.contains_key("typescript"));
        assert_eq!(
            HELPER_DEFINITIONS.with(std::cell::Cell::get),
            defined,
            "the standard embedding defined the TypeScript helpers"
        );
    }

    /// The helpers the build defined are the ones the TypeScript dialect
    /// defines against the kernel library and lash's extensions, in order
    /// and under the same identities: a worker holds exactly the functions
    /// it would have defined itself.
    #[test]
    fn the_built_helpers_are_the_ones_the_dialect_defines() {
        let mut registry = FunctionRegistry::new();
        crate::library::register_kernel(&mut registry).expect("the kernel library");
        crate::library::register_extensions(&mut registry).expect("lash's extensions");
        let mut library = NamedLibrary::from_registry(&registry).expect("the library");
        let defined: Vec<_> = lash_dialect_typescript::define_helpers(&mut library)
            .expect("the TypeScript helpers")
            .into_iter()
            .map(|definition| (definition.identity().expect("an identity"), definition))
            .collect();
        let built = ValidatedFunctions::from_json(TYPESCRIPT_HELPERS).expect("the built helpers");
        let built: Vec<_> = built
            .iter()
            .map(|(function, definition)| (*function, definition.clone()))
            .collect();
        assert_eq!(built.len(), defined.len(), "the same number of helpers");
        assert!(
            built == defined,
            "the built helpers differ from the defined ones"
        );
    }

    /// Target 5: the worker registers every function before a document
    /// arrives, and a document resolves against nothing else. The standard
    /// embedding holds the kernel library, the machine's functions, the
    /// regular-expression extension and the TypeScript dialect's helpers,
    /// each under one name, and the functions of the helper releases it
    /// retains under none (FIG-5799), each once for every kernel version
    /// the build interprets: a build that interprets a version's successor
    /// holds each function redeclared for it too (FIG-5793), from when a
    /// document of the successor first needs them (FIG-5796).
    #[test]
    fn the_standard_embedding_registers_every_function_at_startup() {
        let embedding = standard(&WorkerTuning::standard()).expect("the standard embedding");
        let registry = embedding.registry().expect("every version's functions");
        let names: Vec<String> = embedding
            .library()
            .iter()
            .map(|(name, _)| name.to_string())
            .collect();
        for name in [
            "num.add",
            "text.concat",
            "collection.map",
            "deref",
            "tasks.unfinished",
            "regex.ecma.exec",
            "ts.number.parse",
        ] {
            assert!(
                names.iter().any(|known| known == name),
                "{name} is registered"
            );
        }
        let written = embedding
            .registry_for(lash_kernel_doc::KERNEL_VERSION)
            .expect("the functions as registered");
        let named: BTreeSet<FunctionId> = embedding.library().iter().map(|(_, id)| *id).collect();
        assert!(
            named.iter().all(|function| written.get(function).is_some()),
            "every name resolves to a registered function"
        );
        for version in lash_kernel_doc::KernelVersion::ALL {
            let held = registry
                .iter()
                .filter(|(_, function)| function.definition.kernel == version.number())
                .count();
            assert_eq!(
                held,
                written.len(),
                "kernel version {version} holds every registered function once"
            );
        }
        assert_eq!(
            registry.iter().count(),
            written.len() * lash_kernel_doc::KernelVersion::ALL.len(),
            "every registered function is of a version the build interprets"
        );
        assert!(embedding.dialects.contains_key("typescript"));
    }

    /// A parent holds exactly the functions a worker holds, without
    /// linking a dialect or an extension (FIG-5812): the registry it reads
    /// from the retained helper releases is the standard embedding's, every
    /// function under the same identity and definition, for every kernel
    /// version the build interprets. It is the union of every retained
    /// release, so a run parked on an earlier release's helpers resolves on
    /// the parent too (FIG-5799). The parent assembles it once in a process:
    /// every session it opens links against the same one (FIG-5759).
    #[test]
    fn a_parent_holds_exactly_the_functions_a_worker_holds() {
        let parent = lash_vm_library::standard_functions().expect("the parent's functions");
        let again = lash_vm_library::standard_functions().expect("the parent's functions");
        assert!(Arc::ptr_eq(&parent, &again), "assembled once in a process");
        let embedding = standard(&WorkerTuning::standard()).expect("the standard embedding");
        let worker = embedding.registry().expect("every version's functions");
        let definitions =
            |registry: &FunctionRegistry| -> BTreeMap<FunctionId, FunctionDefinition> {
                registry
                    .iter()
                    .map(|(function, registered)| {
                        (*function, FunctionDefinition::clone(&registered.definition))
                    })
                    .collect()
            };
        let held = definitions(&parent);
        assert!(
            held == definitions(worker),
            "the parent's functions differ from the worker's"
        );
        for release in lash_vm_library::standard_helper_releases().expect("the releases") {
            for function in &release.functions {
                assert!(
                    held.contains_key(function),
                    "the parent holds {function} of helper release {}",
                    release.release
                );
            }
        }
    }

    /// A worker that starts redeclares nothing for a successor version: a
    /// document written in the version the dialects lower runs against the
    /// functions as registered, and the successor's are redeclared when one
    /// of its documents first needs them (FIG-5796).
    #[cfg(feature = "synthetic-next")]
    #[test]
    fn a_successor_is_redeclared_when_a_document_of_it_first_needs_it() {
        let embedding = standard(&WorkerTuning::standard()).expect("the standard embedding");
        let written = embedding
            .registry_for(lash_kernel_doc::KERNEL_VERSION)
            .expect("the functions as registered");
        assert!(Arc::ptr_eq(written, &embedding.written));
        assert!(
            embedding.interpreted.get().is_none(),
            "nothing is redeclared before a successor's document runs"
        );
        let next = embedding
            .registry_for(lash_kernel_doc::KernelVersion::SyntheticNext.number())
            .expect("the successor's functions");
        assert!(Arc::ptr_eq(
            next,
            embedding.registry().expect("every version's functions")
        ));
        assert_eq!(
            next.len(),
            2 * written.len(),
            "each function once per version"
        );
    }

    /// The host flow cells under `examples/typescript-host-flows/` lower as a
    /// host lowers a cell, against a catalogue that offers their tools, so a
    /// retired spelling fails here instead of rotting in the example.
    #[test]
    fn the_typescript_host_flow_examples_lower_against_their_host_catalogue() {
        use lash_kernel_doc::{EffectName, Name, Signature};
        use std::collections::BTreeSet;

        let embedding = standard(&WorkerTuning::standard()).expect("the standard embedding");
        let tool = Signature {
            params: vec![lash_kernel_doc::Param {
                name: Name::new("input"),
                ty: lash_kernel_doc::Type::Any,
                optional: false,
            }],
            result: lash_kernel_doc::Type::Any,
        };
        let effects: BTreeMap<_, _> = [
            "web.fetch",
            "host.approval",
            "processes.start",
            "control.finish",
        ]
        .into_iter()
        .map(|name| (EffectName::new(name).expect("a tool's name"), tool.clone()))
        .collect();
        let controls = BTreeMap::from([(
            EffectName::new("control.finish").expect("a tool's name"),
            BTreeSet::from([lash_kernel_dialect::EffectControl::Finish]),
        )]);
        let tool_roots: BTreeSet<Name> = ["web", "host", "processes", "control"]
            .into_iter()
            .map(Name::new)
            .collect();
        let bindings = BTreeSet::new();
        let functions = BTreeMap::new();
        for (cell, source) in [
            (
                "turn.ts",
                include_str!("../../../examples/typescript-host-flows/turn.ts"),
            ),
            (
                "durable-process.ts",
                include_str!("../../../examples/typescript-host-flows/durable-process.ts"),
            ),
        ] {
            let lowered = embedding
                .lower(
                    TYPESCRIPT,
                    source,
                    &lash_kernel_dialect::Environment {
                        library: embedding.library(),
                        effects: &effects,
                        tool_roots: &tool_roots,
                        controls: &controls,
                        bindings: &bindings,
                        functions: &functions,
                    },
                )
                .expect("the TypeScript dialect is installed")
                .unwrap_or_else(|diagnostic| panic!("{cell} does not lower: {diagnostic:?}"));
            lash_kernel_doc::validate_document(
                &lowered.document,
                embedding
                    .registry()
                    .expect("the standard functions")
                    .as_ref(),
            )
            .unwrap_or_else(|invalid| panic!("{cell} lowers to an invalid document: {invalid}"));
        }
    }
}

#[cfg(test)]
mod release_tests;
