//! FIG-6953: the readable print of every corpus document and of its
//! canvas edit, in both dialects. Regeneration checks each print against its laws before
//! any text is written:
//!
//! - READABLE-FIXPOINT: print, read and print again gives the same text.
//! - READABLE-IDENTITY: a print with no region in kernel form lowers, as a
//!   plain workflow module under the import's own environment and with no
//!   base, to the document it was printed from: the same identity and the
//!   same captions.
use anyhow::{Result, ensure};
use lash_kernel_doc::{Captions, Document, SourceLanguage};

use super::Printing;

/// The directive that opens a region printed in kernel form.
const KERNEL_FORM: &str = "\"use kernel\"";

/// The kernel scaffolding a lifted region never spells.
pub const SCAFFOLDING: [&str; 4] = ["await await", "helper_", "__ts_brand", "k.returnVia"];

pub(super) fn laws(
    document: &Document,
    captions: &Captions,
    source: &str,
    lang: SourceLanguage,
    printing: &Printing<'_>,
) -> Result<()> {
    let library = printing.environment.library;
    let options = lash_kernel_dialect::PrintOptions::default();
    let again = match lang {
        SourceLanguage::TypeScript => {
            let (read, captions) =
                lash_dialect_typescript::read(source, document, library, Default::default(), None)?;
            lash_dialect_typescript::print(&read, Some(&captions), library, options)?
        }
        SourceLanguage::Python => {
            let (read, captions) =
                lash_dialect_python::read(source, document, library, Default::default(), None)?;
            lash_dialect_python::print(&read, Some(&captions), library, options)?
        }
    };
    ensure!(
        again.source == source,
        "READABLE-FIXPOINT: print -> read -> print changed the text\n{source}\n---\n{}",
        again.source
    );
    if !source.contains(KERNEL_FORM) {
        let lowered = super::lower(source, lang, printing.environment, printing.mode)
            .map_err(|diagnostic| anyhow::anyhow!("READABLE-IDENTITY: {diagnostic}\n{source}"))?;
        ensure!(
            lowered.document.id() == document.identity(),
            "READABLE-IDENTITY: the lifted print lowers to another document\n{source}"
        );
        ensure!(
            lowered.captions.entries() == captions.entries(),
            "READABLE-IDENTITY: the lifted print lowers with other captions\n{source}"
        );
    }
    Ok(())
}

/// How one document printed.
#[derive(Clone, Debug)]
pub struct Lifted {
    pub case: String,
    pub language: SourceLanguage,
    /// Whether this is the case's canvas edit.
    pub edited: bool,
    /// Its regions: each declared function, and `main`.
    pub regions: usize,
    /// Those printed as authored source.
    pub lifted: usize,
    pub source: String,
}

impl Lifted {
    pub(super) fn of(
        case: &str,
        language: SourceLanguage,
        edited: bool,
        document: &Document,
        source: &str,
    ) -> Self {
        let regions = document.functions.len() + 1;
        Self {
            case: case.to_owned(),
            language,
            edited,
            regions,
            lifted: regions - source.matches(KERNEL_FORM).count(),
            source: source.to_owned(),
        }
    }

    /// Whether every region printed as authored source.
    pub fn whole(&self) -> bool {
        self.lifted == self.regions
    }
}

/// Every corpus document and canvas edit, printed and checked against the
/// laws.
pub fn readable_prints(root: &std::path::Path) -> Result<Vec<Lifted>> {
    super::render_with(root).map(|(_, report)| report)
}
