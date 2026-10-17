use crate::*;
pub fn process_event_invocation(
    process_id: &ProcessId,
    sequence: u64,
    event_type: &str,
    replay: Option<RuntimeReplay>,
) -> RuntimeInvocation {
    RuntimeInvocation {
        attribution: RuntimeAttribution::none(),
        subject: RuntimeSubject::ProcessEvent {
            process_id: process_id.clone(),
            sequence,
            event_type: event_type.to_string(),
        },
        caused_by: Some(CausalRef::Process {
            process_id: process_id.clone(),
        }),
        replay,
    }
}
