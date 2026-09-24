use super::*;

pub(super) async fn full_events(
    core: &LashCore,
    process_id: &ProcessId,
) -> Result<Vec<lash_core::facade_support::ObservedProcessEvent>> {
    let outcome = core
        .processes()
        .events(
            crate::process::ProcessEventsFrom::Start(process_id.clone()),
            std::num::NonZeroUsize::new(64).expect("non-zero event page size"),
            lash_core::ProcessEventQueryMode::Full,
        )
        .await?
        .outcome;
    match outcome {
        lash_core::ProcessEventReadOutcome::Retained(lash_core::ProcessEventPage {
            events: lash_core::ProcessEventPageEvents::Full(events),
            more: lash_core::ProcessEventPageMore::Complete,
        }) => Ok(events),
        _ => panic!("expected one complete full event page"),
    }
}
