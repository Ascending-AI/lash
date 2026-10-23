//! Selected function tokens lift to bound fragments, never capture declarations.
use super::{lift::Lifter, source};
use lash_kernel_dialect::{
    CallableEditRefusal, CallableReadError, CallableSourceBase, CallableSourceCodec,
    CallableSourceRead, CellEnvironment, Environment, GeneratedSourceLimits, Library, Mode,
    PrintError, PrintOptions, PrintedSource, SourceFormat, callable,
};
use lash_kernel_doc as k;
use lash_vocabulary::{
    Name,
    sites::{Site, Unit},
};
use std::collections::{BTreeMap, BTreeSet};

fn unprintable(base: &CallableSourceBase) -> PrintError {
    PrintError::Unprintable {
        site: Site::new(Unit::Library(base.function.callable.function), []),
    }
}
fn token(base: &CallableSourceBase) -> Result<k::Expr, PrintError> {
    let values = base
        .function
        .token
        .as_ref()
        .ok_or_else(|| unprintable(base))?;
    let mut parts = Vec::new();
    for (index, value) in values.iter().enumerate() {
        parts.push(if index == crate::FUNCTION_CALLABLE as usize {
            k::Expr::Closure(Box::new(k::Closure {
                function: base.function.callable.function,
                presentation: Some(k::CallablePresentation {
                    signature: base.function.signature.clone(),
                    convention: base.function.convention.clone(),
                    representation: base.function.representation.clone(),
                }),
            }))
        } else {
            match value {
                k::Datum::Null => k::Expr::Literal(k::Literal::Null),
                k::Datum::Text(text) => k::Expr::Literal(k::Literal::Text(text.clone())),
                k::Datum::Float(n) => k::Expr::Literal(k::Literal::Float(*n)),
                k::Datum::Record(fields) if fields.is_empty() => k::Expr::Record(Vec::new()),
                _ => return Err(unprintable(base)),
            }
        });
    }
    Ok(k::Expr::Tuple(parts))
}
fn fragment(
    base: &CallableSourceBase,
    library: &dyn Library,
    options: PrintOptions,
) -> Result<PrintedSource, PrintError> {
    let document = base
        .document
        .to_document()
        .map_err(|e| PrintError::Invalid(e.to_string()))?;
    lash_kernel_dialect::check_document_depth(&document, options.limits)?;
    let mut lifter = Lifter::new(
        &document,
        Some(&base.function.captions),
        Unit::Library(base.function.callable.function),
        library,
    );
    let aliases = callable::aliases(base, crate::printer::legal_identifier)?;
    lifter.bindings.extend(
        aliases
            .iter()
            .map(|(name, alias)| (name.clone(), alias.to_string())),
    );
    let function = lifter
        .closure(
            &token(base)?,
            &Site::new(Unit::Library(base.function.callable.function), []),
        )
        .map_err(|_| unprintable(base))?
        .ok_or_else(|| unprintable(base))?;
    let mut writer = source::Writer::default();
    writer.top_level(&[source::Stmt {
        caption: base
            .function
            .captions
            .at(&Site::new(
                Unit::Library(base.function.callable.function),
                [],
            ))
            .cloned(),
        kind: source::StmtKind::Const(
            callable::entry_name(base, crate::printer::legal_identifier)?.to_string(),
            source::Expr::arrow(function),
        ),
    }]);
    if writer.out.len() > options.limits.max_bytes {
        return Err(PrintError::Bytes {
            limit: options.limits.max_bytes,
        });
    }
    Ok(PrintedSource {
        source: writer.out,
        format: SourceFormat::TypeScriptV1,
        source_map: None,
        dropped: base
            .function
            .captions
            .entries()
            .iter()
            .map(|c| c.target.clone())
            .collect(),
    })
}
fn lower(
    source: &str,
    base: &CallableSourceBase,
    library: &dyn Library,
    limits: GeneratedSourceLimits,
    policy: Option<lash_kernel_dialect::workflow::WorkflowSourceLimits>,
    capture_types: bool,
) -> Result<(lash_kernel_dialect::Lowered, Site), CallableReadError> {
    if source.len() > limits.max_bytes {
        return Err(lash_kernel_dialect::Diagnostic::from(PrintError::Bytes {
            limit: limits.max_bytes,
        })
        .into());
    }
    let document = base
        .document
        .to_document()
        .map_err(|e| lash_kernel_dialect::Diagnostic::from(PrintError::Invalid(e.to_string())))?;
    let namespaces = lash_kernel_dialect::regions::namespaces(&document)
        .map_err(|e| lash_kernel_dialect::Diagnostic::from(PrintError::Invalid(e.to_string())))?;
    let environment = Environment::new(library, &namespaces);
    let defaults = crate::SourceLimits::standard();
    let policy = policy.unwrap_or(lash_kernel_dialect::workflow::WorkflowSourceLimits {
        max_bytes: defaults.max_bytes(),
        max_nesting: defaults.max_nesting(),
        implicit_text_conversion: Default::default(),
    });
    for captured in base.function.nodes().into_iter().skip(1) {
        policy
            .implicit_text_conversion
            .check(&captured.implicit_text_conversions)?;
    }
    let parser_limits = crate::SourceLimits::new(policy.max_bytes, policy.max_nesting)
        .map_err(lash_kernel_dialect::Diagnostic::from)?;
    let program = crate::adapter::parse_bound(source, parser_limits)
        .map_err(lash_kernel_dialect::Diagnostic::from)?;
    fn declaration(statement: &crate::adapter::Stmt) -> bool {
        use crate::adapter::{Expr, Stmt};
        match statement {
            Stmt::Spanned(_, statement) => declaration(statement),
            Stmt::Function { .. } => true,
            Stmt::Var { declarations, .. } => matches!(
                declarations.as_slice(),
                [crate::adapter::Var {
                    init: Some(Expr::Function(_)),
                    ..
                }]
            ),
            _ => false,
        }
    }
    if program.statements.is_empty() {
        return Err(CallableEditRefusal::EntryMissing.into());
    }
    if !matches!(program.statements.as_slice(), [statement] if declaration(statement)) {
        return Err(CallableEditRefusal::OutsideEditableRegion {
            site: Site::new(Unit::Main, []),
        }
        .into());
    }
    let aliases = callable::aliases(base, crate::printer::legal_identifier)
        .map_err(lash_kernel_dialect::Diagnostic::from)?;
    let mut bindings = aliases.values().cloned().collect::<BTreeSet<_>>();
    let lowered = loop {
        match crate::lower::lower_bound(
            &program,
            source,
            &environment,
            Mode::Cell(CellEnvironment {
                bindings: &bindings,
                functions: &BTreeMap::new(),
            }),
            &[],
            policy.implicit_text_conversion,
            base.function
                .captures
                .iter()
                .map(|(name, value)| {
                    (
                        aliases.get(name).cloned().unwrap_or_else(|| name.clone()),
                        if capture_types {
                            capture_type(value, &base.function.nodes())
                        } else {
                            crate::types::Ty::Unknown
                        },
                    )
                })
                .collect(),
            Some(&document.imports),
        ) {
            Ok(lowered) => break lowered,
            Err(error) => {
                // Give only a diagnosed free identifier a frozen-scope slot,
                // then classify the complete lowered free set through CodeReads.
                if error.code == crate::DiagnosticCode::PackageMemberUnknown {
                    return Err(CallableEditRefusal::ImportsChanged.into());
                }
                if error.code == crate::DiagnosticCode::UnknownBinding
                    && let Some(span) = error.span
                    && let Some(word) = source
                        .get(span.start..span.end)
                        .filter(|s| source::identifier(s))
                    && bindings.insert(Name::new(word))
                {
                    continue;
                }
                return Err(lash_kernel_dialect::Diagnostic::from(error).into());
            }
        }
    };
    let lowered = callable::restore(lowered, &aliases)?;
    let mut roots = lowered
        .written
        .keys()
        .filter(|site| site.unit == Unit::Main);
    let creation = roots
        .next()
        .cloned()
        .ok_or(CallableEditRefusal::EntryMissing)?;
    if let Some(site) = roots.next() {
        return Err(CallableEditRefusal::OutsideEditableRegion { site: site.clone() }.into());
    }
    Ok((lowered, creation))
}
impl CallableSourceCodec for crate::TypeScript {
    fn print(
        &self,
        base: &CallableSourceBase,
        library: &dyn Library,
        options: PrintOptions,
    ) -> Result<PrintedSource, PrintError> {
        let mut printed = fragment(base, library, options)?;
        let (read, _, _, correspondence) =
            verified_lower(&printed.source, base, library, options.limits)?;
        printed.dropped = callable::printed_dropped(base, &read, &correspondence);
        if options.source_map
            && let Some(map) = read.source_map
        {
            let old = base
                .document
                .to_document()
                .map_err(|e| PrintError::Invalid(e.to_string()))?;
            let sites = map
                .sites
                .into_iter()
                .filter_map(|mut row| {
                    if let Unit::Library(id) = &mut row.site.unit {
                        *id = correspondence.get(id).copied()?;
                        old.node(&row.site).map(|_| row)
                    } else {
                        None
                    }
                })
                .collect();
            printed.source_map = Some(
                k::SourceMap::new(
                    &old,
                    &printed.source,
                    SourceFormat::TypeScriptV1.language(),
                    sites,
                )
                .map_err(|e| PrintError::Invalid(e.to_string()))?,
            );
        }

        Ok(printed)
    }
    fn read(
        &self,
        source: &str,
        base: &CallableSourceBase,
        library: &dyn Library,
        limits: GeneratedSourceLimits,
        policy: Option<lash_kernel_dialect::workflow::WorkflowSourceLimits>,
    ) -> Result<CallableSourceRead, CallableReadError> {
        let baseline_source = fragment(
            base,
            library,
            PrintOptions {
                source_map: false,
                limits,
            },
        )
        .map_err(lash_kernel_dialect::Diagnostic::from)?;
        let (baseline, _, capture_types, _) =
            verified_lower(&baseline_source.source, base, library, limits)
                .map_err(lash_kernel_dialect::Diagnostic::from)?;
        let (mut lowered, creation) = lower(source, base, library, limits, policy, capture_types)?;
        if !capture_types {
            let (evidence, at) = lower(source, base, library, limits, policy, true)?;
            if let Some(written) = evidence.written.get(&at) {
                lowered.written.insert(creation.clone(), written.clone());
            }
        }
        let read = callable::patch(base, lowered, creation, &baseline)?;
        if read.document == base.document {
            (policy
                .map(|p| p.implicit_text_conversion)
                .unwrap_or_default())
            .check(&base.function.implicit_text_conversions)?;
        }
        Ok(read)
    }
}

