//! What a worker can run: the function registry it derives from its
//! parent's one registration of packages, and the dialects it lowers and
//! prints.
//!
//! A host names its dialects: there is no embedding it gets without
//! choosing (FIG-5990). It configures a worker with natives and dialects
//! only, `Embedder::new().native(id, native)?.dialect(typescript(tuning)?)?`,
//! and binds that configuration once to the packages its parent holds,
//! `.finish(&packages)` (FIG-6917, V1): a worker never lists packages of
//! its own, so it cannot hold a package the parent lacks or miss one it
//! holds. An embedding with no dialect is refused. Each dialect is a build
//! feature of this crate (`dialect-typescript`, `dialect-python`), off by
//! default: a host that enables one compiles none of the other's parser.
//!
//! A document names every function it uses by content identity and carries
//! none of them (kernel spec §9 rule 2), so the registry is fixed before the
//! first document arrives: every held artifact's code, lash's packages'
//! natives and plans, and the host's natives, each by identity. lash's
//! packages are made once, when `lash-vm-releases` is built; a worker
//! registers their images as built (FIG-5796, FIG-6843).
//!
//! The embedding also holds the helper releases its parent's dialects
//! retain (FIG-5799, `lash-vm-releases`): names resolve against the
//! embedding's own functions alone, or, for a writer the fleet holds to an
//! earlier release, against that release's.

use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::{Diagnostic, Dialect, Environment, Lowered, Mode, NamedLibrary};
use lash_kernel_doc::{FunctionId, FunctionRegistry, NativeFunction};
use lash_kernel_package::{HeldPackages, Origin, PackageInstall, PackageRefusal};
use lash_kernel_vm::PreparedLibrary;
use lash_vm_client::graph::ReleaseGraph;
use lash_vm_library::{HELPER_RELEASE, HelperReleaseIndex};

/// Why a worker could not assemble what it runs. The worker refuses to
/// start: it is a defect of the build or of the host's configuration,
/// never of a document.
#[derive(Clone, Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("the function registry refused a definition: {0}")]
    Registry(String),
    /// The packages refused: a `Native`-only function with no native, a
    /// native for no held function, or an artifact the registry refused.
    #[error(transparent)]
    Package(#[from] PackageRefusal),
    #[error("the dialect `{dialect}` is installed twice")]
    DuplicateDialect { dialect: String },
    #[error("the dialect `{dialect}` could not be installed: {message}")]
    Dialect { dialect: String, message: String },
    /// The host named no dialect. An embedding has no default one: the host
    /// installs each dialect its sessions and processes are written in.
    #[error(
        "the embedding installs no dialect; name each one the host runs, as in \
         `Embedder::new().dialect(typescript(tuning)?)?.finish(&packages)`"
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
    /// Prepared indexes of `written` and `interpreted`, shared by every
    /// run of the worker. Each helper body compiles only when it is needed.
    prepared_written: OnceLock<PreparedLibrary>,
    prepared_interpreted: OnceLock<PreparedLibrary>,
    pub(crate) library: NamedLibrary,
    pub(crate) dialects: BTreeMap<String, Dialect>,
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
    /// functions, oldest first: each release as every dialect of the build
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
    /// library bodies compiled on first use.
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

    /// Lowers `source` as the installed dialect `dialect` does in `mode`:
    /// against `environment`'s library, offered effects and namespace
    /// roots, and, for a cell, its session bindings and saved functions.
    /// `None` when no such dialect is installed.
    ///
    /// A host calls it to learn whether text it shows or stores lowers,
    /// without a worker or a session.
    pub fn lower(
        &self,
        dialect: &str,
        source: &str,
        environment: &Environment<'_>,
        mode: Mode<'_>,
    ) -> Option<Result<Lowered, Diagnostic>> {
        let dialect = self.dialects.get(dialect)?;
        Some(dialect.front_end.lower(source, environment, mode))
    }
}

/// A worker's configuration while it is assembled: the natives the host
/// supplies and the dialects it installs. It holds no package: it is bound
/// once to its parent's ([`Embedder::finish`]).
#[derive(Default)]
pub struct Embedder {
    natives: PackageInstall,
    dialects: BTreeMap<String, Dialect>,
}

