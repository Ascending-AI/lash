//! The deferred tool provider: what resolves the call paths a run's surface
//! or a process's links do not offer (S6).
//!
//! A session or a process env registers at most one. It is a tool provider
//! under a declared key that advertises nothing and resolves on demand, and
//! each surface records its key. Resolution is read-only discovery: it is
//! repeat-safe and effect-free until its [`crate::DeferredMatch`] commits,
//! and a committed match is never resolved again.

use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::plugin::OwnerConfigView;
use crate::{DeferredOutcome, ToolProvider};

/// Who one deferred resolution resolves for: the execution whose call paths
/// are resolved, the logical Run it belongs to, the capability refs that
/// Run's recorded shape names, and the configuration it runs under.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct DeferredResolveContext<'a> {
    /// The session frame or process whose call paths are resolved.
    pub owner: &'a crate::ExecutionOwner,
    /// The admitted logical Run; `None` for a process.
    pub run: Option<&'a crate::runtime::TurnAddress>,
    /// The capability refs the Run's recorded shape names, by slot.
    pub capabilities: &'a BTreeMap<crate::SlotId, crate::CapabilityRef>,
    /// The run's resolved configuration, or the process env's.
    pub config: &'a OwnerConfigView,
}

impl<'a> DeferredResolveContext<'a> {
    pub fn new(
        owner: &'a crate::ExecutionOwner,
        run: Option<&'a crate::runtime::TurnAddress>,
        capabilities: &'a BTreeMap<crate::SlotId, crate::CapabilityRef>,
        config: &'a OwnerConfigView,
    ) -> Self {
        Self {
            owner,
            run,
            capabilities,
            config,
        }
    }
}

/// A tool provider that resolves call paths no surface offers. It
/// advertises nothing; a resolved match names the provider that executes
/// it, which may be this one.
#[async_trait]
pub trait DeferredToolProvider: ToolProvider {
    /// Resolve a deterministic batch of call paths. A path omitted from the
    /// returned map is recorded as [`DeferredOutcome::Unavailable`], and
    /// entries for paths outside the batch are ignored.
    ///
    /// This is read-only discovery: it must not install routes or perform
    /// externally visible work, since a resolution that has not committed
    /// may run again. What a resolved tool needs to route its calls travels
    /// in the match's [`crate::ExecutionBinding`].
    async fn resolve(
        &self,
        cx: &DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> BTreeMap<String, DeferredOutcome>;
}

/// Maximum recorded host-note length in UTF-8 bytes, including the marker.
pub const MAX_DEFERRED_NOTE_BYTES: usize = 512;

/// `outcome` with its note bounded to [`MAX_DEFERRED_NOTE_BYTES`].
pub fn bounded_outcome(outcome: DeferredOutcome) -> DeferredOutcome {
    const MARKER: &str = " [truncated]";
    let bound = |mut note: String| {
        if note.len() > MAX_DEFERRED_NOTE_BYTES {
            let mut end = MAX_DEFERRED_NOTE_BYTES - MARKER.len();
            while !note.is_char_boundary(end) {
                end -= 1;
            }
            note.truncate(end);
            note.push_str(MARKER);
        }
        note
    };
    match outcome {
        DeferredOutcome::Unavailable { note } => DeferredOutcome::Unavailable {
            note: note.map(bound),
        },
        DeferredOutcome::Refused { reason } => DeferredOutcome::Refused {
            reason: bound(reason),
        },
        resolved @ DeferredOutcome::Resolved { .. } => resolved,
    }
}
