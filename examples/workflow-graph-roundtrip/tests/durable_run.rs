#[path = "support/runtime.rs"]
mod runtime;

use runtime::run_workflow;
use serde_json::json;
use workflow_graph_roundtrip::{RunStatus, WorkflowDocument};

#[tokio::test]
async fn a_saved_workflow_runs_as_a_durable_process() {
    let (state, core) = runtime::state_and_core().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let server = tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
    let client = reqwest::Client::new();
    let document: WorkflowDocument = client
        .post(format!("{base}/workflow/select"))
        .json(&json!({"id": "counter-loop"}))
        .send()
        .await
        .expect("select")
        .json()
        .await
        .expect("saved document");
    let events = run_workflow(&client, &base).await;
    assert!(!events.is_empty());
    let process_id = events[0]
        .run_id
        .parse::<lash::ProcessId>()
        .expect("the run overlay names the durable process that executes the saved version");
    assert!(
        events
            .iter()
            .all(|event| event.run_id == process_id.as_str()
                && event.workflow_version == document.version)
    );
    let process = core
        .processes()
        .get(&process_id)
        .await
        .expect("durable process read")
        .expect("retained process");
    assert_eq!(process.lifecycle, lash::process::ProcessStatus::Completed);
    assert!(
        process.env_ref.is_some(),
        "execution uses the published environment"
    );
    let graph = lash::typescript::workflow_graph::workflow_graph_from_source(&document.source)
        .expect("saved graph");
    let process_name = document
        .nodes
        .iter()
        .find_map(|node| node.data.process_name().as_deref())
        .expect("saved process name");
    let map = lash::process::trace_lashlang_process_map(&graph, process_name)
        .expect("saved execution map");
    let mut node_ids = map
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let root_id = graph
        .process(process_name)
        .expect("saved root")
        .id
        .to_string();
    assert!(document.roots.processes.contains(&root_id));
    node_ids.insert(root_id.as_str());
    assert!(
        events
            .iter()
            .all(|event| node_ids.contains(event.node_id.as_str()))
    );
    let last = events.last().expect("terminal overlay");
    assert_eq!(last.status, RunStatus::Succeeded);
    assert_eq!(last.display.progress, 100.0);
    assert_eq!(
        last.display.lists.get("counts").expect("loop items").len(),
        3
    );
    let page = core
        .processes()
        .events(
            lash::process::ProcessEventsFrom::Start(process_id),
            std::num::NonZeroUsize::new(128).expect("nonzero page"),
            lash::process::ProcessEventQueryMode::Full,
        )
        .await
        .expect("durable events");
    let lash::process::ProcessEventReadOutcome::Retained(page) = page.outcome else {
        panic!("event history retained");
    };
    let lash::process::ProcessEventPageEvents::Full(durable) = page.events else {
        panic!("full events requested");
    };
    let outcomes = durable
        .iter()
        .filter_map(|event| match &event.fact {
            lash::process::ProcessLifecycleFact::EffectOutcome(occurrence) => Some(occurrence),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(!outcomes.is_empty(), "the engine recorded graph effects");
    assert!(outcomes.iter().all(|outcome| {
        events
            .iter()
            .any(|event| event.node_id == outcome.node_id && event.status == RunStatus::Succeeded)
    }));
    assert!(durable.iter().any(|event| matches!(&event.fact,
        lash::process::ProcessLifecycleFact::Terminal { outcome, .. }
            if outcome.status() == lash::process::TerminalProcessStatus::Completed)));
    server.abort();
}
