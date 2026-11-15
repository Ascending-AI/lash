//! A parked bytecode-core run carried onto its successor's code by the run
//! format's id rewrite (cutover contract §6).
use crate::{CarryFault, ParkedRefusal};
use lash_kernel_doc::heap::{PageId, PageRef};
use lash_kernel_state::kernel_internals;
use lash_kernel_state::paged::run::rewrite::{
    LiveCode, RewriteFault, RunCorrespondence, Unmapped, live_code, rewrite, rewrite_effect,
};
use lash_kernel_state::paged::{Checkpoint, PageBody, PageKind, PageSource, RootStatus};
use lash_kernel_value::kernel_internals::{effect_id, effect_parts};
use lash_kernel_value::names::DocumentId;
use lash_kernel_value::sites::EffectId;
use lash_kernel_value::tokens::encode_body;

fn refused(detail: impl Into<String>) -> CarryFault {
    ParkedRefusal::StateNotCarried {
        detail: detail.into(),
    }
    .into()
}

/// The code the parked run under `checkpoint` names.
///
/// # Errors
///
/// A refusal for an ended run; the page port's faults.
pub fn run_code(
    source: &mut dyn PageSource,
    checkpoint: &Checkpoint,
) -> Result<LiveCode, CarryFault> {
    let root = kernel_internals::checkpoint_root(checkpoint);
    if !matches!(root.status, RootStatus::Parked) {
        return Err(refused("an ended image cannot be carried"));
    }
    Ok(live_code(source, root)?)
}

fn carry_fault(fault: RewriteFault) -> CarryFault {
    match fault {
        RewriteFault::Heap(fault) => CarryFault::Heap(fault),
        RewriteFault::Unmapped(Unmapped::Function(function)) => {
            ParkedRefusal::FunctionNotCarried { function }.into()
        }
        RewriteFault::Unmapped(Unmapped::Site { site, held }) => ParkedRefusal::SiteNotCarried {
            site,
            held: held.into(),
        }
        .into(),
        other => refused(other.to_string()),
    }
}

/// The code a carried run continues under.
#[derive(Clone, Copy, Debug)]
pub struct Successor {
    pub document: DocumentId,
    pub kernel: u32,
    pub closure: PageId,
    pub helper_library: lash_kernel_value::HelperLibraryId,
}

/// The parked run under `checkpoint`, its code ids rewritten by `map` and
/// sealed as a root of `successor`'s code. Nothing is sealed when `map`
/// misses an id the run names.
///
/// # Errors
///
/// [`ParkedRefusal::FunctionNotCarried`] for a function `map` has no image
/// for, [`ParkedRefusal::StateNotCarried`] for any other id; the page
/// port's faults.
pub fn carry_run(
    source: &mut dyn PageSource,
    checkpoint: &Checkpoint,
    map: &RunCorrespondence,
    successor: Successor,
) -> Result<PageRef, CarryFault> {
    let mut root =
        rewrite(source, kernel_internals::checkpoint_root(checkpoint), map).map_err(carry_fault)?;
    root.document = successor.document;
    root.kernel = successor.kernel;
    root.closure = successor.closure;
    root.helper_library = successor.helper_library;
    Ok(source.seal(PageBody {
        kind: PageKind::CHECKPOINT_ROOT,
        tokens: encode_body(&root),
        children: vec![root.heap],
    })?)
}

/// The wait `from` names, rekeyed by `map` as [`carry_run`] rekeys the
/// requests of the run whose code is `live`, so a host's wait records keep matching the run.
///
/// # Errors
///
/// As [`carry_run`], for an id `map` has no image for.
pub fn carry_effect(
    map: &RunCorrespondence,
    live: &LiveCode,
    from: &EffectId,
) -> Result<EffectId, CarryFault> {
    let mut identity = effect_parts(from).clone();
    rewrite_effect(&mut identity, map, live).map_err(carry_fault)?;
    Ok(effect_id(identity))
}
