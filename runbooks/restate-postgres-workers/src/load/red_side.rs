//! Plant verifier violations in copies of the real witness, preserving the
//! observed run. Every newly executed class must reject its own mutation.
use super::{LoadContext, behavior, verify};
use anyhow::{Result, ensure};
use serde_json::json;

pub fn checks(
    load: &LoadContext,
    run: &str,
    snapshot: &verify::WitnessSnapshot,
) -> Result<Vec<String>> {
    let baseline = verify::verify(load, run, snapshot)?;
    let mut lines = Vec::new();
    for class in behavior::CLASSES {
        ensure!(
            baseline.classes[class].witnessed > 0 && baseline.classes[class].violations.is_empty(),
            "cannot plant a discriminating {class} violation without green observed evidence"
        );
        let mut planted = snapshot.clone();
        if class == "provider-streams" {
            planted
                .receipts
                .retain(|(_, scenario)| !scenario.starts_with("load_stream_chunks_"));
        } else {
            let event = planted
                .events
                .iter_mut()
                .rev()
                .find(|event| {
                    event.evidence.operation() == "behaviors"
                        && event.evidence.phase() == "terminal"
                })
                .ok_or_else(|| anyhow::anyhow!("no bounded behavior terminal to mutate"))?;
            let report = &mut event.detail["response"];
            match class {
                "history-prefill" => report["prefill"]["reopened"] = json!([]),
                "admin-compaction" => report["admin"]["after"] = report["admin"]["before"].clone(),
                "context-pressure" => {
                    report["pressure"]["summary"] = json!("planted wrong summary")
                }
                "auxiliary-requests" => {
                    report["auxiliary"]["output"] = json!("planted wrong output")
                }
                "external-occurrences" => report["external"]["started"] = json!([]),
                "trigger-edits" => report["edit"]["after_delete"] = json!(["planted late target"]),
                "promotion-reads" => {
                    report["promotion"]["artifact_definition"] = json!("planted other definition")
                }
                _ => unreachable!("the behavior class list is exhaustive"),
            }
        }
        let verdict = verify::verify(load, run, &planted)?;
        let violations = verdict.classes[class].violations.len();
        ensure!(
            violations > 0,
            "planted {class} violation escaped the verifier"
        );
        lines.push(format!(
            "load red-side class={class} planted=1 caught={violations} verdict=failed"
        ));
    }
    Ok(lines)
}