impl Embedder {
    /// A worker with no host native and no dialect yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Supplies `native` for the held function `function`: the only thing
    /// a host gives a worker of its own packages.
    ///
    /// # Errors
    ///
    /// [`EmbedError::Registry`] when a native for `function` was supplied
    /// already.
    pub fn native(
        mut self,
        function: FunctionId,
        native: Arc<dyn NativeFunction>,
    ) -> Result<Self, EmbedError> {
        if self.natives.natives.insert(function, native).is_some() {
            return Err(EmbedError::Registry(format!(
                "a native for {function} is supplied twice"
            )));
        }
        Ok(self)
    }

    /// Installs `dialect`'s front end, reader and printer.
    ///
    /// # Errors
    ///
    /// [`EmbedError::DuplicateDialect`].
    pub fn dialect(mut self, dialect: Dialect) -> Result<Self, EmbedError> {
        if self.dialects.contains_key(&dialect.name) {
            return Err(EmbedError::DuplicateDialect {
                dialect: dialect.name,
            });
        }
        self.dialects.insert(dialect.name.clone(), dialect);
        Ok(self)
    }

    /// Binds this configuration to `packages`, the parent's one
    /// registration: registers every held artifact's code, lash's natives
    /// and plans and the host's natives, each by identity.
    ///
    /// # Errors
    ///
    /// [`EmbedError::NoDialect`]; [`PackageRefusal::NativeMissing`] for a
    /// `Native`-only function no native is supplied for, and
    /// [`PackageRefusal::NativeUnused`] for a native of no held function
    /// that states one; [`EmbedError::Registry`].
    pub fn finish(self, packages: &HeldPackages) -> Result<Embedding, EmbedError> {
        if self.dialects.is_empty() {
            return Err(EmbedError::NoDialect);
        }
        let mut install = lash_install(packages)?;
        for (function, native) in self.natives.natives {
            if install.natives.insert(function, native).is_some() {
                return Err(EmbedError::Registry(format!(
                    "lash implements {function}; a host supplies no native for it"
                )));
            }
        }
        let mut registry = FunctionRegistry::new();
        packages.register_into(&mut registry, &|function| {
            install.natives.get(function).cloned()
        })?;
        for function in install.natives.keys() {
            if registry
                .get(function)
                .is_none_or(|registered| !registered.states_native())
            {
                return Err(PackageRefusal::NativeUnused {
                    function: *function,
                }
                .into());
            }
        }
        registry
            .install_plans(&install.plans)
            .map_err(|error| EmbedError::Registry(error.to_string()))?;
        if let Some((function, registered)) = registry.iter().find(|(_, registered)| {
            registered.states_native() && !registered.has_body() && registered.native().is_none()
        }) {
            return Err(PackageRefusal::NativeMissing {
                function: *function,
                name: registered.definition().name.clone(),
            }
            .into());
        }
        // The names a front end resolves are those of the version it
        // writes. A successor's functions are redeclared when a document
        // of it first runs (`Embedding::registry`).
        let library = NamedLibrary::from_registry(&registry)
            .map_err(|error| EmbedError::Registry(error.to_string()))?;
        let releases = lash_vm_library::standard_helper_releases()
            .map_err(|error| EmbedError::Registry(error.to_string()))?;
        Ok(Embedding {
            library,
            written: Arc::new(registry),
            interpreted: OnceLock::new(),
            prepared_written: OnceLock::new(),
            prepared_interpreted: OnceLock::new(),
            dialects: self.dialects,
            writes: HELPER_RELEASE,
            releases: releases
                .into_iter()
                .map(|release| (release, OnceLock::new()))
                .collect(),
            manifests: OnceLock::new(),
        })
    }
}

/// The natives and plans of lash's held packages, by identity: each one's
/// crate implements it by name in a registry holding every held artifact,
/// requirements first, and hands it on.
fn lash_install(packages: &HeldPackages) -> Result<PackageInstall, EmbedError> {
    let mut registry = FunctionRegistry::declarations();
    packages.register_into(&mut registry, &|_| None)?;
    let mut functions = Vec::new();
    for package in packages.in_order() {
        if package.origin() == Origin::Lash {
            crate::library::implement(
                &package.path().to_string(),
                &mut registry,
                package.code().ids(),
                package.digest(),
            )
            .map_err(EmbedError::Registry)?;
            functions.extend_from_slice(package.code().ids());
        }
    }
    Ok(PackageInstall::of_registry(&registry, &functions))
}

