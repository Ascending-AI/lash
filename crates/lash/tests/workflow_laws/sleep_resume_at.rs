//! FIG-6826: a waiting sleep's occurrence carries `resume_at_ms`, the
//! instant its recorded park resumes at, so a host can say when a sleep with
//! a computed duration wakes. It is read from the park, not recomputed, so a
//! reload from the store reads the same instant.

use super::failures::Module;
use lash::workflow::ProcessWorkflowSiteState;
use lash::workflow::ProcessWorkflowWaitKind;
use lash::workflow::document::SourceLanguage;
use lash_vocabulary::schema_contract::*;

/// `nap(minutes)` sleeps the duration its argument computes.
const TYPESCRIPT: &str = "export async function nap(minutes: number) {\n  await sleep(minutes * 60000);\n  return \"woke\";\n}\n";
const PYTHON: &str = "import asyncio\n\n\nasync def nap(minutes):\n    await asyncio.sleep(minutes * 60)\n    return \"woke\"\n";

const DIALECTS: [(SourceLanguage, &str); 2] = [
    (SourceLanguage::TypeScript, TYPESCRIPT),
    (SourceLanguage::Python, PYTHON),
];

/// The instant the runs park at: the store clock, held still.
const PARKED_AT: u64 = 1_700_000_123_456;
/// `nap(7)` sleeps seven minutes.
const MINUTES: i64 = 7;
const RESUME_AT: i64 = PARKED_AT as i64 + MINUTES * 60_000;

/// The state of the one sleep `process` committed an entry for, from its
/// observation snapshot.
async fn sleep_state(
    core: &lash::LashCore,
    process: &lash_vocabulary::identity::ProcessId,
) -> ProcessWorkflowSiteState {
    let snapshot = core
        .processes()
        .observe(process)
        .snapshot()
        .await
        .expect("the snapshot reads");
    let lash::process::ProcessReadView::Retained(view) = snapshot.read_view else {
        panic!("the process is retained");
    };
    let rows = view
        .workflow
        .expect("a workflow process has an overlay")
        .sites;
    let entered: Vec<_> = rows
        .iter()
        .filter(|row| row.first_entered.is_some())
        .collect();
    let [row] = entered.as_slice() else {
        panic!("one entered sleep: {rows:?}");
    };
    row.state.clone()
}

/// Starts `nap(7)` in each dialect and waits for each run to park on its
/// sleep.
async fn parked_naps(
    core: &lash::LashCore,
    files: &Files,
) -> Vec<(SourceLanguage, lash_vocabulary::identity::ProcessId)> {
    files.clock.set(PARKED_AT);
    let mut parked = Vec::new();
    for (language, source) in DIALECTS {
        let module = Module::import(core, language, source).await;
        let (_, process) = module
            .start(core, "nap", &[], serde_json::json!({ "minutes": MINUTES }))
            .await;
        timer_park_after(core, &process, 0).await;
        parked.push((language, process));
    }
    parked
}

fn waiting_until(resume_at_ms: i64) -> ProcessWorkflowSiteState {
    ProcessWorkflowSiteState::Waiting {
        kind: ProcessWorkflowWaitKind::Sleep,
        resume_at_ms: Some(resume_at_ms),
    }
}

/// A sleep whose duration the run computes reports, while it waits, the
/// instant it parked at plus that duration.
#[tokio::test]
async fn a_sleep_with_a_computed_duration_reports_when_it_resumes() {
    let files = Files::new();
    let core = core_over(files.open().await, "resume-at").await;
    for (language, process) in parked_naps(&core, &files).await {
        assert_eq!(
            sleep_state(&core, &process).await,
            waiting_until(RESUME_AT),
            "{language:?}"
        );
    }
    core.shutdown().await.expect("shutdown");
}

/// A node that reopens the store reads the same resume instant for a run
/// another node parked, from the stored park alone.
#[tokio::test]
async fn a_sleeps_resume_instant_survives_a_reload_from_the_store() {
    let files = Files::new();
    let core = core_over(files.open().await, "resume-at-first").await;
    let parked = parked_naps(&core, &files).await;
    core.shutdown().await.expect("shutdown");
    let core = core_over(files.open().await, "resume-at-second").await;
    for (language, process) in parked {
        assert_eq!(
            sleep_state(&core, &process).await,
            waiting_until(RESUME_AT),
            "{language:?}"
        );
    }
    core.shutdown().await.expect("shutdown");
}
