//! H2's external mutation receiver uses the real process registry and returns
//! its original event records. The caller lends an admitted handler scope.
use anyhow::{Result, ensure};
use lash::process::{
    ProcessEvent, ProcessEventPageEvents, ProcessEventPageMore, ProcessEventQueryMode,
    ProcessEventReadOutcome, ProcessEventType, ProcessOriginator, ProcessStartReceipt,
    ProcessStartRequest,
};
use lash::runtime::ScopedEffectController;
use serde::{Deserialize, Serialize};

pub async fn register_receiver(
    core: &lash::LashCore,
    session: &lash::SessionId,
    event_type: &str,
    scoped: ScopedEffectController<'_>,
) -> Result<ProcessStartReceipt> {
    let start = ProcessStartRequest::external(
        ProcessOriginator::host_scoped(format!("h2:{session}")),
        serde_json::json!({"fixture":"h2-receiver","session":session}),
        lash::process::Lifetime::Detached,
    )
    .with_host_start_key(format!("h2-receiver:{session}"))
    .with_observers([session.clone()])
    .with_extra_event_types([ProcessEventType {
        name: event_type.to_owned(),
        payload_schema: lash::schema::JsonSchema::admit(
            serde_json::json!({"type":"object","required":["call_id","value"],
            "properties":{"call_id":{"type":"string"},"value":{}},"additionalProperties":false}),
        )?,
        semantics: Default::default(),
    }]);
    Ok(core.processes().start(start, scoped).await?)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReceiverEvents {
    pub kind: String,
    pub process_id: lash::ProcessId,
    pub events: Vec<ProcessEvent>,
}

pub async fn receiver_events(
    core: &lash::LashCore,
    process_id: &lash::ProcessId,
) -> Result<ReceiverEvents> {
    let read = core
        .process_registry()
        .event_page_after(
            process_id,
            0,
            std::num::NonZeroUsize::new(1024)
                .ok_or_else(|| anyhow::anyhow!("receiver page must be nonzero"))?,
            ProcessEventQueryMode::Full,
        )
        .await?;
    let ProcessEventReadOutcome::Retained(page) = read else {
        anyhow::bail!("receiver events no longer retained");
    };
    ensure!(
        page.more == ProcessEventPageMore::Complete,
        "receiver evidence page truncated"
    );
    let ProcessEventPageEvents::Full(events) = page.events else {
        anyhow::bail!("receiver evidence is not full");
    };
    Ok(ReceiverEvents {
        kind: "h2_receiver_events".to_owned(),
        process_id: process_id.clone(),
        events,
    })
}
