//! FIG-6821: a cancel that cuts a parked sleep short ends that wait as
//! cancelled, never as resumed, so the sleep's activity reads cancelled. A
//! sleep that elapsed stays completed, whether the run then finishes or is
//! cancelled later.

use super::failures::Module;
use lash::workflow::ProcessWorkflowSiteState;
use lash::workflow::document::SourceLanguage;
use lash_vocabulary::schema_contract::*;

/// `nap()` sleeps a minute; `naps()` sleeps a minute, then ten.
const TYPESCRIPT: &str = "export async function nap() {\n  await sleep(60000);\n  return \"woke\";\n}\n\nexport async function naps() {\n  await sleep(60000);\n  await sleep(600000);\n  return \"woke\";\n}\n";
const PYTHON: &str = "import asyncio\n\n\nasync def nap():\n    await asyncio.sleep(60)\n    return \"woke\"\n\n\nasync def naps():\n    await asyncio.sleep(60)\n    await asyncio.sleep(600)\n    return \"woke\"\n";

const DIALECTS: [(SourceLanguage, &str); 2] = [
    (SourceLanguage::TypeScript, TYPESCRIPT),
    (SourceLanguage::Python, PYTHON),
];

/// The committed state of each sleep `process` entered, in the order it
/// entered them, read from its observation snapshot.
async fn sleeps(
    core: &lash::LashCore,
    process: &lash_vocabulary::identity::ProcessId,
) -> Vec<ProcessWorkflowSiteState> {
    let snapshot = core
        .processes()
        .observe(process)
        .snapshot()
        .await
        .expect("the snapshot reads");
    let lash::process::ProcessReadView::Retained(view) = snapshot.read_view else {
        panic!("the process is retained");
    };
    let mut rows: Vec<_> = view
        .workflow
        .expect("a workflow process has an overlay")
        .sites
        .into_iter()
        .filter(|row| row.first_entered.is_some())
        .collect();
    rows.sort_by_key(|row| row.first_entered);
    rows.into_iter().map(|row| row.state).collect()
}

/// The stored kinds of `process`'s wait-ending events, in order.
async fn wait_ends(
    core: &lash::LashCore,
    process: &lash_vocabulary::identity::ProcessId,
) -> Vec<&'static str> {
    let page = core
        .processes()
        .events(
            lash::process::ProcessHistoryContinuation::start(process.clone()),
            std::num::NonZeroUsize::new(100).expect("limit"),
            lash::process::ProcessEventQueryMode::Full,
        )
        .await
        .expect("stored events");
    let lash::process::ProcessEventReadOutcome::Retained(page) = page.outcome else {
        panic!("retained events");
    };
    let lash::process::ProcessEventPageEvents::Full(events) = page.events else {
        panic!("full events");
    };
    events
        .iter()
        .map(|event| event.fact.event_type())
        .filter(|kind| matches!(*kind, "process.resumed" | "process.wait_cancelled"))
        .collect()
}

/// A sleep that elapses settles completed, and the run finishes.
#[tokio::test]
async fn a_sleep_that_elapses_reports_completed() {
    let files = Files::new();
    let core = core_over(files.open().await, "elapsed-sleep").await;
    for (language, source) in DIALECTS {
        let module = Module::import(&core, language, source).await;
        let (_, process) = module.start(&core, "nap", &[], serde_json::json!({})).await;
        let due = timer_park_after(&core, &process, 0).await;
        files.clock.set(due);
        assert_eq!(
            finished(&core, &process).await,
            serde_json::json!("woke"),
            "{language:?}"
        );
        assert_eq!(
            sleeps(&core, &process).await,
            [ProcessWorkflowSiteState::Completed],
            "{language:?}"
        );
        assert_eq!(
            wait_ends(&core, &process).await,
            ["process.resumed"],
            "{language:?}"
        );
    }
    core.shutdown().await.expect("shutdown");
}

/// A run cancelled while parked on its second sleep ends that wait as
/// cancelled: its sleep reads cancelled. Its first sleep elapsed before the
/// cancel and stays completed. Fails while the cancel ends the parked sleep
/// as `process.resumed`, which reads completed.
#[tokio::test]
async fn a_cancel_during_a_sleep_reports_it_cancelled_and_leaves_an_elapsed_one_completed() {
    let files = Files::new();
    let core = core_over(files.open().await, "cancelled-sleep").await;
    for (language, source) in DIALECTS {
        let module = Module::import(&core, language, source).await;
        let (_, process) = module
            .start(&core, "naps", &[], serde_json::json!({}))
            .await;
        let first = timer_park_after(&core, &process, 0).await;
        files.clock.set(first);
        timer_park_after(&core, &process, first).await;
        core.processes()
            .cancel(&process, core.effect_host())
            .await
            .expect("the cancel is requested");
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            core.processes().await_output(&process),
        )
        .await
        .expect("the process settles")
        .expect("the output reads");
        assert!(
            matches!(
                output,
                lash_execution_contracts::process::ProcessAwaitOutput::Settled { .. }
            ),
            "{language:?}: {output:?}"
        );
        assert_eq!(
            sleeps(&core, &process).await,
            [
                ProcessWorkflowSiteState::Completed,
                ProcessWorkflowSiteState::Cancelled
            ],
            "{language:?}"
        );
        assert_eq!(
            wait_ends(&core, &process).await,
            ["process.resumed", "process.wait_cancelled"],
            "{language:?}"
        );
    }
    core.shutdown().await.expect("shutdown");
}
