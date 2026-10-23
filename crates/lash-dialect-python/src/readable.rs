//! Readable export: a document printed as the Python a model would write,
//! verified region by region ([`lash_kernel_dialect::regions`]).
//!
//! Printing verifies the complete lowering identity and captions. A document
//! with no faithful authored representation is refused.

use std::collections::BTreeSet;

use lash_kernel_dialect::regions::{self, RegionRead};
use lash_kernel_dialect::{
    Environment, GeneratedSourceLimits, Library, Mode, PrintError, PrintOptions, PrintedSource,
    Printer, SourceFormat, check_document_depth,
};
use lash_kernel_doc::{self as k};

use lash_vocabulary::Name;

use crate::printer::{Regions, print_regions};
use crate::{Code, Diagnostic, SourceLimits};

/// Print authored Python after checking its full lowering identity.
///
/// # Errors
/// Refuses documents that authored source cannot faithfully express, or source
/// exceeding the configured limits.
pub fn print(
    document: &k::Document,
    captions: Option<&k::Captions>,
    library: &dyn Library,
    options: PrintOptions,
) -> Result<PrintedSource, PrintError> {
    check_document_depth(document, options.limits)?;
    let namespaces =
        regions::namespaces(document).map_err(|error| PrintError::Invalid(error.to_string()))?;
    let printing = Printing {
        document,
        captions,
        options,
        environment: Environment::new(library, &namespaces),
    };
    let authored = document.exports.keys().cloned().collect();
    regions::print_verified(document, captions, options, authored, &printing)
}

/// Read ordinary Python using the base document's declared import signatures.
///
/// # Errors
/// The front end's diagnostic for invalid or unsupported authored source.
pub fn read(
    source: &str,
    base: &k::Document,
    library: &dyn Library,
    limits: GeneratedSourceLimits,
    source_limits: Option<lash_kernel_dialect::workflow::WorkflowSourceLimits>,
) -> Result<(k::Document, k::Captions), Diagnostic> {
    let namespaces = regions::namespaces(base).map_err(|refused| {
        crate::diagnostics::unplaced(
            Code::Syntax,
            format!("the base document's operations compose no namespace: {refused}"),
        )
    })?;
    let environment = Environment::new(library, &namespaces);
    let parser_limits = source_limits
        .map(|limits| SourceLimits::new(limits.max_bytes, limits.max_nesting))
        .transpose()?;
    let read = read_regions(source, base, &environment, limits, parser_limits)?;
    Ok((read.document, read.captions))
}

/// The Python printer, as a dialect package holds it.
impl Printer for crate::Python {
    fn print(
        &self,
        document: &k::Document,
        captions: Option<&k::Captions>,
        library: &dyn Library,
        options: PrintOptions,
    ) -> Result<PrintedSource, PrintError> {
        print(document, captions, library, options)
    }
}

/// One document, printed and read for [`regions::print_verified`].
struct Printing<'a> {
    document: &'a k::Document,
    captions: Option<&'a k::Captions>,
    options: PrintOptions,
    environment: Environment<'a>,
}

impl regions::Regions for Printing<'_> {
    fn format(&self) -> SourceFormat {
        SourceFormat::PythonV1
    }

    fn whole(&self, functions: &BTreeSet<Name>) -> Result<regions::Whole, PrintError> {
        let unprintable = || PrintError::Unprintable {
            site: lash_vocabulary::sites::Site::new(lash_vocabulary::sites::Unit::Main, Vec::new()),
        };
        let parts = regions::module_parts(self.document).ok_or_else(unprintable)?;
        let regions = Regions {
            authored: functions.clone(),
            whole: parts,
        };
        // The front end maps what it lowers; this print is only read.
        let options = PrintOptions {
            source_map: false,
            ..self.options
        };
        let result = print_regions(
            self.document,
            self.environment.library,
            self.captions,
            options,
            &regions,
            &BTreeSet::new(),
        );
        let (printed, written) = result.map_err(|error| match error {
            PrintError::Invalid(_) => unprintable(),
            error => error,
        })?;
        Ok(regions::Whole {
            source: printed.source,
            functions: written,
        })
    }

    fn read(&self, source: &str) -> Result<RegionRead, Option<usize>> {
        read_regions(
            source,
            self.document,
            &self.environment,
            self.options.limits,
            None,
        )
        .map_err(|diagnostic| diagnostic.span.map(|span| span.start))
    }
}

