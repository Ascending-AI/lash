use super::tests::{RUN, answer, ideal, smoke, verdict};
use super::{LoadEventRow, WitnessSnapshot};
use serde_json::{Value, json};

pub(crate) fn report() -> Value {
    let prefill = crate::load::behavior::prefill(&smoke(), RUN).unwrap();
    json!({"op":"behaviors", "prefill":{"expected":prefill,"reopened":prefill},
        "admin":{"before":"initial", "after":"admin", "summary":"load administrative summary", "applied":true},
        "pressure":{"before":"admin", "after":"pressure", "summary":"load pressure summary", "applied":true},
        "auxiliary":{"key":format!("{RUN}/behaviors/llm"), "output":"auxiliary answer"},
        "external":{"key":format!("{RUN}/behaviors/event"),"started":["process"],"outputs":[{"key":format!("{RUN}/behaviors/event"),"revision":1}]},
        "edit":{"revision":2,"listed_revision":2,"started":["edited-process"],"outputs":[{"key":format!("{RUN}/behaviors/edit"),"revision":2}],"after_delete":[]},
        "promotion":{"process_id":"process", "session_origin":true, "engine":"lashlang", "record_name":"__process_fixture", "artifact_name":"__process_fixture"}})
}

pub(crate) fn add(snapshot: &mut WitnessSnapshot) {
    snapshot.events.push(LoadEventRow {
        subject: format!("{RUN}/behaviors"),
        operation: "behaviors".into(),
        phase: "sent".into(),
        observer: "driver".into(),
        detail: json!({}),
        content_digest: None,
        recorded_at_us: 0,
    });
    snapshot.events.push(LoadEventRow {
        subject: format!("{RUN}/behaviors"),
        operation: "behaviors".into(),
        phase: "terminal".into(),
        observer: "driver".into(),
        detail: json!({"response":report()}),
        content_digest: None,
        recorded_at_us: 1,
    });
    snapshot
        .receipts
        .push((format!("{RUN}/behaviors/llm"), "load_auxiliary".into()));
    snapshot
        .commits
        .push((format!("{RUN}/behaviors/event"), "mark".into()));
    snapshot
        .commits
        .push((format!("{RUN}/behaviors/edit"), "mark".into()));
    let load = smoke();
    let generator = load.generator(RUN).unwrap();
    for actor in 0..4 {
        for ordinal in 0..6 {
            let plan = generator.plan(actor, ordinal).unwrap();
            if plan.provider_streamed && !plan.cancel {
                snapshot.receipts.push((
                    plan.operation.key(),
                    format!("load_stream_chunks_{}", plan.provider_chunks),
                ));
            }
        }
    }
}

fn planted(class: &str, field: &str, broken: Value) {
    let mut snapshot = ideal(&smoke(), lash_perf::workload::SMOKE_TURNS_PER_SESSION);
    if class == "provider-streams" {
        snapshot
            .receipts
            .retain(|(_, scenario)| !scenario.starts_with("load_stream_chunks_"));
    } else {
        let event = snapshot
            .events
            .iter_mut()
            .find(|e| e.operation == "behaviors" && e.phase == "terminal")
            .unwrap();
        event.detail["response"][field] = broken;
    }
    let result = verdict(&snapshot);
    assert!(
        result
            .classes
            .get(class)
            .is_some_and(|t| !t.violations.is_empty()),
        "planted violation escaped {class}: {:?}",
        result.lines()
    );
}

#[test]
fn missing_provider_chunks_are_a_stream_violation() {
    planted("provider-streams", "", Value::Null);
}
#[test]
fn lost_prefill_is_a_history_violation() {
    planted(
        "history-prefill",
        "prefill",
        json!({"expected":["old-user","old-answer"],"reopened":["old-user"]}),
    );
}
#[test]
fn an_admin_compaction_without_a_new_frame_is_a_violation() {
    planted(
        "admin-compaction",
        "admin",
        json!({"before":"initial","after":"initial","summary":"load administrative summary","applied":true}),
    );
}
#[test]
fn pressure_without_a_summary_frame_is_a_violation() {
    planted(
        "context-pressure",
        "pressure",
        json!({"before":"admin","after":"pressure","summary":"wrong","applied":true}),
    );
}
#[test]
fn a_wrong_auxiliary_completion_is_a_violation() {
    planted(
        "auxiliary-requests",
        "auxiliary",
        json!({"key":format!("{RUN}/behaviors/llm"),"output":"wrong"}),
    );
}
#[test]
fn an_external_occurrence_without_a_target_is_a_violation() {
    planted(
        "external-occurrences",
        "external",
        json!({"key":format!("{RUN}/behaviors/event"),"started":[],"outputs":[]}),
    );
}
#[test]
fn a_deleted_trigger_that_still_starts_work_is_a_violation() {
    planted(
        "trigger-edits",
        "edit",
        json!({"revision":2,"listed_revision":2,"started":["edited-process"],"outputs":[{"key":format!("{RUN}/behaviors/edit"),"revision":2}],"after_delete":["late"]}),
    );
}
#[test]
fn a_promotion_read_of_another_process_is_a_violation() {
    planted(
        "promotion-reads",
        "promotion",
        json!({"process_id":"process","session_origin":true,"engine":"lashlang","record_name":"on_event","artifact_name":"other"}),
    );
}
#[test]
fn a_batched_turn_keeps_full_tool_coverage_and_zero_violations() {
    let mut snapshot = ideal(&smoke(), lash_perf::workload::SMOKE_TURNS_PER_SESSION);
    let generator_load = smoke();
    let generator = generator_load.generator(RUN).unwrap();
    let queued = (0..6)
        .map(|i| generator.plan(2, i).unwrap())
        .flat_map(|p| p.queued_inputs)
        .find(|q| !q.cancel)
        .unwrap();
    let turn = (0..6)
        .map(|i| generator.plan(2, i).unwrap())
        .find(|p| {
            !p.cancel
                && !p.tool_batches.is_empty()
                && p.operation.ordinal
                    > lash_perf::workload::OperationId::parse(&queued.idempotency_key)
                        .unwrap()
                        .0
                        .ordinal
        })
        .unwrap();
    let key = turn.operation.key();
    let root = crate::load::turn_id_for(&key);
    answer(&mut snapshot, &key, &root, &queued.idempotency_key);
    answer(
        &mut snapshot,
        &queued.idempotency_key,
        &root,
        &queued.idempotency_key,
    );
    for event in &mut snapshot.events {
        if event.phase != "terminal" || event.operation != "turn" {
            continue;
        }
        if event.detail["response"]["operation"] == key {
            event.detail["response"]["outcome"]["final_value"]["operations"] =
                json!([key, queued.idempotency_key]);
        }
        for input in event.detail["response"]["queued"]
            .as_array_mut()
            .into_iter()
            .flatten()
        {
            if input["key"] == queued.idempotency_key {
                input["outcome"]["final_value"]["operations"] =
                    json!([key, queued.idempotency_key]);
            }
        }
    }
    let result = verdict(&snapshot);
    assert!(result.passed(), "{:?}", result.lines());
    let tool = &turn.tool_batches[0][0].idempotency_key;
    snapshot.commits.retain(|(k, _)| k != tool);
    assert!(!verdict(&snapshot).classes["tools"].violations.is_empty());
}
