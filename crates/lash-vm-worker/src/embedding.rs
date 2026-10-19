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
use lash_kernel_package::{HeldPackages, PackageInstall, PackageRefusal};
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

/// What a worker holds from its parent's pool-start frame.
pub struct Embedding {
    /// The functions as registered, each written for the kernel version
    /// the dialects lower to.
    written: Arc<FunctionRegistry>,
    prepared: OnceLock<PreparedLibrary>,
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
        let own: std::collections::BTreeSet<_> = packages
            .view(&packages.own())
            .map_err(|error| EmbedError::Registry(error.to_string()))?
            .packages()
            .flat_map(|package| package.code().ids().iter().copied())
            .collect();
        let library = NamedLibrary::resolving(&registry, |function| own.contains(function))
            .map_err(|error| EmbedError::Registry(error.to_string()))?;
        let registry = Arc::new(registry);
        let prepared =
            PreparedLibrary::over_frame(registry.clone(), lash_kernel_vm::Charge::Exact, index)
                .map_err(|error| EmbedError::Registry(error.to_string()))?;
        let releases = lash_vm_library::standard_helper_releases()
            .map_err(|error| EmbedError::Registry(error.to_string()))?;
        Ok(Embedding {
            library,
            written: registry,
            prepared: OnceLock::from(prepared),
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
        let packages = lash_vm_library::lash_packages().expect("the parent's holdings");
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
