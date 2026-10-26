//! A worker binds native implementations and explicitly selected dialects to
//! its parent's immutable native library image. Resolver roots travel in that
//! image; linked documents and Import slots travel with each execution.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::{Diagnostic, Dialect, Environment, Lowered, Mode, NamedLibrary};
use lash_kernel_doc::{FunctionRegistry, NativeFunction};

use lash_kernel_value::names::FunctionId;
use lash_kernel_vm::PreparedLibrary;
use lash_protocol_types::engine_fault::WorkerLibraryRefusal;
use lash_vm_client::packages::WorkerLibrary;

/// Why a worker could not assemble what it runs. The worker refuses to
/// start: it is a defect of the build or of the host's configuration,
/// never of a document.
#[derive(Clone, Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("the function registry refused a definition: {0}")]
    Registry(String),
    /// The packages refused: a `Native`-only function with no native, a
    /// native or plan for no held function, or an artifact the registry refused.
    #[error(transparent)]
    Library(#[from] WorkerLibraryRefusal),
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
}

impl From<lash_kernel_doc::RegistryError> for EmbedError {
    fn from(error: lash_kernel_doc::RegistryError) -> Self {
        use lash_kernel_doc::RegistryError;
        match error {
            RegistryError::FrameNativeMissing { function, name } => {
                Self::Library(WorkerLibraryRefusal::NativeMissing { function, name })
            }
            RegistryError::FrameNativeUnused { function } => {
                Self::Library(WorkerLibraryRefusal::NativeUnused { function })
            }
            RegistryError::FramePlanMissing { function, name } => {
                Self::Library(WorkerLibraryRefusal::PlanMissing { function, name })
            }
            other => Self::Registry(other.to_string()),
        }
    }
}

