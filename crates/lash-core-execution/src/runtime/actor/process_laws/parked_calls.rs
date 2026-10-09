//! The laws of a parked call's reopen (FIG-5557): the completion wait its
//! admission pinned is the one place the park's deadline is recorded. The
//! admission record names that wait and repeats nothing of it, so a node
//! that reopens the round reads the deadline from the wait's row, as a
//! host's discovery does, and refuses an admission whose wait is missing or
//! is not its call's.

use lash_durable::domain::OwnerKey;

use super::*;
use crate::runtime::actor::round::{CallOwner, PinnedWaits};

/// A step parked on its completion wait is reopened from that wait. A
/// backend assembled anew over the same stores folds the step's park
/// deadline from the wait's row and discovers the parked call with the same
/// deadline; the fold follows the row, refuses the admission without it or
/// with a row bound to another call, tool or actor; and a successor node
/// that takes the process over settles the step from the wait's resolution
/// without entering its body again.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_parked_call_is_reopened_from_its_completion_wait_after_a_restart_and_a_handover(
    backend: &Backend,
) -> LawOutcome {
    let first = law_backend(backend)?;
    let serving = serve(&first);
    let tag = tag("park-reopen");
    let parked = async {
        let process = root(&first, payload(&tag, "park")).await?;
        eventually(
            SETTLE,
            "the parked step's process released waiting",
            || async {
                Ok(PARKED_KEYS
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .contains_key(&tag)
                    && settled_waiting(&first, &process).await?)
            },
        )
        .await?;
        Ok::<_, LawBroken>(process)
    }
    .await;
    serving.stop().await;
    let process = parked?;

    let restarted = law_backend(backend)?;
    let owner = OwnerKey::Process(process.clone());
    let rows = restarted.durable().run_records(&owner).await?;
    let stored = PinnedWaits::read(restarted.durable().as_ref(), &rows).await?;
    let fold_with = |waits: &PinnedWaits| round::fold(&rows, &PolicyView::default(), waits);
    let fold = fold_with(&stored).map_err(|refusal| LawBroken(refusal.to_string()))?;
    let draft = fold
        .rounds()
        .next()
        .and_then(|view| view.members().first())
        .map(|member| member.draft().clone())
        .ok_or_else(|| LawBroken("the step was never admitted".to_owned()))?;
    let pinned = draft
        .pinned_wait()
        .ok_or_else(|| LawBroken("the parked step's admission pinned no wait".to_owned()))?;
    let row = restarted
        .durable()
        .wait(&pinned.id)
        .await?
        .ok_or_else(|| LawBroken("the pinned completion wait has no row".to_owned()))?;
    let WaitPurpose::ToolCompletion {
        call,
        tool,
        deadline: Some(deadline),
    } = row.purpose.clone()
    else {
        return Err(LawBroken(format!(
            "the pinned wait is {:?}, not a completion with a deadline",
            row.purpose
        )));
    };
    let park_at = |at| Some(ParkDeadline::At(WaitDeadline::at_instant(at)));
    ensure!(
        draft.park() == park_at(deadline),
        "the reopened step parks {:?}, not until its wait's deadline {deadline:?}",
        draft.park()
    );
    let discovered = round::parked(&restarted, CallOwner::Process(process.clone())).await?;
    ensure!(
        discovered.len() == 1
            && discovered[0].call_id == *draft.call()
            && discovered[0].tool_id == *draft.tool()
            && discovered[0].deadline == Some(deadline),
        "discovery lists {} parked calls, or not the step under its wait's deadline",
        discovered.len()
    );

    // The fold follows the wait's row: another deadline on the row is the
    // deadline the reopened step parks under.
    let rebound = |purpose| {
        let mut row = row.clone();
        row.purpose = purpose;
        PinnedWaits::default().with([row])
    };
    let later = lash_durable::DurableInstant(deadline.0 + 1);
    let moved = fold_with(&rebound(WaitPurpose::ToolCompletion {
        call: call.clone(),
        tool: tool.clone(),
        deadline: Some(later),
    }))
    .map_err(|refusal| LawBroken(refusal.to_string()))?;
    ensure!(
        moved
            .rounds()
            .next()
            .and_then(|view| view.members().first())
            .and_then(|member| member.draft().park())
            == park_at(later),
        "the fold did not take the park's deadline from its wait's row"
    );

    // The admission is refused without its wait, and with a wait bound to
    // another call, another tool or another actor.
    let mut elsewhere = row.clone();
    elsewhere.owner =
        ActorKey::process("another-process").map_err(|error| LawBroken(error.to_string()))?;
    let unbound = [
        ("no row", PinnedWaits::default()),
        (
            "another call",
            rebound(WaitPurpose::ToolCompletion {
                call: crate::ToolCallId::fixture("another-call"),
                tool: tool.clone(),
                deadline: Some(deadline),
            }),
        ),
        (
            "another tool",
            rebound(WaitPurpose::ToolCompletion {
                call: call.clone(),
                tool: crate::ToolId::new("another-tool"),
                deadline: Some(deadline),
            }),
        ),
        (
            "another purpose",
            rebound(WaitPurpose::EngineKey {
                name: "another".to_owned(),
                deadline: Some(deadline),
            }),
        ),
        ("another actor", PinnedWaits::default().with([elsewhere])),
    ];
    for (what, waits) in &unbound {
        ensure!(
            matches!(
                fold_with(waits),
                Err(round::FoldRefusal::Undecodable { .. })
            ),
            "an admission whose pinned wait has {what} folded"
        );
    }

    let successor = serve_as(&restarted, "process-laws-successor");
    let result = async {
        let key = PARKED_KEYS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&tag)
            .cloned()
            .unwrap_or_default();
        let answer =
            waits::resolve_host(&restarted, &key, Resolution::Ok(json!({ "approved": tag })))
                .await?;
        ensure!(
            answer == lash_durable::domain::ResolveAnswer::Resolved,
            "the host's resolution of the parked step's key answered {answer:?}"
        );
        eventually(SETTLE, "the successor ended the process", || async {
            Ok(terminal(&restarted, &process).await?.is_some())
        })
        .await?;
        let outcome = terminal(&restarted, &process).await?.unwrap_or_default();
        ensure!(
            find(&outcome, "settled") == Some(&json!("completed"))
                && outcome.to_string().contains("approved"),
            "the reopened step settled as {outcome}, not from its wait's resolution"
        );
        ensure!(
            step_entries(draft.call()) == 1,
            "the parked step's body was entered {} times",
            step_entries(draft.call())
        );
        Ok(())
    }
    .await;
    successor.stop().await;
    result
}
