//! What a worker can run: the function registry it assembles when it
//! starts, and the dialects it lowers and prints.
//!
//! A document names every function it uses by content identity and carries
//! none of them (kernel spec §9 rule 2), so the registry is fixed before the
//! first document arrives: the kernel library, the machine's own functions,
//! each extension crate's, and each dialect's helpers. The standard
//! embedding's TypeScript helpers are defined once, when the crate is built
//! (`build.rs`); a worker registers them as built (FIG-5796).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use lash_kernel_dialect::{
    Diagnostic, Environment, FrontEnd, Library, Lowered, NamedLibrary, Package, Printer,
};
use lash_kernel_doc::{Document, FunctionDefinition, FunctionRegistry, ValidatedFunctions};
use lash_vm_client::WorkerTuning;

/// Why a worker could not assemble what it runs. The worker refuses to
/// start: it is a defect of the build, never of a document.
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("the function registry refused a definition: {0}")]
    Registry(String),
    #[error("the dialect `{dialect}` is installed twice")]
    DuplicateDialect { dialect: String },
    #[error("the dialect `{dialect}` could not be installed: {message}")]
    Dialect { dialect: String, message: String },
}

/// Everything a worker registered at startup.
pub struct Embedding {
    /// The functions as registered, each written for the kernel version
    /// the dialects lower to.
    written: Arc<FunctionRegistry>,
    /// `written` and each function redeclared for every successor version
    /// the build interprets, made when first asked for: a worker that runs
    /// no successor's document never redeclares (FIG-5796).
    interpreted: OnceLock<Arc<FunctionRegistry>>,
    pub(crate) library: NamedLibrary,
    pub(crate) dialects: BTreeMap<String, Package>,
}

impl Embedding {
    /// Every registered function, by name: what a document's manifest
    /// resolves its names against.
    pub fn library(&self) -> &NamedLibrary {
        &self.library
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
        if let Some(registry) = self.interpreted.get() {
            return Ok(registry);
        }
        let mut registry = FunctionRegistry::clone(&self.written);
        let mut redeclared = false;
        for version in lash_kernel_doc::KernelVersion::ALL {
            if let Some(migration) = lash_kernel_migrate::migration_from(*version) {
                lash_kernel_migrate::migrate_registry(&mut registry, migration)
                    .map_err(|error| EmbedError::Registry(error.to_string()))?;
                redeclared = true;
            }
        }
        let registry = if redeclared {
            Arc::new(registry)
        } else {
            Arc::clone(&self.written)
        };
        Ok(self.interpreted.get_or_init(|| registry))
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
        NamedLibrary::from_registry(&self.registry)
            .map_err(|error| EmbedError::Registry(error.to_string()))
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
            dialects: self.dialects,
        })
    }
}

/// The TypeScript helpers as the build defined them against the kernel
/// library and lash's extensions, validated in a registry holding exactly
/// those (`build.rs`).
const TYPESCRIPT_HELPERS: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/typescript_helpers.json"));

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
    embedder.install(typescript_package(tuning, Vec::new()))?;
    embedder.finish()
}

/// The function registry of [`standard`], assembled once in this process:
/// what a parent links and admits documents against for workers assembled
/// as lash ships them. The registry is the same under every tuning, which
/// sizes only the parser's stack.
///
/// # Errors
///
/// [`EmbedError`].
pub fn standard_functions() -> Result<Arc<FunctionRegistry>, EmbedError> {
    static FUNCTIONS: OnceLock<Arc<FunctionRegistry>> = OnceLock::new();
    if let Some(functions) = FUNCTIONS.get() {
        return Ok(Arc::clone(functions));
    }
    let functions = Arc::clone(standard(&WorkerTuning::standard())?.registry()?);
    Ok(Arc::clone(FUNCTIONS.get_or_init(|| functions)))
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
    /// each under one name, once for every kernel version the build
    /// interprets: a build that interprets a version's successor holds each
    /// function redeclared for it too (FIG-5793), from when a document of
    /// the successor first needs them (FIG-5796).
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
        let mut library = names.clone();
        library.sort();
        for version in lash_kernel_doc::KernelVersion::ALL {
            let mut held: Vec<String> = registry
                .iter()
                .filter(|(_, function)| function.definition.kernel == version.number())
                .map(|(_, function)| function.definition.name.to_string())
                .collect();
            held.sort();
            assert_eq!(
                held, library,
                "kernel version {version} holds every function once, under its one name"
            );
        }
        assert_eq!(
            registry.iter().count(),
            names.len() * lash_kernel_doc::KernelVersion::ALL.len(),
            "every registered function is of a version the build interprets"
        );
        assert!(embedding.dialects.contains_key("typescript"));
    }

    /// A parent assembles the shipped registry once: every session it
    /// opens links against the same one, however many it opens (FIG-5759).
    #[test]
    fn the_standard_functions_are_assembled_once_in_a_process() {
        let first = standard_functions().expect("the standard functions");
        let again = standard_functions().expect("the standard functions");
        assert!(Arc::ptr_eq(&first, &again));
        let embedding = standard(&WorkerTuning::standard()).expect("the standard embedding");
        assert_eq!(
            first.iter().count(),
            embedding
                .registry()
                .expect("every version's functions")
                .iter()
                .count(),
            "the registry a worker assembles"
        );
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
}
