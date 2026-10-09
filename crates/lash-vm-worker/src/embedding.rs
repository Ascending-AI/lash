//! What a worker can run: the function registry it assembles when it
//! starts, and the dialects it lowers and prints.
//!
//! A document names every function it uses by content identity and carries
//! none of them (kernel spec §9 rule 2), so the registry is fixed before the
//! first document arrives: the kernel library, the machine's own functions,
//! each extension crate's, and each dialect's helpers.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};

use lash_kernel_dialect::{
    Diagnostic, Environment, FrontEnd, Library, Lowered, NamedLibrary, Package, Printer,
};
use lash_kernel_doc::{Document, EffectName, FunctionRegistry, Name, Signature};
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
    pub(crate) registry: Arc<FunctionRegistry>,
    pub(crate) library: NamedLibrary,
    pub(crate) dialects: BTreeMap<String, Package>,
}

impl Embedding {
    /// Every registered function, by name: what a document's manifest
    /// resolves its names against.
    pub fn library(&self) -> &NamedLibrary {
        &self.library
    }

    /// The registry a machine runs documents against.
    pub fn registry(&self) -> &Arc<FunctionRegistry> {
        &self.registry
    }

    /// Lowers `source` as the installed dialect `dialect` does for a cell:
    /// against this embedding's library, the effects a host offers and the
    /// session bindings in scope. `None` when no such dialect is installed.
    ///
    /// A host calls it to learn whether text it shows or stores lowers,
    /// without a worker or a session.
    pub fn lower(
        &self,
        dialect: &str,
        source: &str,
        effects: &BTreeMap<EffectName, Signature>,
        bindings: &BTreeSet<Name>,
    ) -> Option<Result<Lowered, Diagnostic>> {
        let package = self.dialects.get(dialect)?;
        Some(package.front_end.lower(
            source,
            &Environment {
                library: &self.library,
                effects,
                bindings,
            },
        ))
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
        let refused = |error: &dyn std::fmt::Display| EmbedError::Registry(error.to_string());
        lash_kernel_lib::register_numbers(&mut registry).map_err(|error| refused(&error))?;
        lash_kernel_lib::register_text_json(&mut registry).map_err(|error| refused(&error))?;
        lash_kernel_vm::register_machine_functions(&mut registry)
            .map_err(|error| refused(&error))?;
        lash_kernel_lib::register_collections(&mut registry).map_err(|error| refused(&error))?;
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
        Ok(Embedding {
            library: self.library()?,
            registry: Arc::new(self.registry),
            dialects: self.dialects,
        })
    }
}

/// How many compiled patterns the regular-expression extension keeps.
const CACHED_PATTERNS: usize = 256;

/// The embedding lash ships: the kernel library, the ECMAScript
/// regular-expression extension and the TypeScript dialect.
///
/// # Errors
///
/// [`EmbedError`].
pub fn standard(tuning: &WorkerTuning) -> Result<Embedding, EmbedError> {
    let mut embedder = Embedder::kernel()?;
    lash_ext_regex_ecma::register(
        embedder.registry(),
        &Arc::new(lash_ext_regex_ecma::Engine::new(CACHED_PATTERNS)),
    )
    .map_err(|error| EmbedError::Registry(error.to_string()))?;
    lash_ext_date_ecma::register(embedder.registry())
        .map_err(|error| EmbedError::Registry(error.to_string()))?;
    lash_ext_url_whatwg::register(embedder.registry())
        .map_err(|error| EmbedError::Registry(error.to_string()))?;
    embedder.install(typescript(&embedder.library()?, tuning)?)?;
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
    let functions = Arc::clone(standard(&WorkerTuning::standard())?.registry());
    Ok(Arc::clone(FUNCTIONS.get_or_init(|| functions)))
}

/// The TypeScript dialect, its helpers defined against `library`.
///
/// # Errors
///
/// [`EmbedError::Dialect`] when a helper names a function `library` lacks.
pub fn typescript(library: &NamedLibrary, tuning: &WorkerTuning) -> Result<Package, EmbedError> {
    let mut library = library.clone();
    let functions = lash_dialect_typescript::define_helpers(&mut library).map_err(|error| {
        EmbedError::Dialect {
            dialect: TYPESCRIPT.to_owned(),
            message: error.to_string(),
        }
    })?;
    Ok(Package {
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
    })
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

    /// Target 5: the worker registers every function before a document
    /// arrives, and a document resolves against nothing else. The standard
    /// embedding holds the kernel library, the machine's functions, the
    /// regular-expression extension and the TypeScript dialect's helpers,
    /// each under one name.
    #[test]
    fn the_standard_embedding_registers_every_function_at_startup() {
        let embedding = standard(&WorkerTuning::standard()).expect("the standard embedding");
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
        assert_eq!(
            embedding.registry().iter().count(),
            names.len(),
            "every registered function has one name"
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
            embedding.registry().iter().count(),
            "the registry a worker assembles"
        );
    }
}