/// What a worker holds from its parent's pool-start frame.
pub struct Embedding {
    pub(crate) documents: crate::documents::Documents,
    /// The functions as registered, each written for the kernel version
    /// the dialects lower to.
    written: Arc<FunctionRegistry>,
    prepared: OnceLock<PreparedLibrary>,
    /// The names of the node's own set.
    pub(crate) library: NamedLibrary,
    pub(crate) dialects: BTreeMap<String, Dialect>,
    /// The packages the parent registered.
    held: WorkerLibrary,
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
    pub fn library_image(&self) -> &WorkerLibrary {
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
    embedding: &Embedding,
    document: &lash_kernel_doc::Document,
    definitions: &[(FunctionId, &[u8])],
    limits: lash_kernel_value::data_limits::KernelDataLimits,
    selection: &lash_kernel_doc::Selection,
) -> Result<
    Result<
        (PreparedLibrary, lash_vm_broker::kernel::closure::Closure),
        lash_vm_broker::kernel::loader::LoadRefusal,
    >,
    EmbedError,
> {
    use lash_vm_broker::kernel::loader;
    let frame = embedding.prepared_library()?;
    let mut stored = match definitions
        .iter()
        .map(|(function, body)| {
            embedding
                .stored
                .decode(*function, body, limits)
                .map(|definition| (*function, definition))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()
    {
        Ok(stored) => stored,
        Err(refusal) => return Ok(Err(refusal)),
    };
    for (id, definition) in &document.functions {
        if frame.registry().get(id).is_none() {
            stored
                .entry(*id)
                .or_insert_with(|| Arc::new(definition.clone()));
        }
    }
    let assembled = match loader::assemble(
        document,
        frame.registry(),
        &stored,
        embedding.library_image().helpers(),
        selection,
    ) {
        Ok(assembled) => assembled,
        Err(refusal) => return Ok(Err(refusal)),
    };
    if assembled.held {
        return Ok(Ok((frame.clone(), assembled.closure)));
    }
    let library = embedding
        .loaded
        .get_or_try::<std::convert::Infallible>(assembled.closure.reference(), || {
            Ok(PreparedLibrary::with_charge(
                Arc::clone(&assembled.registry),
                lash_kernel_vm::Charge::Exact,
            )
            .with_helper_handle(Arc::clone(embedding.library_image().helpers())))
        })
        .unwrap_or_else(|never| match never {});
    Ok(Ok((library, assembled.closure)))
}

/// The linked worker path admits original documents before forming its union
/// registry. Wire definitions must agree with those immutable code records.
pub(crate) fn linked_library_for(
    embedding: &Embedding,
    root: &lash_kernel_doc::Document,
    documents: &BTreeMap<lash_kernel_value::DocumentId, Arc<lash_kernel_doc::Document>>,
    functions: &BTreeMap<lash_kernel_value::LinkId, std::collections::BTreeSet<FunctionId>>,
    definitions: &[(FunctionId, &[u8])],
    limits: lash_kernel_value::KernelDataLimits,
    selection: &lash_kernel_doc::Selection,
) -> Result<
    Result<
        (PreparedLibrary, lash_vm_broker::kernel::closure::Closure),
        lash_vm_broker::kernel::loader::LoadRefusal,
    >,
    EmbedError,
> {
    use lash_vm_broker::kernel::loader::{self, LoadRefusal};
    let frame = embedding.prepared_library()?;
    for (function, body) in definitions {
        let decoded = match embedding.stored.decode(*function, body, limits) {
            Ok(decoded) => decoded,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let Some(original) = documents
            .values()
            .find_map(|document| document.functions.get(function))
        else {
            return Ok(Err(LoadRefusal::Unlisted {
                function: *function,
            }));
        };
        if *decoded != *original {
            return Ok(Err(LoadRefusal::Identity {
                function: *function,
                found: decoded.identity(),
            }));
        }
    }
    let assembled = match loader::assemble_reach(
        root,
        documents,
        functions,
        &root.requires,
        frame.registry(),
        embedding.library_image().helpers(),
        selection,
    ) {
        Ok(assembled) => assembled,
        Err(refusal) => return Ok(Err(refusal)),
    };
    if assembled.held {
        return Ok(Ok((frame.clone(), assembled.closure)));
    }
    let library = embedding
        .loaded
        .get_or_try::<std::convert::Infallible>(assembled.closure.reference(), || {
            Ok(PreparedLibrary::with_charge(
                Arc::clone(&assembled.registry),
                lash_kernel_vm::Charge::Exact,
            )
            .with_helper_handle(Arc::clone(embedding.library_image().helpers())))
        })
        .unwrap_or_else(|never| match never {});
    Ok(Ok((library, assembled.closure)))
}

/// A worker's configuration while it is assembled: the natives the host
/// supplies and the dialects it installs. It holds no package: it is bound
/// once to its parent's ([`Embedder::finish`]).
#[derive(Default)]
pub struct Embedder {
    natives: BTreeMap<FunctionId, Arc<dyn NativeFunction>>,
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
        if self.natives.insert(function, native).is_some() {
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
    /// [`EmbedError::NoDialect`]; [`WorkerLibraryRefusal::NativeMissing`] for a
    /// `Native`-only function no native is supplied for, and
    /// [`WorkerLibraryRefusal::NativeUnused`] for a native of no held function
    /// that states one; [`WorkerLibraryRefusal::PlanMissing`] for a generated
    /// plan without its exact body; [`EmbedError::Registry`].
    pub fn finish(self, library: &WorkerLibrary) -> Result<Embedding, EmbedError> {
        if self.dialects.is_empty() {
            return Err(EmbedError::NoDialect);
        }
        let [image, index, positions, _] = library.indexes()?;
        let installed = lash_kernel_engine::embedding::install(
            image,
            index,
            positions,
            library.roots(),
            Arc::clone(library.helpers()),
            &self.natives,
        )
        .map_err(|error| match error {
            lash_kernel_engine::embedding::InstallError::Registry(error) => EmbedError::from(error),
            other => EmbedError::Registry(other.to_string()),
        })?;
        Ok(Embedding {
            documents: Default::default(),
            library: installed.names,
            written: installed.registry,
            prepared: OnceLock::from(installed.prepared),
            dialects: self.dialects,
            held: library.clone(),
            stored: lash_vm_broker::kernel::loader::Definitions::default(),
            loaded: lash_vm_broker::kernel::loader::Libraries::new(16),
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
        name: lash_kernel_releases::TYPESCRIPT.to_owned(),
        front_end: Box::new(front_end),
        read: |source, base, library, limits, source_limits| {
            lash_dialect_typescript::read(source, base, library, limits, source_limits)
                .map_err(Into::into)
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
                dialect: lash_kernel_releases::TYPESCRIPT.to_owned(),
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
        ) -> Result<lash_kernel_dialect::ToolCalls, Diagnostic> {
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
    use lash_vm_client::WorkerTuning;
    use std::collections::BTreeSet;

    use lash_kernel_dialect::{
        EffectControl, NamespaceKind, NamespaceMember, Namespaces, OperationSignature,
    };
    use {
        lash_kernel_value::EffectName, lash_kernel_value::OperationMode,
        lash_kernel_value::QualifiedOperation, lash_kernel_value::Signature,
        lash_kernel_value::TurnControls, lash_kernel_value::package::Visibility,
    };
    pub(crate) fn namespaces(
        effects: &BTreeMap<EffectName, Signature>,
        controls: &BTreeMap<EffectName, BTreeSet<EffectControl>>,
    ) -> Result<Namespaces, lash_kernel_dialect::NamespaceRefusal> {
        let mut packages = BTreeMap::new();
        for (name, signature) in effects {
            let operation = QualifiedOperation::from_effect(name).expect("fixture operation");
            let flags = controls.get(name);
            let mut accepted = TurnControls::none();
            if flags.is_some_and(|flags| flags.contains(&EffectControl::Finish)) {
                accepted = accepted.with_finish(lash_kernel_value::JsonSchema::any());
            }
            if flags.is_some_and(|flags| flags.contains(&EffectControl::SwitchAgentFrame)) {
                accepted = accepted.with_switch_agent_frame();
            }
            packages
                .entry(operation.package)
                .or_insert_with(BTreeMap::new)
                .insert(
                    operation.name,
                    NamespaceMember {
                        visibility: Visibility::Public,
                        kind: NamespaceKind::Operation(OperationSignature {
                            signature: signature.clone(),
                            controls: accepted,
                            mode: OperationMode::Waited,
                        }),
                    },
                );
        }
        Namespaces::new(packages, BTreeSet::new(), &|_| false)
    }
    /// FIG-7239: generated plans bind by identity on the facade's worker path.
    #[cfg(all(feature = "dialect-typescript", feature = "dialect-python"))]
    #[test]
    fn every_generated_plan_binds_on_the_production_registry() {
        let embedding = crate::built_embedding(&WorkerTuning::default()).expect("built embedding");
        let registry = embedding.registry().expect("production registry");
        for plan in lash_ext_ts_core::PLANS
            .iter()
            .chain(lash_ext_py_core::PLANS)
        {
            let registered = registry.get(&plan.function).unwrap_or_else(|| {
                panic!(
                    "{}: generated plan {} has no registered match",
                    plan.name, plan.function
                )
            });
            assert!(
                registered.plan().is_some(),
                "{}: plan did not bind",
                plan.name
            );
        }
    }

    /// FIG-7239: an unknown generated identity refuses registry construction.
    #[cfg(feature = "dialect-typescript")]
    #[test]
    fn a_generated_plan_without_a_registered_identity_is_refused() {
        use lash_kernel_doc::plan::HelperPlan;
        let original = lash_ext_ts_core::PLANS[0];
        let function = FunctionId::from_bytes([0; 32]);
        let plan = Box::leak(Box::new(HelperPlan {
            name: original.name,
            function,
            callees: original.callees,
            literals: original.literals,
            fields: original.fields,
            records: original.records,
            registers: original.registers,
            slots: original.slots,
            closures: original.closures,
            sites: original.sites,
            run: original.run,
            template: None,
        }));
        let library = WorkerLibrary::built().expect("production image");
        let [image, _, positions, _] = library.indexes().expect("production positions");
        let mut registry = FunctionRegistry::over_frame(image).expect("production frame");
        let error = registry
            .bind_frame_plans(positions, &[plan])
            .expect_err("unknown plan");
        assert_eq!(
            error,
            lash_kernel_doc::RegistryError::FramePlanMissing {
                function,
                name: lash_kernel_value::FunctionName::written(original.name),
            }
        );
        assert!(matches!(EmbedError::from(error), EmbedError::Library(
            WorkerLibraryRefusal::PlanMissing { function: id, name }
        ) if id == function && name.as_str() == original.name));
    }

    /// lash's own packages, as the parent holds them.
    pub(crate) fn lash() -> WorkerLibrary {
        let release = lash_kernel_releases::helper_release().expect("helper release");
        WorkerLibrary::with_helpers(
            &release.functions,
            release.resolver_roots(),
            &Default::default(),
            std::sync::Arc::clone(&release.helpers),
        )
        .expect("native image")
    }

    /// Fulfil a law's authored document against the same immutable helper
    /// release a worker holds, with each offered operation pinned by slot.
    pub(crate) fn linked(
        document: lash_kernel_doc::Document,
        library: PreparedLibrary,
    ) -> lash_kernel_vm::Program {
        use {
            lash_kernel_value::Fill, lash_kernel_value::ImportKind,
            lash_kernel_value::OperationRequirement,
        };
        let fills = document
            .imports
            .iter()
            .map(|(slot, import)| {
                let ImportKind::Operation { signature } = &import.kind else {
                    panic!("fixture code imports require an ordinary package Link");
                };
                (
                    *slot,
                    Fill::Operation(OperationRequirement::new(
                        signature.clone(),
                        lash_kernel_value::ContractId::new("1"),
                    )),
                )
            })
            .collect();
        let root = lash_kernel_value::Link {
            document: document.identity(),
            fills,
        };
        let link = root.identity();
        let mut links = std::collections::BTreeMap::new();
        links.insert(link, root);
        let mut documents = std::collections::BTreeMap::new();
        documents.insert(document.identity(), Arc::new(document));
        let stored = documents
            .values()
            .flat_map(|document| document.functions.iter())
            .map(|(id, definition)| (*id, Arc::new(definition.clone())))
            .collect();
        let mut program = lash_kernel_vm::Program::prepare(
            link,
            documents,
            &links,
            lash_kernel_doc::heap::PageId([0; 32]),
            library,
            lash_kernel_doc::Selection::Fresh,
        )
        .expect("linked law document");
        let closure = lash_vm_broker::kernel::closure::derive(
            program
                .functions
                .iter()
                .chain(&program.document.requires.natives)
                .copied(),
            program.library.registry(),
            &stored,
            program.helpers(),
            &program.document.library,
        )
        .expect("linked definitions");
        program.closure = lash_kernel_doc::heap::PageId(closure.reference().0);
        program
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

    /// The host flow cells under `examples/typescript-host-flows/` lower as a
    /// host lowers a cell, against a catalogue that offers their tools, so a
    /// retired spelling fails here instead of rotting in the example.
    #[test]
    fn the_typescript_host_flow_examples_lower_against_their_host_catalogue() {
        use lash_kernel_value::names::EffectName;
        use lash_kernel_value::names::Name;
        use lash_kernel_value::types::Signature;
        use std::collections::BTreeSet;

        let embedding = typescript_only(&WorkerTuning::standard()).expect("a TypeScript embedding");
        let tool = Signature {
            params: vec![lash_kernel_value::types::Param {
                name: Name::new("input"),
                ty: lash_kernel_value::types::Type::Any,
                optional: false,
            }],
            result: lash_kernel_value::types::Type::Any,
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
                    lash_kernel_releases::TYPESCRIPT,
                    source,
                    &lash_kernel_dialect::Environment::new(
                        embedding.library(),
                        &namespaces(&effects, &controls)
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
        let environment_namespaces = namespaces(&Default::default(), &Default::default())
            .expect("the test's effects are operations");
        let environment = Environment::new(embedding.library(), &environment_namespaces);
        embedding
            .lower(
                lash_kernel_releases::TYPESCRIPT,
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
        let environment_namespaces = namespaces(&Default::default(), &Default::default())
            .expect("the test's effects are operations");
        let environment = Environment::new(embedding.library(), &environment_namespaces);
        embedding
            .lower(
                lash_kernel_releases::PYTHON,
                &python_source,
                &environment,
                Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
            )
            .unwrap()
            .unwrap();
    }
}
