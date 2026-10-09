//! Stack reservation policy, separate from immutable source admission limits.
use super::*;

/// Stack reserved before any source-proportional allowance.
///
/// It covers the parser's fixed frames and everything downstream of the parse,
/// which is bounded independently of the source: [`Adapter`] refuses to convert
/// past [`MAX_SOURCE_NESTING_DEPTH`], so the normalized tree it produces is at
/// most that deep, and the lowerer walks that tree. Only the parse and
/// the drop of its output scale with the source, which is what the allowance
/// below pays for.
pub(super) const PARSE_STACK_BASE_BYTES: usize = 8 * 1024 * 1024;

/// Parser stack reserved per source byte.
///
/// The honest worst case is **one source byte per nesting level**. An earlier
/// derivation claimed two — an opener and a closer — and that is false: `(`
/// repeated with no closers is a complete recursive-descent recursion of depth
/// `n` from `n` bytes, and SWC only discovers the problem at end of input. Worse,
/// that same shape is the most expensive *per level*, so the densest source and
/// the deepest frames coincide rather than trading off.
///
/// Measured by binary search, each attempt in its own process, on the unclosed
/// forms:
///
/// | shape | bytes per level | bytes per source byte |
/// | --- | ---: | ---: |
/// | `(` unclosed | ~19 900 | **~19 900** |
/// | `A<` unclosed | ~19 100 | ~9 300 |
/// | `{` unclosed | ~12 000 | ~12 000 |
/// | `[` unclosed | ~11 300 | ~11 300 |
/// | `a:` labels | ~11 300 | ~5 600 |
/// | `(`…`)` closed | ~20 700 | ~10 400 |
///
/// The round-7 verification measured the same shape at up to **22 540** bytes
/// per source byte, which is the figure this constant is set against. Usage is
/// linear in depth — at depths 1 000, 2 000, 4 000 and 8 000 the per-level cost
/// varies by under half a percent — which is what makes extrapolating to the
/// bound sound.
///
/// So reserving 40 000 bytes per source byte leaves a margin of roughly
/// **1.8x** (40 000 / 22 540), not the 4x an earlier comment claimed. An
/// independent check agrees: the worst shape at the bound touches 1 228 MB of
/// the 2 508 MB reserved, a **2.04x** margin by peak RSS.
///
/// Two reasons that margin is accepted rather than widened. Raising the constant
/// to restore 4x would reserve 5.9 GiB for a cap-sized cell, which makes the
/// address-space requirement in the deviation register worse — the reservation
/// already fails closed on a host with `RLIMIT_AS` under 2 GiB. And the margin
/// is *guarded*, not asserted: `tests/no_abort_guarantee.rs` runs these worst
/// shapes filled to the bound with the nesting preflight disabled, so a future
/// SWC whose frames outgrew the reservation would abort there and fail CI rather
/// than in production.
///
/// With [`MAX_SOURCE_BYTES`] at 64 KiB the largest reservation is 8 MiB +
/// 2.44 GiB. That is address space rather than memory: pages commit only when
/// touched, and an ordinary cell touches a few hundred kilobytes.
pub(super) const PARSE_STACK_BYTES_PER_SOURCE_BYTE: usize = 40_000;

/// The stack a source of this size is parsed on.
pub(super) fn parse_stack_size(source_bytes: usize) -> usize {
    PARSE_STACK_BASE_BYTES + PARSE_STACK_BYTES_PER_SOURCE_BYTE * source_bytes
}

/// Native parser stack policy. Lower reservations can fail or exhaust the stack;
/// source size and nesting refusal ceilings remain independently enforced.
#[derive(Clone, Copy, Debug)]
pub struct ParserStack {
    pub base_bytes: usize,
    pub bytes_per_source_byte: usize,
}
impl ParserStack {
    /// 8 MiB base plus 40,000 bytes per source byte. The slope has a measured
    /// 1.8x margin over the worst unclosed-parenthesis parser frames. The 8 MiB
    /// base covers fixed conversion/lowering frames with no measured sizing evidence.
    pub const fn standard() -> Self {
        Self {
            base_bytes: PARSE_STACK_BASE_BYTES,
            bytes_per_source_byte: PARSE_STACK_BYTES_PER_SOURCE_BYTE,
        }
    }
    pub(super) fn size(self, source_bytes: usize) -> Option<usize> {
        self.bytes_per_source_byte
            .checked_mul(source_bytes)?
            .checked_add(self.base_bytes)
    }
}
impl Default for ParserStack {
    fn default() -> Self {
        Self::standard()
    }
}

pub(super) fn parse_on_proportional_stack(source: &str) -> Result<Program, Diagnostic> {
    let stack_size = parse_stack_size(source.len());
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("typescript-parse".to_string())
            .stack_size(stack_size)
            .spawn_scoped(scope, || parse_source(source))
            .map_err(|error| {
                // The host could not give us the reservation. That is a
                // resource failure, not a defect in the program, and it must not
                // be reported as one: an operator reading `TS_INVALID_SHARED_AST`
                // would go and debug the cell instead of the address-space
                // limit. See MAX_SOURCE_BYTES for what the requirement is.
                Diagnostic::new(
                    DiagnosticCode::ParseResourcesUnavailable,
                    format!(
                        "the TypeScript parser could not reserve {stack_size} bytes of stack for a                          {}-byte source: {error}",
                        source.len()
                    ),
                    None,
                )
            })?;
        handle.join().unwrap_or_else(|_| {
            Err(Diagnostic::new(
                DiagnosticCode::SyntaxError,
                "the TypeScript parser failed while reading this source",
                None,
            ))
        })
    })
}