/// The TypeScript dialect, its parser's stack sized by the parent's working
/// policy. It runs on lash's `typescript` package and the extensions it
/// requires: ECMAScript regular expressions and dates, and WHATWG URLs.
///
/// # Errors
///
/// [`EmbedError::Dialect`] when the policy's source limits are invalid.
#[cfg(feature = "dialect-typescript")]
pub fn typescript(tuning: &lash_vm_client::WorkerTuning) -> Result<Dialect, EmbedError> {
    let front_end = typescript::FrontEnd::new(tuning)?;
    Ok(Dialect {
        name: lash_vm_releases::TYPESCRIPT.to_owned(),
        front_end: Box::new(front_end),
        read: |source, base, limits| {
            lash_dialect_typescript::read(source, base, limits).map_err(Into::into)
        },
        printer: Some(Box::new(lash_dialect_typescript::TypeScript::default())),
        captions: lash_dialect_typescript::CAPTIONS,
        function_values: Some(lash_dialect_typescript::function_values()),
    })
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
    let mut dialect = lash_dialect_python::dialect();
    dialect.front_end = Box::new(lash_dialect_python::Python { limits });
    dialect
}

#[cfg(feature = "dialect-typescript")]
mod typescript {
    use std::sync::Mutex;

    use lash_kernel_dialect::{Diagnostic, Environment, Lowered, Mode};

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
        fn tool_calls(
            &self,
            source: &str,
            environment: &Environment<'_>,
            cell: lash_kernel_dialect::CellEnvironment<'_>,
        ) -> Result<std::collections::BTreeSet<String>, Diagnostic> {
            self.parser
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .tool_calls(source, environment, cell)
                .map_err(Into::into)
        }

