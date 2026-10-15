//! The explicit codec variant numbers of the persisted round enums (FIG-5887 §2).
//!
//! Each number is the variant's zero-based declaration position at revision
//! `9503e220ed`, transcribed once into an exhaustive match. From here on the
//! numbers are the format: a new variant takes an unused number, and no
//! variant is renumbered when the Rust declaration moves.
//!
//! A RoundFact's own variants (Admit, Start, Wait, Retry, Present, Outcome,
//! CellReport: 0 to 3 and 5 to 7; 4 is retired) are pinned by
//! `lash_durable::domain::graph::RunFactKind::round_fact_variant`.

use lash_core_store::checkpoint_graph::CodecVariant;

use super::SettledOutput;

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
