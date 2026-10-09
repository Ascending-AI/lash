#[path = "support/runtime.rs"]
mod runtime;

use runtime::run_workflow;
use serde_json::json;
use workflow_graph_roundtrip::{RunStatus, WorkflowDocument};

#[tokio::test]
async fn an_opened_workflow_with_an_inline_process_runs_its_selected_entry_after_edits() {
    use lash::vm::testing::ast_builders as b;
    use lash::workflow::{WorkflowDraft, WorkflowEntry, WorkflowPublish};

    let (state, core) = runtime::state_and_core().await;
    let graph = lash::vm::ir::workflow_graph_from_program(&b::module(
        Vec::new(),
        vec![b::assign(
            "workflow",
            b::process_literal(
                Vec::new(),
                b::block(vec![
                    b::assign(
                        "inline",
                        b::process_literal(Vec::new(), b::finish(b::string("inline"))),
                    ),
                    b::module_call(
                        &["display"],
                        "set_progress",
                        vec![b::record(vec![("pct", b::num(100.0))])],
                    ),
                    b::finish(b::string("workflow")),
                ]),
            ),
        )],
    ));
    let draft = WorkflowDraft::open(&graph).expect("draft");
    let entry = draft.document().declarations.iter().find_map(|declaration| match declaration {
        lash::vm::ir::WorkflowDeclaration::Process(process) if matches!(&process.origin,
            lash::vm::ir::ProcessOrigin::Lifted { site, .. } if site.root == lash::vm::ir::AstRoot::Main) => Some(process.id.clone()),
        _ => None,
    }).expect("the workflow bound in main");
    let pin = lash::process::HostArtifactPin::mint();
    let environment = lash::process::ProcessExecutionEnvSpec::new(
        lash::plugins::AdmittedPluginConfig::default(),
        lash::runtime::SessionPolicy::new(
            lash::TurnBudget::bounded(32),
            lash::MaxToolCalls::new(1024),
            lash::NoProgressBudget::bounded(12),
        ),
        lash::plugins::SessionToolAccess::ambient(),
    );
    let WorkflowPublish::Published(publication) = core
        .host_artifacts()
        .publish_workflow(&pin, &draft, WorkflowEntry::Process(entry), &environment)
        .await
        .expect("admit document")
    else {
        panic!("the workflow admits");
    };
    let graph = publication.document.graph;
    assert!(
        matches!(&graph.declarations[0], lash::vm::ir::WorkflowDeclaration::Process(process)
        if process.origin.is_lifted()),
        "the inline process precedes the workflow"
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let server = tokio::spawn(workflow_graph_roundtrip::serve(listener, state));
    let client = reqwest::Client::new();
    let opened: WorkflowDocument = client
        .post(format!("{base}/workflow/ir"))
        .json(&json!({"graph": graph}))
        .send()
        .await
        .expect("open")
        .json()
        .await
        .expect("opened document");
    assert!(opened.not_admitted.is_none(), "{opened:?}");
    let edited: workflow_graph_roundtrip::SaveWorkflowResponse = client
        .post(format!("{base}/workflow/edits"))
        .json(&json!({"version": opened.version, "edits": [{
            "op": "insertProcess", "name": "other"
        }]}))
        .send()
        .await
        .expect("edit")
        .json()
        .await
        .expect("edited document");
    assert!(
        edited.document.not_admitted.is_none(),
        "{:?}",
        edited.document
    );
    let events = run_workflow(&client, &base).await;
    let terminal = events.last().expect("terminal overlay");
    assert_eq!(terminal.status, RunStatus::Succeeded);
    assert_eq!(terminal.display.progress, 100.0, "the workflow body ran");
    let process_id = terminal.run_id.parse().expect("process id");
    let lash::workflow::WorkflowRead::Inspected(inspection) = core
        .processes()
        .graph(&process_id)
        .await
        .expect("inspection")
    else {
        panic!("retained workflow");
    };
    assert!(
        matches!(&inspection.document.graph.process(&inspection.document.entry)
        .expect("entry process").origin, lash::vm::ir::ProcessOrigin::Lifted { site, .. }
        if site.root == lash::vm::ir::AstRoot::Main),
        "the selected top-level workflow ran"
    );
    server.abort();
    core.host_artifacts()
        .release(pin)
        .await
        .expect("release fixture pin");
}

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
    assert_eq!(process.status(), lash::process::ProcessStatus::Completed);
    assert!(
        process.env_ref.is_some(),
        "execution uses the published environment"
    );
    // The run's graph is lash's read of the process itself: the host kept no
    // module to draw it from.
    let lash::workflow::WorkflowRead::Inspected(inspection) = core
        .processes()
        .graph(&process_id)
        .await
        .expect("the process's workflow reads")
    else {
        panic!("a retained lash_vm process has a workflow");
    };
    assert_eq!(
        Some(&inspection.definition.id),
        process.identity.definition_id.as_ref(),
        "the read names the definition the process runs"
    );
    let graph = &inspection.document.graph;
    let process_name = inspection.document.entry.as_str();
    assert!(
        events
            .iter()
            .all(|event| Some(&event.definition) == graph.source_identity.as_ref())
    );
    let saved_root = graph.process(process_name).expect("saved root");
    fn executing<'g>(
        body: &'g lash::vm::ir::WorkflowSubgraph,
        ids: &mut std::collections::BTreeSet<&'g str>,
    ) {
        for node in &body.nodes {
            if !node.execution_sites.is_empty() {
                ids.insert(node.id.as_str());
            }
            if let lash::vm::ir::WorkflowNodeKind::Container(container) = &node.kind {
                for (_, child) in container.child_subgraphs() {
                    executing(child, ids);
                }
            }
        }
    }
    let mut node_ids = std::collections::BTreeSet::new();
    executing(&saved_root.body, &mut node_ids);
    let root_id = saved_root.id.to_string();
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
            lash::process::ProcessHistoryContinuation::start(process_id),
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
