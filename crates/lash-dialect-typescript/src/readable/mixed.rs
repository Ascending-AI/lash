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
    source_limits: Option<SourceLimits>,
) -> Result<RegionRead, Diagnostic> {
    if source.len() > limits.max_bytes {
        return Err(Diagnostic::new(
            DiagnosticCode::GeneratedSourceBytes,
            "generated source exceeds byte budget",
            None,
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
) -> Result<Lowered, Diagnostic> {
    let limits = match source_limits {
        Some(limits) => limits,
        None => SourceLimits::new(source.len().max(1), crate::MAX_SOURCE_NESTING_DEPTH)?
            .with_implicit_text_conversion(
                lash_kernel_dialect::workflow::ImplicitTextConversionPolicy::Allow,
            ),
    };
    if cell {
        let program = crate::adapter::parse_with_limits(source, limits)?;
        crate::lower::lower_beside(
            &program,
            source,
            environment,
            Mode::Cell(CellEnvironment::first()),
            beside,
            limits.implicit_text_conversion(),
        )
    } else {
        let program = crate::adapter::parse_workflow(source, limits)?;
        crate::lower::lower_beside(
            &program,
            source,
            environment,
            Mode::Module,
            beside,
            limits.implicit_text_conversion(),
        )
    }
}
