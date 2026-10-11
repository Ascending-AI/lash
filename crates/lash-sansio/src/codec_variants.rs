//! The explicit codec variant numbers of the persisted control enums (FIG-5887 §2).
//!
//! Each number is the variant's zero-based declaration position at revision
//! `9503e220ed`, transcribed once into an exhaustive match. From here on the
//! numbers are the format: a new variant takes an unused number, and no
//! variant is renumbered when the Rust declaration moves.

use lash_kernel_doc::graph_tokens::CodecVariant;

use crate::session::CellControl;
use crate::tool_contract::LimitCause;
use crate::tool_declaration::TurnControlKind;
use crate::tool_output::{ToolControl, TurnControl};

#[doc(hidden)]
impl CodecVariant for TurnControlKind {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Finish => 0,
            Self::SwitchAgentFrame => 1,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for TurnControl {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Finish { .. } => 0,
            Self::SwitchAgentFrame { .. } => 1,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for ToolControl {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Turn { .. } => 0,
            Self::Fail { .. } => 1,
            Self::AbortRun { .. } => 2,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for CellControl {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Finish { .. } => 0,
            Self::SwitchAgentFrame { .. } => 1,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for LimitCause {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::ExecutionSlice => 0,
            Self::ExecutionTotal => 1,
            Self::WaitDeadline => 2,
        }
    }
}
