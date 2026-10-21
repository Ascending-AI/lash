//! Readable export: a document printed as the TypeScript a model would
//! write, verified region by region.
//!
//! Each declared function, and `main`, is a region. A region is lifted
//! back to authored source (`lift`), and the lift is kept only when reading
//! the printed module back gives that region again. A document whose every
//! region lifts prints as a plain workflow module: lowering it gives the
//! document, with its identity, and needs no base. A region that does not
//! lift prints in the reserved kernel surface, opened by the kernel marker
//! ([`crate::printer::KERNEL_MARKER`]); such a module keeps the kernel
//! layout, its `main` in kernel form, and reads back against its base
//! (`mixed`). Printing never gives source that reads as another document:
//! when no mixture verifies, every region prints in kernel form.

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
use lash_kernel_doc as k;
use lash_vocabulary::legacy_names::Name;

use crate::Diagnostic;
use crate::printer::{Regions, print_regions};

/// Prints `document` as authored TypeScript wherever a region lifts and
/// reads back as itself, and in the reserved kernel surface elsewhere.
/// `library` is the one the document's dialect helpers resolve in: lifted
/// source is checked by lowering it against `library` and the operations
/// the document performs. Captions are spelled as JSDoc blocks before what
/// they name; [`PrintedSource::dropped`] lists only an entry's parameter
/// and field captions a kernel-form region cannot spell.
///
/// # Errors
///
/// As [`crate::print_kernel`].
pub fn print(
    document: &k::Document,
    captions: Option<&k::Captions>,
    library: &dyn Library,
    options: PrintOptions,
) -> Result<PrintedSource, PrintError> {
    check_document_depth(document, options.limits)?;
    let Ok(namespaces) = regions::namespaces(document) else {
        let kernel = Regions {
            lifted: BTreeMap::new(),
            marked: true,
        };
        return print_regions(document, captions, options, &kernel).map(|(printed, _)| printed);
    };
    let lifted = lift::document(document, captions);
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

    fn kernel(
        &self,
        lifted: &BTreeSet<Name>,
    ) -> Result<(PrintedSource, BTreeMap<Name, std::ops::Range<usize>>), PrintError> {
        let lifted = self
            .items
            .iter()
            .filter(|(name, _)| lifted.contains(*name))
            .map(|(name, item)| {
                let mut writer = source::Writer::default();
                writer.item(item);
                (name.clone(), writer.out.trim_end().to_owned())
            })
            .collect();
        print_regions(
            self.document,
            self.captions,
            self.options,
            &Regions {
                lifted,
                marked: true,
            },
        )
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

/// Reads TypeScript printed by [`print`] back against `base`, the document
/// it was printed from, edited or not. Source with no kernel-form region is
/// a workflow module (a cell when `base` has no entries) and lowers against
/// `library` and the operations `base` performs: what it declares is what
/// the document holds. Each kernel-form region reads as
/// [`crate::read_kernel`] does, its environment the base's.
///
/// # Errors
///
/// The front end's diagnostic for lifted source, or the kernel reader's
/// for a kernel-form region.
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