fn read_regions(
    source: &str,
    base: &k::Document,
    environment: &Environment<'_>,
    limits: GeneratedSourceLimits,
    source_limits: Option<SourceLimits>,
) -> Result<RegionRead, Diagnostic> {
    if source.len() > limits.max_bytes {
        return Err(crate::diagnostics::unplaced(
            Code::SourceTooLarge,
            "generated source exceeds byte budget",
        ));
    }
    let lowered = lower(
        source,
        environment,
        base.exports.is_empty(),
        &[],
        source_limits,
    )?;
    Ok(RegionRead {
        document: lowered.document.document().clone(),
        captions: lowered.captions,
        map: lowered.source_map,
    })
}

fn lower(
    source: &str,
    environment: &Environment<'_>,
    cell: bool,
    beside: &[String],
    source_limits: Option<SourceLimits>,
) -> Result<lash_kernel_dialect::Lowered, Diagnostic> {
    let limits = source_limits.unwrap_or(SourceLimits {
        max_bytes: source.len().max(1),
        ..SourceLimits::standard()
    });
    crate::lower_with_calls(
        source,
        environment,
        if cell {
            Mode::Cell(lash_kernel_dialect::CellEnvironment::first())
        } else {
            Mode::Module
        },
        limits,
        None,
        beside,
    )
}

impl lash_kernel_dialect::CallableSourceCodec for crate::Python {
    fn print(
        &self,
        base: &lash_kernel_dialect::CallableSourceBase,
        library: &dyn Library,
        options: PrintOptions,
    ) -> Result<PrintedSource, PrintError> {
        let unprintable = || PrintError::Unprintable {
            site: lash_vocabulary::sites::Site::new(
                lash_vocabulary::sites::Unit::Library(base.function.callable.function),
                [],
            ),
        };
        let mut printed =
            crate::printer::print_callable(base, library, options).map_err(|_| unprintable())?;
        let (lowered, creation) = callable_lower(
            &printed.source,
            base,
            library,
            options.limits,
            Some(verification_policy()),
        )
        .map_err(|_| unprintable())?;
        let Some(k::Node::Expr(k::Expr::Closure(root))) = lowered.document.node(&creation) else {
            return Err(unprintable());
        };
        let document = base.document.to_document().map_err(|_| unprintable())?;
        let ids = lash_kernel_dialect::callable::equivalent(
            &document,
            base.function.callable.function,
            &lowered.document,
            root.function,
        )
        .ok_or_else(unprintable)?;
        if options.source_map
            && let Some(map) = lowered.source_map.as_ref()
        {
            let sites = map
                .sites
                .iter()
                .filter_map(|row| {
                    let mut row = row.clone();
                    let lash_vocabulary::sites::Unit::Library(id) = &mut row.site.unit else {
                        return None;
                    };
                    *id = *ids.get(id)?;
                    document.node(&row.site).map(|_| row)
                })
                .collect();
            printed.source_map = Some(
                k::SourceMap::new(
                    &document,
                    &printed.source,
                    SourceFormat::PythonV1.language(),
                    sites,
                )
                .map_err(|error| PrintError::Invalid(error.to_string()))?,
            );
        }

        printed.dropped = lash_kernel_dialect::callable::printed_dropped(base, &lowered, &ids);
        Ok(printed)
    }
    fn read(
        &self,
        source: &str,
        base: &lash_kernel_dialect::CallableSourceBase,
        library: &dyn Library,
        limits: GeneratedSourceLimits,
        policy: Option<lash_kernel_dialect::workflow::WorkflowSourceLimits>,
    ) -> Result<lash_kernel_dialect::CallableSourceRead, lash_kernel_dialect::CallableReadError>
    {
        if let Some(policy) = policy
            && source.len() > policy.max_bytes
        {
            return Err(lash_kernel_dialect::Diagnostic::from(PrintError::Bytes {
                limit: policy.max_bytes,
            })
            .into());
        }

        let conversion = policy
            .map(|p| p.implicit_text_conversion)
            .unwrap_or_default();
        for captured in base.function.nodes().into_iter().skip(1) {
            conversion.check(&captured.implicit_text_conversions)?;
        }
        let (lowered, creation) = callable_lower(source, base, library, limits, policy)?;
        let printed = crate::printer::print_callable(
            base,
            library,
            PrintOptions {
                source_map: false,
                limits,
            },
        )
        .map_err(lash_kernel_dialect::Diagnostic::from)?;
        let (baseline, _) = callable_lower(
            &printed.source,
            base,
            library,
            limits,
            Some(verification_policy()),
        )?;
        let read = lash_kernel_dialect::callable::patch(base, lowered, creation, &baseline)?;
        if read.document == base.document {
            conversion.check(&base.function.implicit_text_conversions)?;
        }
        Ok(read)
    }
}
fn callable_lower(
    source: &str,
    base: &lash_kernel_dialect::CallableSourceBase,
    library: &dyn Library,
    limits: GeneratedSourceLimits,
    policy: Option<lash_kernel_dialect::workflow::WorkflowSourceLimits>,
) -> Result<
    (lash_kernel_dialect::Lowered, lash_vocabulary::sites::Site),
    lash_kernel_dialect::CallableReadError,
