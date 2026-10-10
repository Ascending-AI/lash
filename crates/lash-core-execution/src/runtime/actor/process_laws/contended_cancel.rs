//! The law of a cancel whose commit the store finds contended (FIG-5855):
//! contention is not the cancel's answer. The store runs the identical
//! commit again, so the cancel is accepted, recorded once, and ends the
//! process.

use super::*;

use crate::ProcessEventKind;

/// A live process whose cancel commit meets contention is still cancelled:
/// the cancel answers success, the process ends cancelled by the operator,
/// and its history holds one cancel request and one terminal.
///
/// # Errors
///
/// The first rule broken. The dialect may inject contention into the
/// cancel's commit.
pub async fn a_contended_cancel_commit_still_ends_the_process_once(
    backend: &Backend,
) -> LawOutcome {
    let backend = law_backend(backend)?;
    let serving = serve(&backend);
    let result = async {
        let process = root(&backend, payload(&tag("contended-cancel"), "hold")).await?;
        eventually(SETTLE, "the process waits", || {
            settled_waiting(&backend, &process)
        })
        .await?;
        cancel(&backend, &process).await?;
        eventually(SETTLE, "the cancelled process ended", || async {
            Ok(terminal(&backend, &process).await?.is_some())
        })
        .await?;
        let end = terminal(&backend, &process).await?.unwrap_or_default();
        ensure!(
            cancellation(&end).is_some_and(|(origin, _)| origin == "operator_requested"),
            "the process did not end cancelled by the operator: {end}"
        );
        let events = backend
            .process_registry()
            .recent_events(&process, 1_000)
            .await
            .map_err(|error| LawBroken(error.to_string()))?;
        let count =
            |kind: ProcessEventKind| events.iter().filter(|event| event.kind() == kind).count();
        ensure!(
            count(ProcessEventKind::CancelRequested) == 1
                && count(ProcessEventKind::Cancelled) == 1,
            "the history holds {} cancel requests and {} cancelled terminals, not one each",
            count(ProcessEventKind::CancelRequested),
            count(ProcessEventKind::Cancelled)
        );
        Ok(())
    }
    .await;
    serving.stop().await;
    result
}
