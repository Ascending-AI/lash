//! What a worker can run: the function registry it assembles when it
//! starts, and the dialects it lowers and prints.
//!
//! A host names its dialects: there is no embedding it gets without
//! choosing (FIG-5990). It builds one from the kernel library and each
//! dialect it installs, `Embedder::kernel()?.dialect(typescript(tuning))?
//! .dialect(python())?.finish()`, and an embedding with no dialect is
//! refused. Each dialect is a build feature of this crate
//! (`dialect-typescript`, `dialect-python`), off by default: a host that
//! enables one compiles none of the other's parser.
//!
//! A document names every function it uses by content identity and carries
//! none of them (kernel spec §9 rule 2), so the registry is fixed before the
//! first document arrives: the kernel library, the machine's own functions,
//! and each installed dialect's extensions and helpers. A dialect's helpers
//! are defined once, when `lash-vm-releases` is built; a worker registers
//! them as built (FIG-5796).
//!
//! The embedding also holds every function of the helper releases each of
//! its dialects retains that it does not define itself (FIG-5799,
//! `lash-vm-releases`): a run parked under an earlier release's helpers,
//! and a saved function written against them, resume on exactly the
//! functions they pin. Names resolve against the embedding's own functions
//! alone, or, for a writer the fleet holds to an earlier release, against
//! that release's.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::{Diagnostic, Environment, Lowered, NamedLibrary, Package};
use lash_kernel_doc::{FunctionId, FunctionRegistry, ValidatedFunctions};
use lash_kernel_vm::PreparedLibrary;
use lash_vm_client::graph::ReleaseGraph;
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
    /// The host named no dialect. An embedding has no default one: the host
    /// installs each dialect its sessions and processes are written in.
    #[error(
        "the embedding installs no dialect; name each one the host runs, as in \
         `Embedder::kernel()?.dialect(typescript(tuning))?.finish()`"
    )]
    NoDialect,
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
    /// for all the runs of the worker: `written` before the worker is
    /// ready, `interpreted` when a successor's document first runs.
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
    /// The manifests of the helper releases the build holds, by graph
    /// reference: what a parked run pins (FIG-5887 §3), staged when first
    /// asked for.
    manifests: OnceLock<Result<ReleaseGraph, EmbedError>>,
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

    /// The dialects the host installed, by name.
    pub fn dialects(&self) -> impl Iterator<Item = &str> {
        self.dialects.keys().map(String::as_str)
    }

    /// Whether the host installed `dialect`: a worker runs a session or a
    /// process only in a dialect its embedding installs.
    #[must_use]
    pub fn installs(&self, dialect: &str) -> bool {
        self.dialects.contains_key(dialect)
    }

    /// The manifests of the helper releases the build holds: a parked run
    /// resumes only on the exact one it pins.
    ///
    /// # Errors
    ///
    /// [`EmbedError::Registry`] when the build's releases do not stage: a
    /// defect of the build.
    pub(crate) fn release_manifests(&self) -> Result<&ReleaseGraph, EmbedError> {
        cached(&self.manifests, || {
            lash_vm_client::graph::release_manifests()
                .map_err(|error| EmbedError::Registry(error.to_string()))
        })
    }

    /// The released helper sets the embedding holds besides its own
    /// functions, oldest first: each release as every installed dialect
    /// holds it, together.
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

/// A dialect a host installs in its worker embedding: its front end and
/// printer, the library it ships with beyond the kernel's, and its helpers
/// as the build defined them ([`typescript`], [`python`]).
pub struct Dialect {
    name: &'static str,
    package: Result<Package, EmbedError>,
    /// Registers the functions the dialect's helpers call beyond the
    /// kernel library's.
    extensions: Option<RegisterExtensions>,
}

/// Registers a dialect's extension functions.
type RegisterExtensions = fn(&mut FunctionRegistry) -> Result<(), String>;

