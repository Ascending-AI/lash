//! Reading printed source back: lifted source through the front end, and
//! the regions written in kernel form through the kernel reader.

use lash_kernel_dialect::regions::{self, RegionRead};
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
    let marked = crate::intrinsics::read_marked(source, base, limits)?;
    if marked.spans.is_empty() {
        let lowered = lower(source, environment, base.entries.is_empty(), &[])?;
        return Ok(RegionRead {
            document: lowered.document.document().clone(),
            captions: lowered.captions,
            map: lowered.source_map,
        });
    }
    if !marked.main {
        return Err(Diagnostic::new(
            DiagnosticCode::GeneratedSourceFormat,
            "a module with regions in kernel form writes `main` in kernel form",
            None,
        ));
    }
    let spans: Vec<_> = marked
        .spans
        .iter()
        .map(|span| with_caption(source, span.clone()))
        .collect();
    let lowered = if marked.unmarked > 0 {
        let (blanked, beside) = regions::blank(source, &spans)
            .map_err(|refused| Diagnostic::new(DiagnosticCode::InvalidAst, refused, None))?;
        Some(lower(&blanked, environment, false, &beside)?)
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
        Diagnostic::new(
            DiagnosticCode::InvalidAst,
            format!("misplaced caption: {refused}"),
            None,
        )
    })
}

/// `span`, widened to the caption written right before it.
fn with_caption(source: &str, span: std::ops::Range<usize>) -> std::ops::Range<usize> {
    let before = source[..span.start].trim_end();
    if before.ends_with("*/")
        && let Some(open) = before.rfind("/**")
        && !before[open..before.len() - 2].contains("*/")
    {
        return open..span.end;
    }
    span
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
