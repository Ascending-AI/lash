//! The explicit codec variant numbers of the persisted tool-run outcomes (FIG-5887 §2).
//!
//! Each number is the variant's zero-based declaration position at revision
//! `9503e220ed`, transcribed once into an exhaustive match. From here on the
//! numbers are the format: a new variant takes an unused number, and no
//! variant is renumbered when the Rust declaration moves.

use lash_kernel_doc::graph_tokens::CodecVariant;

use crate::tool_run::SettledOutput;
use crate::tool_run::run_event::{AttemptOutcome, KnownFailureReason};

#[doc(hidden)]
impl CodecVariant for AttemptOutcome {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Completed(..) => 0,
            Self::Waiting(..) => 1,
            Self::Failed(..) => 2,
            Self::Interrupted => 3,
            Self::TimedOut { .. } => 4,
            Self::Cancelled { .. } => 5,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for KnownFailureReason {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Reported => 0,
            Self::DeclarationRefused => 1,
            Self::StartRefused => 2,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for SettledOutput {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Completed(..) => 0,
            Self::Failed(..) => 1,
            Self::Waiting(..) => 2,
            Self::Interrupted => 3,
            Self::TimedOut { .. } => 4,
            Self::Cancelled { .. } => 5,
        }
    }
}