> {
    use lash_kernel_dialect::{CallableEditRefusal, CellEnvironment};
    use lash_vocabulary::sites::Unit;
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
    let namespaces = regions::namespaces(&document)
        .map_err(|e| lash_kernel_dialect::Diagnostic::from(PrintError::Invalid(e.to_string())))?;
    let parser_limits = policy
        .map(|policy| SourceLimits::new(policy.max_bytes, policy.max_nesting))
        .transpose()?
        .unwrap_or(SourceLimits::standard());
    let parsed = ruff_python_parser::parse_module(source)
        .map_err(|error| crate::diagnostics::unplaced(Code::Syntax, error.to_string()))?;
    if parsed.syntax().body.is_empty() {
        return Err(CallableEditRefusal::EntryMissing.into());
    }
    if parsed
        .syntax()
        .body
        .iter()
        .filter(|stmt| {
            !matches!(
                stmt,
                ruff_python_ast::Stmt::Import(_) | ruff_python_ast::Stmt::ImportFrom(_)
            )
        })
        .count()
        != 1
        || !parsed
            .syntax()
            .body
            .iter()
            .any(|stmt| matches!(stmt, ruff_python_ast::Stmt::FunctionDef(_)))
    {
        return Err(CallableEditRefusal::OutsideEditableRegion {
            site: lash_vocabulary::sites::Site::new(Unit::Main, []),
        }
        .into());
    }
    let aliases = lash_kernel_dialect::callable::aliases(base, crate::printer::legal_identifier)
        .map_err(lash_kernel_dialect::Diagnostic::from)?;
    let mut bindings = aliases.values().cloned().collect::<BTreeSet<_>>();
    let lowered = loop {
        match crate::lower_bound(
            source,
            &Environment::new(library, &namespaces),
            Mode::Cell(CellEnvironment {
                bindings: &bindings,
                functions: &Default::default(),
            }),
            parser_limits,
            None,
            &[],
            Some(&document.imports),
        ) {
            Ok(lowered) => break lowered,
            Err(error) => {
                if error.code == Code::PackageMemberUnknown.as_str() {
                    return Err(CallableEditRefusal::ImportsChanged.into());
                }
                if error.code == Code::UnknownName.as_str()
                    && let Some(span) = error.span
                    && let Some(word) = source
                        .get(span.start..span.end)
                        .filter(|s| crate::printer::identifier(s))
                    && bindings.insert(Name::new(word))
                {
                    continue;
                }
                return Err(error.into());
            }
        }
    };
    let lowered = lash_kernel_dialect::callable::restore(lowered, &aliases)?;
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

fn verification_policy() -> lash_kernel_dialect::workflow::WorkflowSourceLimits {
    lash_kernel_dialect::workflow::WorkflowSourceLimits {
        max_bytes: SourceLimits::standard().max_bytes,
        max_nesting: SourceLimits::standard().max_nesting,
        implicit_text_conversion:
            lash_kernel_dialect::workflow::ImplicitTextConversionPolicy::Allow,
    }
}
