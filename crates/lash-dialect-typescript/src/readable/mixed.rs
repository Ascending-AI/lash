//! Ordinary authored source read through the front end.

use lash_kernel_dialect::regions::RegionRead;
use lash_kernel_dialect::{CellEnvironment, Environment, GeneratedSourceLimits, Lowered, Mode};
use lash_kernel_doc as k;

use crate::{Diagnostic, DiagnosticCode, SourceLimits};

pub(super) fn read(
    source: &str,
    base: &k::Document,
    environment: &Environment<'_>,
    limits: GeneratedSourceLimits,
) -> Result<RegionRead, Diagnostic> {
    if source.len() > limits.max_bytes {
        return Err(Diagnostic::new(
            DiagnosticCode::GeneratedSourceBytes,
            "generated source exceeds byte budget",
            None,
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
) -> Result<Lowered, Diagnostic> {
    let limits = SourceLimits::new(source.len().max(1), crate::MAX_SOURCE_NESTING_DEPTH)?;
    if cell {
        let program = crate::adapter::parse_with_limits(source, limits)?;
        crate::lower::lower_beside(
            &program,
            source,
            environment,
            Mode::Cell(CellEnvironment::first()),
            beside,
        )
    } else {
        let program = crate::adapter::parse_workflow(source, limits)?;
        crate::lower::lower_beside(&program, source, environment, Mode::Module, beside)
    }
}
