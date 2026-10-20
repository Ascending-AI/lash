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
//! opens its parent's merged image and binds its build's natives.
//!
//! Names resolve against one package set's functions: the set the parent's
//! fleet gate chose for the cell (FIG-6918), the node's own or a retained
//! one it wholly holds. Every held function stays reachable by identity, so
//! a document written under another set reads as written.

use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::{Diagnostic, Dialect, Environment, Lowered, Mode, NamedLibrary};
use lash_kernel_doc::{FunctionId, FunctionRegistry, NativeFunction, PackageSetDigest};
use lash_kernel_package::{HeldPackages, PackageInstall, PackageRefusal};
use lash_kernel_vm::PreparedLibrary;

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
    /// A writer asked for the names of a package set the embedding does
    /// not hold.
    #[error("package set {packages} is not held by this embedding")]
    PackagesNotHeld { packages: PackageSetDigest },
}

fn cached<T>(
    slot: &OnceLock<Result<T, EmbedError>>,
    init: impl FnOnce() -> Result<T, EmbedError>,
) -> Result<&T, EmbedError> {
    slot.get_or_init(init).as_ref().map_err(Clone::clone)
}

/// What a worker holds from its parent's pool-start frame.
pub struct Embedding {
    /// The functions as registered, each written for the kernel version
    /// the dialects lower to.
    written: Arc<FunctionRegistry>,
    prepared: OnceLock<PreparedLibrary>,
    /// The names of the node's own set.
    pub(crate) library: NamedLibrary,
    pub(crate) dialects: BTreeMap<String, Dialect>,
    /// The packages the parent holds, as it registered them.
    held: HeldPackages,
    /// Each retained set's names, made when a writer first asks for them.
    retained: BTreeMap<PackageSetDigest, OnceLock<Result<NamedLibrary, EmbedError>>>,
    /// The stored definitions runs supplied, decoded and checked once.
    stored: lash_vm_broker::kernel::loader::Definitions,
    /// The libraries assembled for runs that reach a function the frame
    /// lacks, by closure.
    loaded: lash_vm_broker::kernel::loader::Libraries<PreparedLibrary>,
}

impl Embedding {
    /// The node's own set's functions by name, over every registered
    /// function by identity: what a document's manifest resolves against.
    pub fn library(&self) -> &NamedLibrary {
        &self.library
    }

