//! Readable export: a document printed as the TypeScript a model would
//! write, verified region by region.
//!
//! Every printed module must lower to the complete original document and
//! captions. If authored source cannot express it, printing refuses.

mod closures;
mod expressions;
mod labels;
mod lift;
mod methods;
mod mixed;
mod optional;
mod patterns;
mod primitives;
mod source;
mod statements;
mod style;
mod switches;
mod updates;

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{
    Environment, GeneratedSourceLimits, Library, PrintError, PrintOptions, PrintedSource, Printer,
    SourceFormat, check_document_depth, regions,
};
use lash_kernel_doc::{self as k};

use lash_kernel_value::Name;

use crate::Diagnostic;

/// Print authored TypeScript after checking its full lowering identity.
///
/// # Errors
/// Refuses a document that has no faithful authored representation or exceeds
/// the configured source limits.
pub fn print(
    document: &k::Document,
    captions: Option<&k::Captions>,
    library: &dyn Library,
    declarations: &lash_kernel_dialect::Namespaces,
    options: PrintOptions,
) -> Result<PrintedSource, PrintError> {
    let mut printed = print_attempt(
        document,
        captions,
        library,
        declarations,
        options,
        style::STYLES[0],
    );
    // Exact operation spellings come last: only a document whose convenient
    // reads and method calls do not read back is printed with them.
    for style in style::attempts().skip(1) {
        if !matches!(printed, Err(PrintError::Unprintable { .. })) {
            break;
        }
        printed = print_attempt(document, captions, library, declarations, options, style);
    }
    printed
}

fn print_attempt(
    document: &k::Document,
    captions: Option<&k::Captions>,
    library: &dyn Library,
    declarations: &lash_kernel_dialect::Namespaces,
    options: PrintOptions,
    style: style::Style,
) -> Result<PrintedSource, PrintError> {
    check_document_depth(document, options.limits)?;
    crate::constructs::conventions::CONVENTIONS.check_document(document, library)?;
    let namespaces = regions::namespaces(document, declarations).map_err(PrintError::Namespace)?;
    let lifted = lift::document(document, captions, library, &namespaces, style);
    let printing = Printing {
        document,
        options,
        environment: Environment::new(library, &namespaces),
        items: lifted
            .functions
            .into_iter()
            .map(|(name, (function, item))| {
                item.map(|item| (name, item))
                    .map_err(|detail| PrintError::Unprintable {
                        reason: lash_kernel_dialect::UnprintableReason::Callable { detail },
                        site: lash_kernel_value::sites::Site::new(
                            lash_kernel_value::sites::Unit::Library(function),
                            Vec::new(),
                        ),
                    })
            })
            .collect::<Result<_, _>>()?,
        main: Some(lifted.main.map_err(|detail| PrintError::Unprintable {
            reason: lash_kernel_dialect::UnprintableReason::Main { detail },
            site: lash_kernel_value::sites::Site::new(
                lash_kernel_value::sites::Unit::Main,
                Vec::new(),
            ),
        })?),
    };
    let verified = printing.items.keys().cloned().collect();
    regions::print_verified(document, captions, options, verified, &printing, library)
}

/// One document's lifts, printed and read for [`regions::print_verified`].
struct Printing<'a> {
    document: &'a k::Document,
    options: PrintOptions,
    environment: Environment<'a>,
    items: BTreeMap<Name, source::Item>,
    main: Option<lift::Main>,
}

impl regions::Regions for Printing<'_> {
    fn format(&self) -> SourceFormat {
        SourceFormat::TypeScriptV1
    }

    fn whole(&self, functions: &BTreeSet<Name>) -> Result<regions::Whole, PrintError> {
        let mut writer = source::Writer::default();
        let mut written = BTreeMap::new();
        let unprintable = || PrintError::Unprintable {
            reason: lash_kernel_dialect::UnprintableReason::ModuleDeclarations,
            site: lash_kernel_value::sites::Site::new(
                lash_kernel_value::sites::Unit::Main,
                Vec::new(),
            ),
        };
        match self.main.as_ref().ok_or_else(unprintable)? {
            lift::Main::Module(parts) => {
                for (index, part) in parts.iter().enumerate() {
                    if index > 0 {
                        writer.out.push('\n');
                    }
                    match part {
                        lift::ModulePart::Export(name) => {
                            let item = self
                                .items
                                .get(name)
                                .filter(|_| functions.contains(name))
                                .ok_or_else(unprintable)?;
                            let start = writer.out.len();
                            writer.item(item);
                            written.insert(name.clone(), start..writer.out.len());
                        }
                        lift::ModulePart::Alias { local, exported } => {
                            writer.export_alias(local.as_str(), exported.as_str())
                        }
                        lift::ModulePart::Statements(statements) => writer.top_level(statements),
                    }
                }
            }
            lift::Main::Cell(statements) => writer.top_level(statements),
        }
        Ok(regions::Whole {
            source: writer.out,
            functions: written,
        })
    }

    fn read(&self, source: &str) -> Result<regions::RegionRead, Option<usize>> {
        mixed::read(
            source,
            self.document,
            &self.environment,
            self.options.limits,
            None,
        )
        .map_err(|diagnostic| diagnostic.span().map(|span| span.start))
    }
}

/// Read ordinary TypeScript using linked package declarations and the base's operation imports.
///
/// # Errors
/// The front end's diagnostic for invalid or unsupported authored source.
pub fn read(
    source: &str,
    base: &k::Document,
    library: &dyn Library,
    declarations: &lash_kernel_dialect::Namespaces,
    limits: GeneratedSourceLimits,
    source_limits: Option<lash_kernel_dialect::workflow::WorkflowSourceLimits>,
) -> Result<(k::Document, k::Captions), Diagnostic> {
    let namespaces = regions::namespaces(base, declarations).map_err(|refused| {
        Diagnostic::new(
            crate::DiagnosticCode::InvalidAst,
            format!("the base document's operations compose no namespace: {refused}"),
            None,
        )
    })?;
    let environment = Environment::new(library, &namespaces);
    let source_limits = match source_limits {
        Some(limits) => crate::SourceLimits::new(limits.max_bytes, limits.max_nesting)?
            .with_implicit_text_conversion(limits.implicit_text_conversion),
        None => crate::SourceLimits::new(source.len().max(1), crate::MAX_SOURCE_NESTING_DEPTH)?,
    };
    let read = mixed::read(source, base, &environment, limits, Some(source_limits))?;
    Ok((read.document, read.captions))
}

/// The TypeScript printer, as a dialect package holds it.
impl Printer for crate::TypeScript {
    fn print(
        &self,
        document: &k::Document,
        captions: Option<&k::Captions>,
        library: &dyn Library,
        declarations: &lash_kernel_dialect::Namespaces,
        options: PrintOptions,
    ) -> Result<PrintedSource, PrintError> {
        print(document, captions, library, declarations, options)
    }
}

#[cfg(test)]
mod tests;
