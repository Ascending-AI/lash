//! Host source admission within native parser safety ceilings.
use super::{Diagnostic, DiagnosticCode, MAX_SOURCE_BYTES, MAX_SOURCE_NESTING_DEPTH};

/// Host admission for TypeScript source. The source-byte hard maximum is
/// 128 KiB: the standard parser stack reserves at most 4.9 GiB at that size.
/// The 28-level nesting hard maximum protects recursive tree conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceLimits {
    max_bytes: usize,
    max_nesting: usize,
}
impl SourceLimits {
    pub const HARD_MAX_BYTES: usize = 128 * 1024;
    pub const fn standard() -> Self {
        Self {
            max_bytes: MAX_SOURCE_BYTES,
            max_nesting: MAX_SOURCE_NESTING_DEPTH,
        }
    }
    pub fn new(max_bytes: usize, max_nesting: usize) -> Result<Self, Diagnostic> {
        if max_bytes == 0
            || max_bytes > Self::HARD_MAX_BYTES
            || max_nesting == 0
            || max_nesting > MAX_SOURCE_NESTING_DEPTH
        {
            return Err(Diagnostic::new(
                DiagnosticCode::ParseResourcesUnavailable,
                format!(
                    "TypeScript source limits require 1..={} bytes and 1..={MAX_SOURCE_NESTING_DEPTH} nesting levels",
                    Self::HARD_MAX_BYTES
                ),
                None,
            ));
        }
        Ok(Self {
            max_bytes,
            max_nesting,
        })
    }
    pub const fn max_bytes(self) -> usize {
        self.max_bytes
    }
    pub const fn max_nesting(self) -> usize {
        self.max_nesting
    }
}
impl Default for SourceLimits {
    fn default() -> Self {
        Self::standard()
    }
}