    /// The packages the embedding was bound to.
    pub fn packages(&self) -> &HeldPackages {
        &self.held
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

    /// The names a writer of the cell's package set `packages` resolves
    /// against: that set's functions, over every function the embedding
    /// holds by identity.
    ///
    /// # Errors
    ///
    /// [`EmbedError::PackagesNotHeld`], and [`EmbedError::Registry`] when
    /// the set's names cannot be assembled: a defect of the build.
    pub fn library_of(&self, packages: &PackageSetDigest) -> Result<&NamedLibrary, EmbedError> {
        if *packages == self.held.own() {
            return Ok(&self.library);
        }
        let library = self
            .retained
            .get(packages)
            .ok_or(EmbedError::PackagesNotHeld {
                packages: *packages,
            })?;
        cached(library, || set_library(&self.written, &self.held, packages))
    }

    /// The fixed registry a machine runs documents against, interpreted by
    /// the parent and held unchanged from the pool-start frame.
    ///
    /// # Errors
    ///
    /// None after the frame was installed successfully.
    pub fn registry(&self) -> Result<&Arc<FunctionRegistry>, EmbedError> {
        Ok(&self.written)
    }

    /// The library reservation the parent computed for this same image.
    pub fn prepared_library(&self) -> Result<&PreparedLibrary, EmbedError> {
        self.prepared
            .get()
            .ok_or_else(|| EmbedError::Registry("unprepared frame".into()))
    }

    /// The library `document` runs on, and its execution closure, which
    /// the run's roots pin (FIG-6985): the frame's own when it holds every
    /// function the document reaches; otherwise one assembled from the
    /// frame's functions and `definitions`, the stored definitions of those
    /// it lacks, each checked, with the frame's natives and plans bound by
    /// identity. A run whose closure is incomplete, whose stored definition
    /// is refused, or that needs a native this worker lacks is refused
    /// alone, naming the function.
    ///
    /// # Errors
    ///
    /// [`EmbedError`] for an unprepared frame; the loader's refusal.
    pub fn library_for(
        &self,
        document: &lash_kernel_doc::Document,
        definitions: &[(FunctionId, &[u8])],
        limits: lash_kernel_doc::KernelDataLimits,
    ) -> Result<
        Result<
            (PreparedLibrary, lash_vm_broker::kernel::closure::Closure),
            lash_vm_broker::kernel::loader::LoadRefusal,
        >,
        EmbedError,
    > {
        use lash_vm_broker::kernel::loader;
        let frame = self.prepared_library()?;
        let stored = match definitions
            .iter()
            .map(|(function, body)| {
                self.stored
                    .decode(*function, body, limits)
                    .map(|definition| (*function, definition))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()
        {
            Ok(stored) => stored,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let assembled = match loader::assemble(document, frame.registry(), &stored) {
            Ok(assembled) => assembled,
            Err(refusal) => return Ok(Err(refusal)),
        };
        if assembled.held {
            return Ok(Ok((frame.clone(), assembled.closure)));
        }
        let library = self
            .loaded
            .get_or_try::<std::convert::Infallible>(assembled.closure.reference(), || {
                Ok(PreparedLibrary::with_charge(
                    Arc::clone(&assembled.registry),
                    lash_kernel_vm::Charge::Exact,
                ))
            })
            .unwrap_or_else(|never| match never {});
        Ok(Ok((library, assembled.closure)))
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

    /// The names and surface a writer of `packages` resolves against: a
    /// held set's, or a stored surface's, sent whole with the definitions
    /// it names that the embedding does not hold (FIG-6991).
    ///
    /// # Errors
    ///
    /// [`EmbedError::PackagesNotHeld`] for a held set the embedding does not
    /// hold, or a stored surface naming a definition neither held nor sent;
    /// [`EmbedError::Registry`] for a surface or definition that does not
    /// read as what it claims to be.
    pub(crate) fn surface(
        &self,
        packages: &lash_vm_client::service::SurfaceRef,
    ) -> Result<Surface<'_>, EmbedError> {
        let Some(stored) = &packages.stored else {
            return Ok(Surface::Held {
                library: self.library_of(&packages.digest)?,
                view: self.held.view(&packages.digest).map_err(|refusal| {
                    EmbedError::PackagesNotHeld {
                        packages: refusal.packages,
                    }
                })?,
            });
        };
        let refused = |message: String| EmbedError::Registry(message);
        let surface =
            lash_kernel_package::SurfaceSnapshot::decode(packages.digest, &stored.surface)
                .map_err(|refusal| refused(refusal.to_string()))?;
        let mut library = lash_kernel_package::PackageView::new(&surface, &self.written)
            .library(&self.written)
            .map_err(|error| refused(error.to_string()))?;
        let mut missing = surface.functions();
        missing.retain(|function| self.written.get(function).is_none());
        for image in &stored.definitions {
            let definition: lash_kernel_doc::FunctionDefinition =
                lash_kernel_doc::graph_tokens::decode_body(image)
                    .map_err(|error| refused(error.to_string()))?;
            if !missing.remove(&definition.identity()) {
                return Err(refused(format!(
                    "a stored definition {} the surface does not need was sent",
                    definition.identity()
                )));
            }
            library
                .insert(definition)
                .map_err(|error| refused(error.to_string()))?;
        }
        if !missing.is_empty() {
            return Err(EmbedError::PackagesNotHeld {
                packages: packages.digest,
            });
        }
        Ok(Surface::Stored(Box::new(StoredNames { surface, library })))
    }

    /// The namespaces `operations` compose with the package surface
    /// `surface`, under the roots its packages and this embedding's
    /// installed dialects reserve: what a cell, a workflow import and
    /// discovery all resolve through (P3).
    ///
    /// # Errors
    ///
    /// [`Namespaces::Refused`] naming both sides of a refused composition.
    pub(crate) fn namespaces(
        &self,
        operations: lash_kernel_dialect::Operations,
        surface: &Surface<'_>,
    ) -> Result<lash_kernel_dialect::Namespaces, Namespaces> {
        let view = surface.view(&self.written);
        let reserves = |root: &str| {
            self.dialects
                .values()
                .find_map(|package| package.front_end.reserves(root))
        };
        lash_kernel_dialect::Namespaces::build(
            &lash_kernel_dialect::Reserving {
                view: &view,
                reserves: &reserves,
            },
            operations,
        )
        .map_err(Namespaces::Refused)
    }
}

/// Why an embedding composed no namespaces for a cell.
#[derive(Debug)]
pub(crate) enum Namespaces {
    Refused(lash_kernel_dialect::NamespaceRefusal),
}

/// The names and surface one writer resolves against.
pub(crate) enum Surface<'a> {
    /// A set the embedding holds.
    Held {
        library: &'a NamedLibrary,
        view: lash_kernel_package::PackageView<'a>,
    },
    /// A stored surface the embedding does not hold.
    Stored(Box<StoredNames>),
}

/// A stored surface and its names: the held functions it names by
/// identity, and those sent with it.
pub(crate) struct StoredNames {
    surface: lash_kernel_package::SurfaceSnapshot,
    library: NamedLibrary,
}

impl Surface<'_> {
    pub(crate) fn library(&self) -> &NamedLibrary {
        match self {
            Self::Held { library, .. } => library,
            Self::Stored(stored) => &stored.library,
        }
    }

    pub(crate) fn view<'b>(
        &'b self,
        catalog: &'b Arc<FunctionRegistry>,
    ) -> lash_kernel_package::PackageView<'b> {
        match self {
            Self::Held { view, .. } => *view,
            Self::Stored(stored) => lash_kernel_package::PackageView::new(&stored.surface, catalog),
        }
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
    /// registration: views its frozen code and binds this build's natives
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
        let (image, index) = packages.frame_indexes()?;
        let mut registry = FunctionRegistry::over_frame(image)
            .map_err(|error| EmbedError::Registry(error.to_string()))?;
        registry
            .bind_native_tables(
                packages.frame_positions()?,
                crate::native_tables::native_tables(),
                &self.natives.natives,
            )
            .map_err(|error| match error {
                lash_kernel_doc::RegistryError::FrameNativeMissing { function, name } => {
                    EmbedError::Package(PackageRefusal::NativeMissing { function, name })
                }
                lash_kernel_doc::RegistryError::FrameNativeUnused { function } => {
                    EmbedError::Package(PackageRefusal::NativeUnused { function })
                }
                other => EmbedError::Registry(other.to_string()),
            })?;
        #[cfg(feature = "dialect-typescript")]
        registry
            .bind_frame_plans(packages.frame_positions()?, lash_ext_ts_core::PLANS)
            .map_err(|error| EmbedError::Registry(error.to_string()))?;
        #[cfg(feature = "dialect-python")]
        registry
            .bind_frame_plans(packages.frame_positions()?, lash_ext_py_core::PLANS)
            .map_err(|error| EmbedError::Registry(error.to_string()))?;
        let library = set_library(&registry, packages, &packages.own())?;
        let registry = Arc::new(registry);
        let prepared =
            PreparedLibrary::over_frame(registry.clone(), lash_kernel_vm::Charge::Exact, index)
                .map_err(|error| EmbedError::Registry(error.to_string()))?;
        Ok(Embedding {
            library,
            written: registry,
            prepared: OnceLock::from(prepared),
            dialects: self.dialects,
            retained: packages
                .sets()
                .keys()
                .filter(|set| **set != packages.own())
                .map(|set| (*set, OnceLock::new()))
                .collect(),
            held: packages.clone(),
            stored: lash_vm_broker::kernel::loader::Definitions::default(),
            loaded: lash_vm_broker::kernel::loader::Libraries::new(16),
        })
    }
}

