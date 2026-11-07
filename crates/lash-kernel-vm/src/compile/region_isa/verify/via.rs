//! A protocol `for`'s rows (`K-ITER-005`): its hook calls, where each
//! returns, and the Test that binds a pass.
use super::*;

/// A hook returns to the header row that takes its result, or `item`'s to
/// the Test that binds it.
pub(super) fn hook_edge(
    draft: &RegionDraft,
    step: &Step,
    hook: Phase,
    next: Pc,
) -> Result<(), Refused> {
    // A `return via` policy's result is taken at its Restore.
    let resumed = match hook {
        Phase::Item => Some(Phase::Test),
        Phase::Policy => Some(Phase::Restore),
        _ => None,
    };
    let expected = match super::super::stages::returned(draft, &step.at)? {
        Some(stage) => stage,
        None if let Some(phase) = resumed => super::super::stages::event_pc(
            draft,
            &Point {
                site: step.at.site.clone(),
                phase,
            },
        )?,
        None => return Err(Refused::Path),
    };
    if hook != step.at.phase || next != expected {
        return Err(Refused::Path);
    }
    Ok(())
}

/// `next` and `check` step the loop's cursor; `item` takes what its header
/// holds besides.
pub(super) fn hook_arguments(
    draft: &RegionDraft,
    step: &Step,
    hook: Phase,
) -> Result<Vec<Reg<LeaseValue>>, Refused> {
    let site = &step.at.site;
    if hook == Phase::Policy {
        return Ok(vec![register(
            draft,
            &Owner::ActiveDeparture(draft.source.site.clone()),
        )?]);
    }
    std::iter::once(register(draft, &Owner::LoopCursor(site.clone())))
        .chain((hook == Phase::Item).then(|| register(draft, &Owner::LoopHeader(site.clone()))))
        .collect()
}

/// The Test binds the item its header holds, or `item`'s result.
pub(super) fn check_item(
    source: &Source,
    step: &Step,
    at: &Site,
    bound: bool,
) -> Result<(), Refused> {
    if *at != step.at.site
        || step.at.phase != Phase::Test
        || !matches!(
            resources::node(source, at)?,
            doc::Node::Stmt(doc::Stmt::For { via: Some(_), .. })
        )
    {
        return Err(Refused::Source);
    }
    if !bound {
        return Err(Refused::Facts);
    }
    Ok(())
}