impl Dialect {
    /// The dialect's name: what a session records and a run names.
    #[must_use]
    pub fn name(&self) -> &str {
        self.name
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
    /// Each retained release, by ordinal, as the dialects installed so far
    /// hold it.
    releases: BTreeMap<u32, HelperReleaseIndex>,
}

impl Embedder {
    /// The kernel library and the machine's own functions: what every
    /// embedding holds before the host names a dialect.
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
            releases: BTreeMap::new(),
        })
    }

    /// The registry, for an extension crate to register its functions in.
    pub fn registry(&mut self) -> &mut FunctionRegistry {
        &mut self.registry
    }

    /// Every function registered so far, by name.
    ///
    /// # Errors
    ///
    /// [`EmbedError::Registry`] when two functions share a name.
    pub fn library(&self) -> Result<NamedLibrary, EmbedError> {
        NamedLibrary::resolving(&self.registry, |function| !self.retained.contains(function))
            .map_err(|error| EmbedError::Registry(error.to_string()))
    }

    /// Installs `dialect`: registers the library it ships with, its helpers
    /// as the build defined them, and the functions of the helper releases
    /// the build retains for it, and keeps its front end and printer.
    ///
    /// # Errors
    ///
    /// [`EmbedError::DuplicateDialect`]; [`EmbedError::Dialect`] when the
    /// build holds no helper release of the dialect; [`EmbedError::Registry`]
    /// when a function is refused.
    pub fn dialect(mut self, dialect: Dialect) -> Result<Self, EmbedError> {
        let Dialect {
            name: _,
            package,
            extensions,
        } = dialect;
        let package = package?;
        if self.dialects.contains_key(&package.dialect) {
            return Err(EmbedError::DuplicateDialect {
                dialect: package.dialect,
            });
        }
        let built =
            lash_vm_releases::dialect(&package.dialect).ok_or_else(|| EmbedError::Dialect {
                dialect: package.dialect.clone(),
                message: "the build holds no helper release of it".to_owned(),
            })?;
        let registry = |error: &dyn std::fmt::Display| EmbedError::Registry(error.to_string());
        if let Some(extensions) = extensions {
            extensions(&mut self.registry).map_err(EmbedError::Registry)?;
        }
        let helpers =
            ValidatedFunctions::from_json(built.helpers).map_err(|error| registry(&error))?;
        self.registry
            .register_validated(helpers)
            .map_err(|error| registry(&error))?;
        // The functions of the releases the dialect retains, validated
        // against exactly what it holds now, under no name: they are found
        // by identity alone (FIG-5799).
        let retained = ValidatedFunctions::from_json(built.retained_functions)
            .map_err(|error| registry(&error))?;
        self.retained
            .extend(retained.iter().map(|(function, _)| *function));
        self.registry
            .register_validated(retained)
            .map_err(|error| registry(&error))?;
        let releases: Vec<HelperReleaseIndex> =
            serde_json::from_slice(built.helper_release_index).map_err(|error| registry(&error))?;
        for release in releases {
            match self.releases.entry(release.ordinal) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(release);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let held = entry.get_mut();
                    held.writes.extend(release.writes);
                    held.functions.extend(release.functions);
                }
            }
        }
        self.dialects.insert(package.dialect.clone(), package);
        Ok(self)
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

    /// The finished embedding.
    ///
    /// # Errors
    ///
    /// [`EmbedError::NoDialect`] when the host installed none, and
    /// [`EmbedError::Registry`].
    pub fn finish(mut self) -> Result<Embedding, EmbedError> {
        if self.dialects.is_empty() {
            return Err(EmbedError::NoDialect);
        }
        self.change_helpers()?;
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
                .into_values()
                .map(|release| (release, OnceLock::new()))
                .collect(),
            manifests: OnceLock::new(),
        })
    }
}

