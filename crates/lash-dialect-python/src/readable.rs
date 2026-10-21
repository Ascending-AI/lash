//! Readable export: a document printed as the Python a model would write,
//! verified region by region ([`lash_kernel_dialect::regions`]).
//!
//! Each declared function, and `main`, is a region. A function is written
//! as authored source, and kept so only when reading the printed module
//! back gives that function again. A module whose every region reads back
//! is a plain workflow module, lowering to the document with its identity;
//! any other prints in the kernel layout, each region that did not verify
//! and `main` in kernel form, opened by the kernel marker
//! ([`crate::printer::KERNEL_MARKER`]), and reads back against its base.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use lash_kernel_dialect::regions::{self, RegionRead};
use lash_kernel_dialect::{
    Environment, GeneratedSourceLimits, Library, Mode, PrintError, PrintOptions, PrintedSource,
    Printer, SourceFormat, check_document_depth,
};
use lash_kernel_doc as k;
use lash_vocabulary::legacy_names::Name;

use crate::printer::{Regions, print_regions};
use crate::{Code, Diagnostic, SourceLimits};

/// Prints `document` as authored Python wherever a region reads back as
/// itself, and in the kernel surface elsewhere. `library` is the one the
/// document's dialect helpers resolve in: authored source is checked by
/// lowering it against `library` and the operations the document performs.
/// Captions are spelled as `#:` lines above what they name.
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
        return print_kernel_layout(document, captions, options, BTreeSet::new());
    };
    let printing = Printing {
        document,
        captions,
        options,
        environment: Environment::new(library, &namespaces),
    };
    let authored = document.functions.keys().cloned().collect();
    regions::print_verified(document, captions, options, authored, &printing)
}

/// Reads Python printed by [`print`] back against `base`, the document it
/// was printed from, edited or not. Source with no kernel-form region is a
/// workflow module and lowers against `library` and the operations `base`
/// performs: what it declares is what the document holds. Each kernel-form
/// region reads as [`crate::read_kernel`] does, its environment the base's.
///
/// # Errors
///
/// The front end's diagnostic for authored source, or the exact reader's
/// for a kernel-form region.
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

fn print_kernel_layout(
    document: &k::Document,
    captions: Option<&k::Captions>,
    options: PrintOptions,
    authored: BTreeSet<Name>,
) -> Result<PrintedSource, PrintError> {
    let regions = Regions {
        authored,
        marked: true,
        whole: None,
    };
    print_regions(document, captions, options, &regions).map(|(printed, _)| printed)
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

    fn whole(&self, functions: &BTreeSet<Name>) -> Option<regions::Whole> {
        let order = entries(self.document, self.captions)?;
        let regions = Regions {
            authored: functions.clone(),
            marked: false,
            whole: Some(order),
        };
        // The front end maps what it lowers; this print is only read.
        let options = PrintOptions {
            source_map: false,
            ..self.options
        };
        let (printed, written) =
            print_regions(self.document, self.captions, options, &regions).ok()?;
        Some(regions::Whole {
            source: printed.source,
            functions: written,
        })
    }

    fn kernel(
        &self,
        lifted: &BTreeSet<Name>,
    ) -> Result<(PrintedSource, BTreeMap<Name, Range<usize>>), PrintError> {
        let regions = Regions {
            authored: lifted.clone(),
            marked: true,
            whole: None,
        };
        print_regions(self.document, self.captions, self.options, &regions)
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

/// The entries a module's `main` binds, in order, when that is all it does.
fn entries(document: &k::Document, captions: Option<&k::Captions>) -> Option<Vec<Name>> {
    let mut order = Vec::new();
    for statement in &document.main {
        let k::Stmt::Let {
            name,
            value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Function(function))),
        } = statement
        else {
            return None;
        };
        if name != function || !document.entries.contains_key(name) || order.contains(name) {
            return None;
        }
        order.push(name.clone());
    }
    let site = k::Site::new(k::Unit::Main, Vec::new());
    let captioned = (0..document.main.len()).any(|index| {
        captions.is_some_and(|captions| captions.at(&site.child(index as u32)).is_some())
    });
    (!order.is_empty()
        && order.len() == document.functions.len()
        && order.len() == document.entries.len()
        && !captioned)
        .then_some(order)
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
    let marked = crate::generated::read_marked(source, base, limits)?;
    if marked.spans.is_empty() {
        let lowered = lower(source, environment, &[])?;
        return Ok(RegionRead {
            document: lowered.document.document().clone(),
            captions: lowered.captions,
            map: lowered.source_map,
        });
    }
    if !marked.main {
        return Err(crate::diagnostics::unplaced(
            Code::Syntax,
            "a module with regions in kernel form writes `main` in kernel form",
        ));
    }
    let lowered = if marked.unmarked > 0 {
        let (blanked, beside) = regions::blank(source, &marked.spans)
            .map_err(|refused| crate::diagnostics::unplaced(Code::Syntax, refused))?;
        Some(lower(&blanked, environment, &beside)?)
    } else {
        None
    };
    regions::assemble(
        base,
        marked.document,
        marked.captions,
        lowered,
        environment.library,
    )
    .map_err(|refused| {
        crate::diagnostics::unplaced(Code::Syntax, format!("misplaced caption: {refused}"))
    })
}

fn lower(
    source: &str,
    environment: &Environment<'_>,
    beside: &[String],
) -> Result<lash_kernel_dialect::Lowered, Diagnostic> {
    let limits = SourceLimits {
        max_bytes: source.len().max(1),
        ..SourceLimits::standard()
    };
    crate::lower_with_calls(source, environment, Mode::Module, limits, None, beside)
}