/// The functions of the held set `packages` by name, over every function
/// of `registry` by identity.
fn set_library(
    registry: &FunctionRegistry,
    held: &HeldPackages,
    packages: &PackageSetDigest,
) -> Result<NamedLibrary, EmbedError> {
    let view = held
        .view(packages)
        .map_err(|refusal| EmbedError::PackagesNotHeld {
            packages: refusal.packages,
        })?;
    view.library(registry)
        .map_err(|error| EmbedError::Registry(error.to_string()))
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
        read: |source, base, library, limits| {
            lash_dialect_typescript::read(source, base, library, limits).map_err(Into::into)
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
        lash_vm_releases::lash_packages().expect("lash's packages")
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
        let packages = lash_vm_releases::built().expect("built packages");
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
        let packages = lash_vm_releases::built().expect("lash's packages");
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

    /// A parent holds exactly the functions a worker of every dialect the
    /// build holds does, without linking a dialect or an extension
    /// (FIG-5812): the registry it reads from the packages it holds is that
    /// embedding's, every function under the same identity and definition.
    /// The parent assembles it once in a process:
    /// every session it opens links against the same one (FIG-5759).
    #[test]
    fn a_parent_holds_exactly_the_functions_a_worker_holds() {
        let packages = lash_vm_releases::lash_packages().expect("the parent's holdings");
        let child = HeldPackages::from_frame(packages.frame()).expect("the child's frame");
        for (key, package) in packages.artifacts() {
            assert_eq!(
                package.display(),
                child
                    .artifacts()
                    .get(key)
                    .expect("the same artifact")
                    .display(),
                "POOL-FRAME carries display facts intact, including comparisons and reply receivers"
            );
        }
        let parent = lash_vm_releases::lash_packages()
            .map(|packages| std::sync::Arc::clone(packages.functions()))
            .expect("the parent's functions");
        let again = lash_vm_releases::lash_packages()
            .map(|packages| std::sync::Arc::clone(packages.functions()))
            .expect("the parent's functions");
        assert!(Arc::ptr_eq(&parent, &again), "assembled once in a process");
        let embedding = every_dialect(&WorkerTuning::standard()).expect("an embedding of both");
        let worker = embedding.registry().expect("every version's functions");
        let definitions =
            |registry: &FunctionRegistry| -> BTreeMap<FunctionId, FunctionDefinition> {
                registry
                    .iter()
                    .map(|(function, _)| {
                        let registered = registry
                            .get(function)
                            .expect("each held identity is indexed");
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
                    &lash_kernel_dialect::Environment::new(
                        embedding.library(),
                        &lash_kernel_dialect::Namespaces::of_effects(
                            &effects,
                            &lash_kernel_doc::ContractId::new("1"),
                            &controls,
                        )
                        .expect("the test's effects are operations"),
                    ),
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
        let environment_namespaces = lash_kernel_dialect::Namespaces::of_effects(
            &Default::default(),
            &lash_kernel_doc::ContractId::new("1"),
            &Default::default(),
        )
        .expect("the test's effects are operations");
        let environment = Environment::new(embedding.library(), &environment_namespaces);
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
        let environment_namespaces = lash_kernel_dialect::Namespaces::of_effects(
            &Default::default(),
            &lash_kernel_doc::ContractId::new("1"),
            &Default::default(),
        )
        .expect("the test's effects are operations");
        let environment = Environment::new(embedding.library(), &environment_namespaces);
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
