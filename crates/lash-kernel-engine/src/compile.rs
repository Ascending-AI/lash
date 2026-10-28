use super::*;
use lash_kernel_dialect::{
    CellEnvironment, Environment, Mode, NamespaceKind, NamespaceMember, Namespaces,
    OperationSignature,
};
use lash_kernel_value::{
    EffectName, Fill, ImportKind, OperationMode, OperationRequirement, QualifiedOperation,
    TurnControls, package::Visibility,
};

impl Engine {
    pub fn compile(
        &self,
        dialect: Dialect,
        source: &str,
        externals: &[External],
    ) -> Result<Program, Diagnostic> {
        self.compile_mode(dialect, source, externals, Mode::Module)
    }
    pub fn compile_snippet(
        &self,
        dialect: Dialect,
        source: &str,
        externals: &[External],
    ) -> Result<Program, Diagnostic> {
        self.compile_mode(
            dialect,
            source,
            externals,
            Mode::Cell(CellEnvironment::first()),
        )
    }
    fn compile_mode(
        &self,
        dialect: Dialect,
        source: &str,
        externals: &[External],
        mode: Mode<'_>,
    ) -> Result<Program, Diagnostic> {
        let diagnostic = |message: String| {
            Diagnostic::dialect(
                lash_kernel_dialect::DialectCode::from_static("ENGINE_COMPILE"),
                lash_kernel_dialect::DiagnosticKind::Refusal,
                None,
                message,
            )
        };
        if source.len() > self.limits.source_bytes {
            return Err(diagnostic("source byte limit exceeded".into()));
        }
        let mut packages = BTreeMap::new();
        let mut operations = BTreeMap::new();
        for external in externals {
            let name = EffectName::new(&external.name).map_err(|e| diagnostic(e.to_string()))?;
            let operation =
                QualifiedOperation::from_effect(&name).map_err(|e| diagnostic(e.to_string()))?;
            if operations.insert(operation.clone(), external).is_some() {
                return Err(diagnostic("duplicate external declaration".into()));
            }
            packages
                .entry(operation.package)
                .or_insert_with(BTreeMap::new)
                .insert(
                    operation.name,
                    NamespaceMember {
                        visibility: Visibility::Public,
                        kind: NamespaceKind::Operation(OperationSignature {
                            signature: external.signature.clone(),
                            controls: TurnControls::none(),
                            mode: OperationMode::Waited,
                        }),
                    },
                );
        }
        let namespaces = Namespaces::new(packages, BTreeSet::new(), &|_| false)
            .map_err(|e| diagnostic(e.to_string()))?;
        let environment = Environment::new(&self.installed.names, &namespaces);
        let lowered = self.lower(dialect, source, &environment, mode)?;
        let document = Arc::clone(lowered.document.shared());
        let target = if matches!(mode, Mode::Module) {
            lash_kernel_check::startable_export(&document, &Name::new("main"))
                .map_err(|error| diagnostic(error.to_string()))?;
            Target::Entry(lash_kernel_value::Name::new("main"))
        } else {
            Target::Main
        };
        let fill = |import: &lash_kernel_value::Import| -> Result<Fill, Diagnostic> {
            let ImportKind::Operation { signature } = &import.kind else {
                return Err(diagnostic(
                    "simple programs accept only declared external operations".into(),
                ));
            };
            let external = operations
                .iter()
                .find(|(name, _)| {
                    name.package == import.name.package
                        && name.name == import.name.member
                        && import.name.type_member.is_none()
                })
                .map(|(_, external)| *external)
                .ok_or_else(|| diagnostic("undeclared external import".into()))?;
            if &external.signature != signature {
                return Err(diagnostic("external signature differs from import".into()));
            }
            Ok(Fill::Operation(OperationRequirement::new(
                signature.clone(),
                external.contract.clone(),
            )))
        };
        let fills = document
            .imports
            .iter()
            .map(|(slot, import)| Ok((*slot, fill(import)?)))
            .collect::<Result<BTreeMap<_, _>, Diagnostic>>()?;
        let helper_fills = lash_kernel_doc::helper_imports(&document, &self.installed.names)
            .map_err(|error| diagnostic(error.to_string()))?
            .into_iter()
            .map(|(id, import)| {
                Ok((
                    id,
                    lash_kernel_value::HelperFill {
                        fill: fill(&import)?,
                        import,
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>, Diagnostic>>()?;
        let link = lash_kernel_value::Link {
            helper_fills,
            document: lowered.document.id(),
            fills,
        };
        let content = lash_kernel_package::LinkedContent {
            documents: BTreeMap::from([(lowered.document.id(), document)]),
            links: BTreeMap::from([(link.identity(), link.clone())]),
            packages: BTreeMap::new(),
        };
        self.prepare(link.identity(), &content, target)
            .map_err(|e| diagnostic(e.to_string()))
    }
    /// Advanced entry for hosts already holding immutable linked kernel content.
    /// Uses the same admission and driver as source compilation.
    pub fn prepare(
        &self,
        root: lash_kernel_value::LinkId,
        content: &lash_kernel_package::LinkedContent,
        target: Target,
    ) -> Result<Program, Fault> {
        let kernel = lash_kernel_vm::Program::prepare(
            root,
            content.documents.clone(),
            &content.links,
            PageId([0; 32]),
            self.installed.prepared.clone(),
            lash_kernel_doc::Selection::Fresh,
        )
        .map_err(kernel_fault)?;
        Ok(Program {
            kernel,
            target,
            limits: self.limits.clone(),
        })
    }
    /// Advanced lowering against a host-provided namespace and cell environment.
    pub fn lower(
        &self,
        dialect: Dialect,
        source: &str,
        environment: &Environment<'_>,
        mode: Mode<'_>,
    ) -> Result<lash_kernel_dialect::Lowered, Diagnostic> {
        let diagnostic = |message: String| {
            Diagnostic::dialect(
                lash_kernel_dialect::DialectCode::from_static("ENGINE_COMPILE"),
                lash_kernel_dialect::DiagnosticKind::Refusal,
                None,
                message,
            )
        };
        if source.len() > self.limits.source_bytes {
            return Err(diagnostic("source byte limit exceeded".into()));
        }
        // Both arguments are meaningful when any dialect is enabled.
        let _ = (environment, &mode);
        match dialect {
            Dialect::TypeScript => {
                #[cfg(feature = "dialect-typescript")]
                {
                    let limits = lash_dialect_typescript::SourceLimits::new(
                        self.limits.source_bytes,
                        self.limits.source_nesting as usize,
                    )
                    .map_err(|e| diagnostic(e.to_string()))?;
                    let limits = if matches!(mode, Mode::Cell(_)) {
                        limits.with_cell_result()
                    } else {
                        limits
                    };
                    lash_dialect_typescript::lower_with_limits(source, environment, mode, limits)
                        .map_err(Diagnostic::from)
                }
                #[cfg(not(feature = "dialect-typescript"))]
                {
                    Err(diagnostic("TypeScript feature is not enabled".into()))
                }
            }
            Dialect::Python => {
                #[cfg(feature = "dialect-python")]
                {
                    let limits = lash_dialect_python::SourceLimits::new(
                        self.limits.source_bytes,
                        self.limits.source_nesting as usize,
                    )
                    .map_err(|e| diagnostic(e.to_string()))?;
                    let limits = if matches!(mode, Mode::Cell(_)) {
                        limits.with_cell_result()
                    } else {
                        limits
                    };
                    lash_dialect_python::lower_with_limits(source, environment, mode, limits)
                }
                #[cfg(not(feature = "dialect-python"))]
                {
                    Err(diagnostic("Python feature is not enabled".into()))
                }
            }
        }
    }
}