/// The TypeScript dialect, its parser's stack sized by the parent's working
/// policy. It ships with lash's extensions: ECMAScript regular expressions
/// and dates, and WHATWG URLs.
#[cfg(feature = "dialect-typescript")]
#[must_use]
pub fn typescript(tuning: &lash_vm_client::WorkerTuning) -> Dialect {
    Dialect {
        name: lash_vm_releases::TYPESCRIPT,
        package: typescript::FrontEnd::new(tuning).map(|front_end| Package {
            dialect: lash_vm_releases::TYPESCRIPT.to_owned(),
            front_end: Box::new(front_end),
            printer: Some(Box::new(typescript::Printer)),
            function_values: Some(lash_dialect_typescript::function_values()),
            functions: Vec::new(),
        }),
        extensions: Some(crate::library::register_extensions),
    }
}

/// The Python dialect. Its helpers call the kernel library alone.
#[cfg(feature = "dialect-python")]
#[must_use]
pub fn python() -> Dialect {
    python_with_limits(lash_dialect_python::SourceLimits::standard())
}

/// The Python dialect under the host's validated source admission policy.
#[cfg(feature = "dialect-python")]
#[must_use]
pub fn python_with_limits(limits: lash_dialect_python::SourceLimits) -> Dialect {
    let mut package = lash_dialect_python::package(Vec::new());
    package.front_end = Box::new(lash_dialect_python::Python { limits });
    Dialect {
        name: lash_vm_releases::PYTHON,
        package: Ok(package),
        extensions: None,
    }
}

#[cfg(feature = "dialect-typescript")]
mod typescript {
    use std::sync::Mutex;

    use lash_kernel_dialect::{Diagnostic, Environment, Lowered};
    use lash_kernel_doc::Document;

    /// The TypeScript front end on one reused parser thread, whose stack
    /// the parent's working policy sizes.
    pub(super) struct FrontEnd {
        parser: Mutex<lash_dialect_typescript::Parser>,
    }

    impl FrontEnd {
        pub(super) fn new(
            tuning: &lash_vm_client::WorkerTuning,
        ) -> Result<Self, super::EmbedError> {
            let limits = lash_dialect_typescript::SourceLimits::new(
                tuning.typescript_source_bytes,
                tuning.typescript_source_nesting,
            )
            .map_err(|error| super::EmbedError::Dialect {
                dialect: lash_vm_releases::TYPESCRIPT.to_owned(),
                message: error.to_string(),
            })?;
            Ok(Self {
                parser: Mutex::new(
                    lash_dialect_typescript::Parser::with_stack(
                        lash_dialect_typescript::ParserStack {
                            base_bytes: tuning.parser_stack_base_bytes,
                            bytes_per_source_byte: tuning.parser_stack_bytes_per_source_byte,
                        },
                    )
                    .with_limits(limits),
                ),
            })
        }
    }

    impl lash_kernel_dialect::FrontEnd for FrontEnd {
        fn lower(
            &self,
            source: &str,
            environment: &Environment<'_>,
        ) -> Result<Lowered, Diagnostic> {
            self.parser
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .lower(source, environment)
                .map_err(Into::into)
        }
    }

    pub(super) struct Printer;

