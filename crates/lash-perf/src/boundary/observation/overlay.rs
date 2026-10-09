//! The workflow execution overlay: what folding one observation and one
//! settlement costs a host by document size.
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use futures_util::StreamExt as _;
use lash::process::ProcessObservationStreamItem;
use lash::workflow::{
    WorkflowDocumentRead, WorkflowExecutionOverlayAccumulator, WorkflowOverlaySettlement,
    WorkflowOverlayTerminal,
};
use lash_core::{ProcessDocumentIdentity, ProcessObservationEventPayload, ProcessReadView};

use super::super::{Args, Case, Meter, Receipt};
use super::{Flavor, Fleet, loop_document, publish, start, within, within_population};

pub(super) async fn run(args: &Args) -> Result<Receipt> {
    let meter = Meter::new(args.ledger_cap);
    match args.case {
        Case::OverlayFold => Box::pin(fold(args, &meter)).await,
        other => anyhow::bail!("{other:?} is not an overlay workload"),
    }
}

/// One process of `--callers` step sites runs `--operations` loop turns on a
/// served node. A host folds what its feed delivered into the execution
/// overlay: one fold and one snapshot per observation, then the settlement.
async fn fold(args: &Args, meter: &Meter) -> Result<Receipt> {
    let fleet = Fleet::open(args, meter, 1, Flavor::Workflow, |builder, _| Ok(builder)).await?;
    let result = Box::pin(async {
        let node = &fleet.nodes[0];
        let published = publish(&node.core, &loop_document(args.operations, args.callers)).await?;
        let process = start(&node.core, &published, "overlay").await?;
        let observed = node.core.processes().observe(&process);
        let snapshot = within("process snapshot", observed.snapshot()).await??;
        let ProcessReadView::Retained(view) = &snapshot.read_view else {
            anyhow::bail!("the process is not retained");
        };
        let ProcessDocumentIdentity::Available(reference) = &view.document else {
            anyhow::bail!("the process names no workflow document: {:?}", view.document);
        };
        let document = match node.core.host_artifacts().execution_document(reference).await? {
            WorkflowDocumentRead::Read(document) => *document,
            other => anyhow::bail!("the workflow document cannot be read: {other:?}"),
        };
        let (nodes, site_count) = (
            document.graph().nodes().len(),
            document.graph().execution_sites().len(),
        );

        let mut feed = observed.subscribe_and_recover(snapshot.cursor.clone());
        let mut observations = Vec::new();
        let mut gaps = 0;
        loop {
            let item = within_population("process feed item", feed.next())
                .await?
                .context("the process feed ended before its terminal")??;
            match item {
                ProcessObservationStreamItem::Event(event) => {
                    let terminal = matches!(&event.payload,
                        ProcessObservationEventPayload::Committed { event }
                            if matches!(event.fact, lash::process::ProcessLifecycleFact::Terminal { .. }));
                    observations.push(event);
                    if terminal {
                        break;
                    }
                }
                ProcessObservationStreamItem::Gap { .. } => gaps += 1,
            }
        }
        ensure!(gaps == 0, "the overlay's feed gapped {gaps} times");

        let mut overlay = WorkflowExecutionOverlayAccumulator::default();
        overlay.set_document(document.overlay_document());
        let window = Instant::now();
        let (mut folded, mut snapshots) = (0, 0);
        for event in &observations {
            let start = Instant::now();
            match &event.payload {
                ProcessObservationEventPayload::LanguageExecution(observation) => {
                    overlay.observe(observation)?;
                    meter.operation("overlay.fold.language", format!("process:{process}/event:{}", event.cursor.as_str()), "ok", start);
                }
                ProcessObservationEventPayload::StepBodyStarted(observation) => {
                    overlay.step_body_started(observation)?;
                    meter.operation("overlay.fold.step_body", format!("process:{process}/event:{}", event.cursor.as_str()), "ok", start);
                }
                ProcessObservationEventPayload::Committed { .. } => continue,
            }
            folded += 1;
            // A host reads the overlay after each observation it folds.
            let start = Instant::now();
            snapshots += usize::from(overlay.snapshot().is_some());
            meter.operation("overlay.snapshot", format!("process:{process}/event:{}", event.cursor.as_str()), "ok", start);
        }
        meter.window("overlay.fold.observations", folded, window);
        let start = Instant::now();
        overlay.settle(WorkflowOverlaySettlement {
            terminal: WorkflowOverlayTerminal::Completed,
            occurred_at: Some(chrono::Utc::now()),
        });
        let settled = overlay.snapshot().context("the settled overlay")?;
        meter.operation("overlay.settlement", &process, "ok", start);
        ensure!(folded > 0 && snapshots == folded, "the overlay folded nothing");
        anyhow::Ok((
            serde_json::json!({
                "document_nodes": nodes,
                "document_sites": site_count,
                "observations_folded": folded,
                "overlay_snapshots": snapshots,
                "overlay_sites": settled.sites.len(),
                "settlements": 1,
            }),
            serde_json::json!({
                "loop_turns": args.operations,
                "step_sites": args.callers,
                "observations_folded": folded,
            }),
        ))
    })
    .await;
    let store = fleet.store;
    fleet.close().await?;
    let (counters, evidence) = result?;
    Ok(Receipt::measured(
        args.case,
        "facade-workflow-process+execution-overlay",
        store,
        args.operations,
        meter,
        counters,
        evidence,
    ))
}