        fn lower(
            &self,
            source: &str,
            environment: &Environment<'_>,
            mode: Mode<'_>,
        ) -> Result<Lowered, Diagnostic> {
            self.parser
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .lower(source, environment, mode)
                .map_err(Into::into)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lash_kernel_doc::FunctionDefinition;
    use lash_vm_client::WorkerTuning;

    /// lash's own packages, as the parent holds them.
    pub(crate) fn lash() -> Arc<HeldPackages> {
        lash_vm_library::lash_packages().expect("lash's packages")
    }

    /// A host that names TypeScript alone.
    pub(crate) fn typescript_only(tuning: &WorkerTuning) -> Result<Embedding, EmbedError> {
        Embedder::new()
            .dialect(typescript(tuning)?)?
            .finish(&lash())
    }

    /// A host that names every dialect lash ships.
    pub(crate) fn every_dialect(tuning: &WorkerTuning) -> Result<Embedding, EmbedError> {
        Embedder::new()
            .dialect(typescript(tuning)?)?
            .dialect(python())?
            .finish(&lash())
    }

    /// FIG-5990: an embedding has no default dialect. A host that names
    /// none is refused when it finishes, by a typed error, and one that
    /// names a dialect twice is refused at the second.
    #[test]
    fn an_embedding_with_no_dialect_is_refused() {
        assert!(matches!(
            Embedder::new().finish(&lash()),
            Err(EmbedError::NoDialect)
        ));
        assert!(matches!(
            Embedder::new()
                .dialect(python())
                .expect("the Python dialect")
                .dialect(python()),
            Err(EmbedError::DuplicateDialect { dialect }) if dialect == "python"
        ));
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

    /// PKG-IDENTITY: moving lash's libraries into packages changes no
    /// definition bytes. Each package the build made holds exactly the
    /// definitions its crates define (the kernel library, the extensions,
    /// each dialect's helpers against the library it ships with), under the
    /// same identities, so a worker holds exactly the functions it would
    /// have defined itself, and defines none when it starts (FIG-5796).
    #[test]
    fn lash_packages_hold_exactly_what_their_crates_define() {
        use lash_ext_helper_compiler::library::{Dialect as SourceDialect, Library};
        let typescript =
            Library::of(SourceDialect::TypeScript).expect("TypeScript source packages");
        let python = Library::of(SourceDialect::Python).expect("Python source packages");
        let packages = lash_vm_library::built().expect("built packages");
        let declared: BTreeSet<_> = typescript
            .packages
            .iter()
            .chain(&python.packages)
            .map(|package| package.path().to_string())
            .collect();
        let built: BTreeSet<_> = packages
            .iter()
            .map(|package| package.path().to_string())
            .collect();
        assert_eq!(
            packages.len(),
            declared.len(),
            "each source package is shipped once"
        );
        assert_eq!(built, declared, "exactly the source packages are shipped");
        for package in packages {
            let source = if package.path().to_string() == "python" {
                &python
            } else {
                &typescript
            };
            let declared = source
                .packages
                .iter()
                .find(|declared| declared.path() == package.path())
                .expect("source package");
            assert_eq!(
                package.code().len(),
                declared.code().len(),
                "{}: every source definition is shipped",
                package.path()
            );
            for at in 0..package.code().len() {
                let id = package.code().ids()[at];
                let built = package.code().definition(at).expect("built definition");
                let defined = source.registry.get(&id).unwrap_or_else(|| {
                    panic!("{}: {} is defined by its crate", package.path(), built.name)
                });
                assert_eq!(
                    built.as_ref(),
                    defined.definition().as_ref(),
                    "{}: {}",
                    package.path(),
                    built.name
                );
            }
        }
    }

    /// PKG-PURE-BUILTIN: lash's six packages export no operations and no
    /// body lists one.
    #[test]
    fn lash_packages_export_and_perform_no_operation() {
        let packages = lash_vm_library::built().expect("lash's packages");
        assert_eq!(packages.len(), lash_vm_releases::PACKAGES.len());
        for package in &packages {
            assert!(
                package
                    .exports()
                    .values()
                    .all(|export| !matches!(export, lash_kernel_package::Export::Operation(_))),
                "{} exports an operation",
                package.path()
            );
            for at in 0..package.code().len() {
                let definition = package.code().definition(at).expect("a definition decodes");
                let Some(body) = definition.body() else {
                    continue;
                };
                assert!(
                    body.effects.is_empty(),
                    "{} lists an operation",
                    definition.name
                );
            }
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
        let named: BTreeSet<FunctionId> = embedding.library().iter().map(|(_, id)| id).collect();
        assert!(
            named.iter().all(|function| written.get(function).is_some()),
            "every name resolves to a registered function"
        );
        for version in lash_kernel_doc::KernelVersion::ALL {
            let held = registry
                .iter()
                .filter(|(_, function)| function.definition().kernel == version.number())
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
        let parent = lash_vm_library::lash_packages()
            .map(|packages| std::sync::Arc::clone(packages.functions()))
            .expect("the parent's functions");
        let again = lash_vm_library::lash_packages()
            .map(|packages| std::sync::Arc::clone(packages.functions()))
            .expect("the parent's functions");
        assert!(Arc::ptr_eq(&parent, &again), "assembled once in a process");
        let embedding = every_dialect(&WorkerTuning::standard()).expect("an embedding of both");
        let worker = embedding.registry().expect("every version's functions");
        let definitions =
            |registry: &FunctionRegistry| -> BTreeMap<FunctionId, FunctionDefinition> {
                registry
                    .iter()
                    .map(|(function, registered)| {
                        (
                            *function,
                            FunctionDefinition::clone(registered.definition()),
                        )
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
                    },
                    Mode::Cell(lash_kernel_dialect::CellEnvironment {
                        bindings: &bindings,
                        functions: &functions,
                    }),
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
        };
        embedding
            .lower(
                lash_vm_releases::TYPESCRIPT,
                &source,
                &environment,
                Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
            )
            .unwrap()
            .unwrap();
        let mut invalid = tuning;
        invalid.typescript_source_nesting = 29;
        assert!(matches!(
            typescript(&invalid),
            Err(EmbedError::Dialect { .. })
        ));
        let python_source = format!("#{}", "x".repeat(1 << 20));
        tuning.python_source_bytes = python_source.len();
        let embedding = crate::built_embedding(&tuning).unwrap();
        let environment = Environment {
            library: embedding.library(),
            effects: &BTreeMap::new(),
            controls: &BTreeMap::new(),
            tool_roots: &BTreeSet::new(),
        };
        embedding
            .lower(
                lash_vm_releases::PYTHON,
                &python_source,
                &environment,
                Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
            )
            .unwrap()
            .unwrap();
    }
}

#[cfg(test)]
mod release_tests;
