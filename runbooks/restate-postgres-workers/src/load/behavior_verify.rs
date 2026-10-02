use super::behavior::{self, BehaviorReport};
use super::verify::{Tally, WitnessSnapshot};
use std::collections::BTreeMap;

pub(super) fn verify(
    load: &super::LoadContext,
    run: &str,
    snapshot: &WitnessSnapshot,
    classes: &mut BTreeMap<&'static str, Tally>,
) -> anyhow::Result<()> {
    let mut note = |class: &'static str, valid: bool, evidence: String| {
        let tally = classes.entry(class).or_default();
        if valid {
            tally.witnessed += 1;
        } else {
            tally.violations.push(evidence);
        }
    };
    let subject = format!("{run}/behaviors");
    let sent = snapshot.events.iter().any(|event| {
        event.evidence.operation() == "behaviors"
            && event.evidence.phase() == "sent"
            && event.subject == subject
    });
    let report = snapshot
        .events
        .iter()
        .rfind(|event| {
            event.evidence.operation() == "behaviors"
                && event.evidence.phase() == "terminal"
                && event.subject == subject
        })
        .and_then(|event| {
            serde_json::from_value::<super::LoadResponse>(event.detail["response"].clone()).ok()
        })
        .and_then(|response| {
            if let super::LoadResponse::Behaviors(report) = response {
                Some(*report)
            } else {
                None
            }
        });
    let Some(BehaviorReport {
        prefill,
        admin,
        pressure,
        auxiliary,
        external,
        edit,
        promotion,
    }) = report.filter(|_| sent)
    else {
        for class in behavior::CLASSES
            .into_iter()
            .filter(|class| *class != "provider-streams")
        {
            note(class, false, format!("{subject} has no receipted terminal"));
        }
        return Ok(());
    };
    let expected = behavior::prefill(load, run)?;
    note(
        "history-prefill",
        !expected.is_empty() && prefill.expected == expected && prefill.reopened == expected,
        format!("prefill differs after reopen: {prefill:?}"),
    );
    for (class, frame, summary) in [
        ("admin-compaction", admin, behavior::ADMIN_SUMMARY),
        ("context-pressure", pressure, behavior::PRESSURE_SUMMARY),
    ] {
        note(
            class,
            frame.applied
                && !frame.before.is_empty()
                && !frame.after.is_empty()
                && frame.before != frame.after
                && frame.summary == summary,
            format!("{class} did not open the expected summary frame: {frame:?}"),
        );
    }
    note(
        "auxiliary-requests",
        auxiliary.key == format!("{run}/behaviors/llm")
            && auxiliary.output == behavior::AUXILIARY_ANSWER
            && snapshot
                .receipts
                .iter()
                .any(|(key, scenario)| key == &auxiliary.key && scenario == "load_auxiliary"),
        format!("auxiliary result or receipt differs: {auxiliary:?}"),
    );
    let key = format!("{run}/behaviors/event");
    note(
        "external-occurrences",
        external.key == key
            && external.started.len() == 1
            && external.outputs == [serde_json::json!({"key":key,"revision":1})]
            && snapshot
                .commits
                .iter()
                .any(|(committed, _)| committed == &key),
        format!("external occurrence did not execute its target: {external:?}"),
    );
    let key = format!("{run}/behaviors/edit");
    note(
        "trigger-edits",
        edit.revision == 2
            && edit.listed_revision == 2
            && edit.started.len() == 1
            && edit.outputs == [serde_json::json!({"key":key,"revision":2})]
            && edit.after_delete.is_empty()
            && snapshot
                .commits
                .iter()
                .any(|(committed, _)| committed == &key),
        format!("edited or deleted subscription disagrees: {edit:?}"),
    );
    note(
        "promotion-reads",
        external.started.first() == Some(&promotion.process_id)
            && promotion.session_origin
            && promotion.engine == "lashlang"
            && !promotion.record_definition.is_empty()
            && promotion.artifact_definition == promotion.record_definition,
        format!("session process's immutable artifact does not resolve: {promotion:?}"),
    );
    Ok(())
}
