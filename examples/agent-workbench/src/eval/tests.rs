//! FIG-763: invalid datasets cannot run, evidence comes from a restarted
//! durable host, and hard gates dominate even a perfect rubric score.
use std::path::Path;

use anyhow::Result;
use lash::provider::ProviderHandle;

use super::*;

const LAW: &str = "eval::tests::manifest_evidence_and_recovery_obey_the_rubric_host_rule";

pub(super) fn provider() -> ProviderHandle {
    lash::testing::TestProvider::builder()
        .kind("rubric-scripted-fixture")
        .complete(|request| async move {
            let history = serde_json::to_string(&request).expect("request");
            let text = if history.contains("Continue that plan") {
                assert!(
                    history.contains("ORCHID-731"),
                    "recovered provider must see prior history"
                );
                "ORCHID-731 handover: catalog, wrap, shelve. All three steps remain planned."
            } else {
                "ORCHID-731 plan: catalog, wrap, shelve."
            };
            Ok(lash::provider::LlmResponse {
                parts: vec![lash::direct::LlmOutputPart::Text {
                    text: text.into(),
                    response_meta: None,
                }],
                ..Default::default()
            })
        })
        .build()
        .into_handle()
}

#[test]
fn manifest_evidence_and_recovery_obey_the_rubric_host_rule() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?;
    // The same exact law is the child executable under the test harness. Its
    // worker branch runs the production host and never recursively runs tests.
    if let Ok(job) = std::env::var("LASH_RUBRIC_FIXTURE_JOB") {
        let phase = std::env::var("LASH_RUBRIC_FIXTURE_PHASE")?;
        return runtime.block_on(host::worker(Path::new(&job), &phase, true));
    }
    runtime.block_on(async {
        let source = include_str!("../../evals/recovery.toml");
        Manifest::parse(source)?;
        for invalid in [
            source.replace("score_max = 4", "score_max = 5"),
            source.replace("weight = 3", "weight = 0"),
            source.replacen("floor = \"frontier\"", "floor = \"economy\"", 1),
            source.replace("driver_floor = \"frontier\"", "driver_floor = \"economy\""),
            source.replace("[\"baseline\", \"candidate\"]", "[\"candidate\", \"candidate\"]"),
            source.replace("tier = \"skill\"", "tier = \"unknown\""),
        ] { assert!(Manifest::parse(&invalid).is_err(), "invalid dataset must be refused before a worker starts"); }
        let source = source.replace("repeats = 2", "repeats = 1")
            .replace("anthropic/claude-opus-4.6", "scripted-frontier")
            .replace("openai/gpt-5.4", "scripted-candidate");
        let dataset = Manifest::parse(&source)?;
        let temporary = tempfile::tempdir()?;
        let output = temporary.path().join("matrix");
        let executable = runner::Executable { program: std::env::current_exe()?, args: vec!["--exact".into(), LAW.into(), "--nocapture".into()], fixture: true };
        let rows = runner::run(&dataset, &source, &output, &executable, &report::Judge::Scripted).await?;
        assert_eq!(rows.len(), 2, "every declared model/case/repeat executes");
        for (position, row) in rows.iter().enumerate() {
            assert!(row.accepted, "cell failures={:?}, gates={:?}; bundle={}", row.failures, row.gates, output.join(&row.bundle).display());
            assert_eq!(row.arm_position, position);
            let bundle = output.join(&row.bundle);
            let evidence: Evidence = serde_json::from_slice(&std::fs::read(bundle.join("evidence.json"))?)?;
            assert_eq!(evidence.turn_index, 2);
            assert!(host::transcript(&evidence).contains("ORCHID-731 handover"));
            assert!(bundle.join("transcript.txt").is_file());
            let typed = host::trace(&bundle.join("trace-slice.jsonl"))?;
            assert!(!typed.is_empty());
            let snapshot: Value = serde_json::from_slice(&std::fs::read(bundle.join("settled-view.json"))?)?;
            assert!(!snapshot.is_null());
            let score = report::Judge::Scripted.score(&dataset.cases[0], &dataset.judge, &evidence, &row.gates).await?;
            assert_eq!(score.score, Some(4.0));
            let mut corrupt = evidence.clone();
            corrupt.messages.pop();
            assert!(!report::accepted(&report::gates(&dataset.cases[0], &corrupt, true), score.score, 3.0, &[]), "a perfect score cannot authorize missing persisted replies");
            assert!(!report::accepted(&report::gates(&dataset.cases[0], &evidence, false), score.score, 3.0, &[]), "a score cannot stand in for a kill/restart");
        }
        assert_eq!(rows[0].input_sha256, rows[1].input_sha256, "arms have identical immutable inputs");
        assert_ne!(rows[0].bundle, rows[1].bundle, "arms never share stores");
        let matrix: Value = serde_json::from_slice(&std::fs::read(output.join("matrix.json"))?)?;
        assert_eq!(matrix["paired_comparisons"][0]["eligible_pairs"], 1);
        assert_eq!(matrix["paired_comparisons"][0]["raw_deltas"], serde_json::json!([0.0]));
        assert!(!matrix["paired_comparisons"][0]["hoeffding_95_radius"].is_null());
        assert!(std::fs::read_to_string(output.join("report.html"))?.contains("transcript.txt"));
        assert!(runner::run(&dataset, &source, &output, &executable, &report::Judge::Scripted).await.is_err(), "never overwrite a captured run");
        eprintln!("FIG-763 fixture: 2/2 matrix cells accepted; 2 SIGKILL/restart recoveries; 4/4 inputs settled; 10/10 hard gates passed; 1 eligible pair; scripted quality only, zero real-model calls");
        Ok(())
    })
}
