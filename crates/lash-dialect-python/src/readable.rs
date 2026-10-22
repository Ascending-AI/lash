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
) -> Result<(k::Document, k::Captions), Diagnostic> {
    let namespaces = regions::namespaces(base).map_err(|refused| {
        crate::diagnostics::unplaced(
            Code::Syntax,
            format!("the base document's operations compose no namespace: {refused}"),
        )
    })?;
    let environment = Environment::new(library, &namespaces);
    let read = read_regions(source, base, &environment, limits)?;
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
        )
        .map_err(|diagnostic| diagnostic.span.map(|span| span.start))
    }
}

fn read_regions(
    source: &str,
    base: &k::Document,
    environment: &Environment<'_>,
    limits: GeneratedSourceLimits,
) -> Result<RegionRead, Diagnostic> {
    if source.len() > limits.max_bytes {
        return Err(crate::diagnostics::unplaced(
            Code::SourceTooLarge,
            "generated source exceeds byte budget",
        ));
    }
    let lowered = lower(source, environment, base.exports.is_empty(), &[])?;
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
) -> Result<lash_kernel_dialect::Lowered, Diagnostic> {
    let limits = SourceLimits {
        max_bytes: source.len().max(1),
        ..SourceLimits::standard()
    };
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