fn capture_type(value: &k::CaptureValue, nodes: &[&k::FunctionValue]) -> crate::types::Ty {
    use crate::types::Ty;
    fn data(value: &k::Datum) -> Ty {
        match value {
            k::Datum::Null => Ty::Null,
            k::Datum::Absent => Ty::Undefined,
            k::Datum::Float(_) => Ty::Float,
            k::Datum::Int(_) | k::Datum::Number(_) => Ty::Number,
            k::Datum::Text(_) => Ty::Text,
            k::Datum::Bool(_) => Ty::Bool,
            k::Datum::Record(fields) => Ty::Record(
                fields
                    .iter()
                    .map(|(name, value)| (name.clone(), data(value)))
                    .collect(),
            ),
            k::Datum::List(_) => Ty::List(Box::new(Ty::Unknown)),
            k::Datum::Map(_) => Ty::Map,
            k::Datum::Set(_) => Ty::Set,
            _ => Ty::Unknown,
        }
    }
    match value {
        k::CaptureValue::Data(value) => data(value),
        k::CaptureValue::Function(function) => {
            Ty::Function(Box::new(Ty::annotated(&function.signature.result)))
        }
        k::CaptureValue::Reference(index) => {
            nodes.get(*index as usize).map_or(Ty::Unknown, |function| {
                Ty::Function(Box::new(Ty::annotated(&function.signature.result)))
            })
        }
        k::CaptureValue::Record(fields) => Ty::Record(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), capture_type(value, nodes)))
                .collect(),
        ),
        k::CaptureValue::List(_) => Ty::List(Box::new(Ty::Unknown)),
        k::CaptureValue::Map(_) => Ty::Map,
        k::CaptureValue::Set(_) => Ty::Set,
        _ => Ty::Unknown,
    }
}

