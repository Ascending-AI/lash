//! The explicit codec variant numbers of the parked state's enums
//! (FIG-5887 §2).
//!
//! Each number is the variant's zero-based declaration position at revision
//! `9503e220ed`, transcribed once into an exhaustive match. From here on the
//! numbers are the format: a new variant takes an unused number, and no
//! variant is renumbered when the Rust declaration moves.

use lash_kernel_doc::graph_tokens::CodecVariant;

use crate::schema::{
    Bound, EffectOutcome, Ended, Entered, Incoming, PerformState, Request, Root, RootState,
    SleepOutcome, TaskState,
};

#[doc(hidden)]
impl CodecVariant for TaskState {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Ready => 0,
            Self::Resuming(..) => 1,
            Self::Performing(..) => 2,
            Self::Joining(..) => 3,
            Self::JoiningMany(..) => 4,
            Self::Ended(..) => 5,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Incoming {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Value(..) => 0,
            Self::Raise(..) => 1,
            Self::Joined(..) => 2,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Request {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Effect { .. } => 0,
            Self::Sleep { .. } => 1,
        }
    }
}

#[doc(hidden)]
impl<O> CodecVariant for PerformState<O> {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Requested => 0,
            Self::Admitted => 1,
            Self::Committed(..) => 2,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for EffectOutcome {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Completed(..) => 0,
            Self::Failed(..) => 1,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for SleepOutcome {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Elapsed => 0,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Ended {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Returned(..) => 0,
            Self::Raised(..) => 1,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Bound {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Value(..) => 0,
            Self::Shared(..) => 1,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Entered {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Normal => 0,
            Self::Throw => 1,
            Self::Return => 2,
            Self::Break => 3,
            Self::Continue => 4,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for Root {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Session(..) => 0,
            Self::Task(..) => 1,
            Self::Call { .. } => 2,
            Self::Held { .. } => 3,
        }
    }
}

#[doc(hidden)]
impl CodecVariant for RootState {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Session(..) => 0,
            Self::Task(..) => 1,
            Self::Call(..) => 2,
            Self::Held(..) => 3,
        }
    }
}
