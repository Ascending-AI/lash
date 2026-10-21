//! Readable export: a document printed as the TypeScript a model would
//! write, verified region by region.
//!
//! Every printed module must lower to the complete original document and
//! captions. If authored source cannot express it, printing refuses.

mod closures;
mod expressions;
mod lift;
mod methods;
mod mixed;
mod source;
mod statements;

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{
    Environment, GeneratedSourceLimits, Library, PrintError, PrintOptions, PrintedSource, Printer,
    SourceFormat, check_document_depth, regions,
};
use lash_kernel_doc::{self as k};

use lash_vocabulary::Name;

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
    options: PrintOptions,
) -> Result<PrintedSource, PrintError> {
    check_document_depth(document, options.limits)?;
    let namespaces =
        regions::namespaces(document).map_err(|error| PrintError::Invalid(error.to_string()))?;
    let lifted = lift::document(document, captions, library);
    let printing = Printing {
        document,
        captions,
        options,
        environment: Environment::new(library, &namespaces),
        items: lifted
            .functions
            .into_iter()
            .filter_map(|(name, item)| item.ok().map(|item| (name, item)))
            .collect(),
        main: lifted.main.ok(),
    };
    let verified = printing.items.keys().cloned().collect();
    regions::print_verified(document, captions, options, verified, &printing)
}

/// One document's lifts, printed and read for [`regions::print_verified`].
struct Printing<'a> {
    document: &'a k::Document,
    captions: Option<&'a k::Captions>,
    options: PrintOptions,
    environment: Environment<'a>,
    items: BTreeMap<Name, source::Item>,
    main: Option<lift::Main>,
}

impl regions::Regions for Printing<'_> {
    fn format(&self) -> SourceFormat {
        SourceFormat::TypeScriptV1
    }

    fn whole(&self, functions: &BTreeSet<Name>) -> Option<regions::Whole> {
        let mut writer = source::Writer::default();
        let mut written = BTreeMap::new();
        match self.main.as_ref()? {
            lift::Main::Module(order) => {
                for (index, name) in order.iter().enumerate() {
                    if index > 0 {
                        writer.out.push('\n');
                    }
                    if let Some(item) = self.items.get(name).filter(|_| functions.contains(name)) {
                        let start = writer.out.len();
                        writer.item(item);
                        written.insert(name.clone(), start..writer.out.len());
                    }
                }
            }
            lift::Main::Cell(statements) => writer.top_level(statements),
        }
        Some(regions::Whole {
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
        )
        .map_err(|diagnostic| diagnostic.span.map(|span| span.start))
    }
}

/// Read ordinary TypeScript using the base document's declared signatures.
///
/// # Errors
/// The front end's diagnostic for invalid or unsupported authored source.
pub fn read(
    source: &str,
    base: &k::Document,
    library: &dyn Library,
    limits: GeneratedSourceLimits,
) -> Result<(k::Document, k::Captions), Diagnostic> {
    let namespaces = regions::namespaces(base).map_err(|refused| {
        Diagnostic::new(
            crate::DiagnosticCode::InvalidAst,
            format!("the base document's operations compose no namespace: {refused}"),
            None,
        )
    })?;
    let environment = Environment::new(library, &namespaces);
    let read = mixed::read(source, base, &environment, limits)?;
    Ok((read.document, read.captions))
}

/// The TypeScript printer, as a dialect package holds it.
impl Printer for crate::TypeScript {
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
