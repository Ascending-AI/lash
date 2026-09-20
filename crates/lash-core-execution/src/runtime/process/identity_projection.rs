//! Identity-projection helpers shared by the trigger-definition and
//! process-registration fingerprint preimages.
//!
//! Both durable families project the same process vocabulary — event types,
//! value selectors, and the canonical payload/schema leaves — so the
//! projection grammar lives exactly once: a change to how a value or a
//! process status is projected cannot drift the two families' fingerprints
//! for the same underlying fact.

use crate::stable_identity::IdentityEncoder;

pub(crate) fn project_process_event_type(
    identity: &mut IdentityEncoder,
    event_type: &crate::ProcessEventType,
) {
    let crate::ProcessEventType {
        name,
        payload_schema,
        semantics,
    } = event_type;
    identity.string(name);
    let crate::LashSchema { schema } = payload_schema;
    project_process_schema_leaf(identity, schema);
    let crate::ProcessEventSemanticsSpec { terminal, wake } = semantics;
    identity.optional(terminal.as_ref(), |identity, terminal| {
        let crate::ProcessTerminalSpec {
            status,
            await_output,
        } = terminal;
        identity.tag(match status {
            crate::ProcessStatus::Running => 1,
            crate::ProcessStatus::Waiting => 2,
            crate::ProcessStatus::Completed => 3,
            crate::ProcessStatus::Failed => 4,
            crate::ProcessStatus::Cancelled => 5,
            crate::ProcessStatus::Abandoned => 6,
            crate::ProcessStatus::CallerDeparted => 7,
        });
        identity.optional(await_output.as_ref(), project_process_value_selector);
    });
    identity.optional(wake.as_ref(), |identity, wake| {
        let crate::ProcessWakeSpec { when, input } = wake;
        identity.optional(when.as_ref(), project_process_value_selector);
        project_process_value_selector(identity, input);
    });
}

pub(crate) fn project_process_value_selector(
    identity: &mut IdentityEncoder,
    selector: &crate::ProcessValueSelector,
) {
    match selector {
        crate::ProcessValueSelector::Payload => identity.tag(1),
        crate::ProcessValueSelector::Pointer(pointer) => {
            identity.tag(2);
            identity.string(pointer);
        }
        crate::ProcessValueSelector::Const(value) => {
            identity.tag(3);
            project_process_payload_leaf(identity, value);
        }
        crate::ProcessValueSelector::Template { template, fields } => {
            identity.tag(4);
            identity.string(template);
            identity.sequence(fields.iter(), |identity, (name, selector)| {
                identity.string(name);
                project_process_value_selector(identity, selector);
            });
        }
        crate::ProcessValueSelector::Present(pointer) => {
            identity.tag(5);
            identity.string(pointer);
        }
    }
}

pub(crate) fn project_process_payload_leaf(
    identity: &mut IdentityEncoder,
    value: &serde_json::Value,
) {
    identity.bytes(&crate::identity_json::payload_leaf(value));
}

pub(crate) fn project_process_schema_leaf(
    identity: &mut IdentityEncoder,
    value: &serde_json::Value,
) {
    identity.bytes(&crate::identity_json::schema_leaf(value));
}