fn verified_lower(
    source: &str,
    base: &CallableSourceBase,
    library: &dyn Library,
    limits: GeneratedSourceLimits,
) -> Result<
    (
        lash_kernel_dialect::Lowered,
        Site,
        bool,
        BTreeMap<lash_vocabulary::FunctionId, lash_vocabulary::FunctionId>,
    ),
    PrintError,
> {
    let original = base
        .document
        .to_document()
        .map_err(|e| PrintError::Invalid(e.to_string()))?;
    let policy = lash_kernel_dialect::workflow::WorkflowSourceLimits {
        max_bytes: crate::SourceLimits::standard().max_bytes(),
        max_nesting: crate::SourceLimits::standard().max_nesting(),
        implicit_text_conversion:
            lash_kernel_dialect::workflow::ImplicitTextConversionPolicy::Allow,
    };
    for capture_types in [false, true] {
        if let Ok((lowered, creation)) =
            lower(source, base, library, limits, Some(policy), capture_types)
            && let Some(k::Node::Expr(k::Expr::Closure(root))) = lowered.document.node(&creation)
            && let Some(ids) = callable::equivalent(
                &original,
                base.function.callable.function,
                &lowered.document,
                root.function,
            )
        {
            return Ok((lowered, creation, capture_types, ids));
        }
    }
    Err(unprintable(base))
}