    impl lash_kernel_dialect::Printer for Printer {
        fn print(
            &self,
            document: &Document,
            options: lash_kernel_dialect::PrintOptions,
        ) -> Result<lash_kernel_dialect::PrintedSource, lash_kernel_dialect::PrintError> {
            lash_dialect_typescript::print(document, options)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lash_kernel_doc::FunctionDefinition;
    use lash_vm_client::WorkerTuning;

    /// A host that names TypeScript alone.
    pub(crate) fn typescript_only(tuning: &WorkerTuning) -> Result<Embedding, EmbedError> {
        Embedder::kernel()?.dialect(typescript(tuning))?.finish()
    }

    /// A host that names every dialect lash ships.
    pub(crate) fn every_dialect(tuning: &WorkerTuning) -> Result<Embedding, EmbedError> {
        Embedder::kernel()?
            .dialect(typescript(tuning))?
            .dialect(python())?
            .finish()
    }

    /// FIG-5990: an embedding has no default dialect. A host that names
    /// none is refused when it finishes, by a typed error, and one that
    /// names a dialect twice is refused at the second.
    #[test]
    fn an_embedding_with_no_dialect_is_refused() {
        assert!(matches!(
            Embedder::kernel().expect("the kernel library").finish(),
            Err(EmbedError::NoDialect)
        ));
        assert!(matches!(
            Embedder::kernel()
                .expect("the kernel library")
                .dialect(python())
                .expect("the Python dialect")
                .dialect(python()),
            Err(EmbedError::DuplicateDialect { dialect }) if dialect == "python"
        ));
    }

    /// FIG-5990: an embedding holds the dialects its host named and no
    /// other: a TypeScript-only one installs no Python front end and holds
    /// none of Python's helpers, a Python-only one holds none of
    /// TypeScript's or of its extensions, and one with both holds each.
    #[test]
    fn an_embedding_holds_only_the_dialects_its_host_named() {
        let tuning = WorkerTuning::standard();
        let helpers = |dialect: &str| -> BTreeSet<FunctionId> {
            let built = lash_vm_releases::dialect(dialect).expect("the built dialect");
            ValidatedFunctions::from_json(built.helpers)
                .expect("the built helpers")
                .iter()
                .map(|(function, _)| *function)
                .collect()
        };
        let (ts, py) = (helpers("typescript"), helpers("python"));
        let holds = |embedding: &Embedding, functions: &BTreeSet<FunctionId>| {
            functions
                .iter()
                .filter(|function| embedding.written.get(function).is_some())
                .count()
        };
        let typescript_only = typescript_only(&tuning).expect("a TypeScript embedding");
        assert_eq!(
            typescript_only.dialects().collect::<Vec<_>>(),
            ["typescript"]
        );
        assert!(!typescript_only.installs("python"));
        assert_eq!(holds(&typescript_only, &ts), ts.len());
        assert_eq!(holds(&typescript_only, &py), 0);

        let python_only = Embedder::kernel()
            .and_then(|embedder| embedder.dialect(python()))
            .and_then(Embedder::finish)
            .expect("a Python embedding");
        assert_eq!(python_only.dialects().collect::<Vec<_>>(), ["python"]);
        assert_eq!(holds(&python_only, &py), py.len());
        assert_eq!(holds(&python_only, &ts), 0);
        assert!(
            python_only
                .library()
                .iter()
                .all(|(name, _)| !name.to_string().starts_with("regex.ecma.")),
            "the extensions are the TypeScript dialect's"
        );

        let both = every_dialect(&tuning).expect("an embedding of both");
        assert_eq!(
            both.dialects().collect::<Vec<_>>(),
            ["python", "typescript"]
        );
        assert_eq!(holds(&both, &ts) + holds(&both, &py), ts.len() + py.len());
    }

    /// V25: a fixed assembly failure is initialized once, just like a successful catalog.
    #[test]
    fn worker_catalog_initialization_retains_a_build_failure() {
        let mut embedding =
            typescript_only(&WorkerTuning::standard()).expect("a TypeScript embedding");
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

    /// The helpers the build defined for a dialect are the ones the dialect
    /// defines against the library it ships with (the kernel library, and
    /// for TypeScript lash's extensions), in order and under the same
    /// identities: a worker holds exactly the functions it would have
    /// defined itself, and defines none when it starts (FIG-5796).
    #[test]
    fn the_built_helpers_are_the_ones_each_dialect_defines() {
        let mut registry = FunctionRegistry::new();
        crate::library::register_kernel(&mut registry).expect("the kernel library");
        let mut library = NamedLibrary::from_registry(&registry).expect("the library");
        let python = lash_dialect_python::define_helpers(&mut library).expect("Python's helpers");
        crate::library::register_extensions(&mut registry).expect("lash's extensions");
        let mut library = NamedLibrary::from_registry(&registry).expect("the library");
        let typescript =
            lash_dialect_typescript::define_helpers(&mut library).expect("TypeScript's helpers");
        for (dialect, defined) in [("python", python), ("typescript", typescript)] {
            let defined: Vec<_> = defined
                .into_iter()
                .map(|definition| (definition.identity().expect("an identity"), definition))
                .collect();
            let built = lash_vm_releases::dialect(dialect).expect("the built dialect");
            let built = ValidatedFunctions::from_json(built.helpers).expect("the built helpers");
            let built: Vec<_> = built
                .iter()
                .map(|(function, definition)| (*function, definition.clone()))
                .collect();
            assert_eq!(built.len(), defined.len(), "{dialect}: as many helpers");
            assert!(
                built == defined,
                "{dialect}: the built helpers differ from the defined ones"
            );
        }
    }

    /// Target 5: the worker registers every function before a document
    /// arrives, and a document resolves against nothing else. A TypeScript
    /// embedding holds the kernel library, the machine's functions, the
    /// regular-expression extension and the TypeScript dialect's helpers,
    /// each under one name, and the functions of the helper releases it
    /// retains under none (FIG-5799), each once for every kernel version
    /// the build interprets: a build that interprets a version's successor
    /// holds each function redeclared for it too (FIG-5793), from when a
    /// document of the successor first needs them (FIG-5796).
    #[test]
    fn an_embedding_registers_every_function_at_startup() {
        let embedding = typescript_only(&WorkerTuning::standard()).expect("a TypeScript embedding");
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

    /// A parent holds exactly the functions a worker of every dialect the
    /// build holds does, without linking a dialect or an extension
    /// (FIG-5812): the registry it reads from each built dialect's retained
    /// helper releases is that embedding's, every
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
        let embedding = every_dialect(&WorkerTuning::standard()).expect("an embedding of both");
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
        let embedding = typescript_only(&WorkerTuning::standard()).expect("a TypeScript embedding");
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

        let embedding = typescript_only(&WorkerTuning::standard()).expect("a TypeScript embedding");
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
                    lash_vm_releases::TYPESCRIPT,
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
    /// WORKER-DATA-POLICY: the bootstrap's source admission reaches the installed dialect.
    #[test]
    fn worker_tuning_allows_a_larger_typescript_source() {
        let source = format!("//{}", "x".repeat(64 * 1024));
        let mut tuning = WorkerTuning::standard();
        tuning.typescript_source_bytes = source.len();
        let embedding = typescript_only(&tuning).unwrap();
        let environment = Environment {
            library: embedding.library(),
            effects: &BTreeMap::new(),
            controls: &BTreeMap::new(),
            tool_roots: &BTreeSet::new(),
            bindings: &BTreeSet::new(),
            functions: &BTreeMap::new(),
        };
        embedding
            .lower(lash_vm_releases::TYPESCRIPT, &source, &environment)
            .unwrap()
            .unwrap();
        let mut invalid = tuning;
        invalid.typescript_source_nesting = 29;
        assert!(matches!(
            Embedder::kernel().unwrap().dialect(typescript(&invalid)),
            Err(EmbedError::Dialect { .. })
        ));
        let python_source = format!("#{}", "x".repeat(1 << 20));
        tuning.python_source_bytes = python_source.len();
        let embedding = crate::built_dialects(&tuning).unwrap();
        let environment = Environment {
            library: embedding.library(),
            effects: &BTreeMap::new(),
            controls: &BTreeMap::new(),
            tool_roots: &BTreeSet::new(),
            bindings: &BTreeSet::new(),
            functions: &BTreeMap::new(),
        };
        embedding
            .lower(lash_vm_releases::PYTHON, &python_source, &environment)
            .unwrap()
            .unwrap();
    }
}

#[cfg(test)]
mod release_tests;
