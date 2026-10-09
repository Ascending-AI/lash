//! The workflow execution overlay: what folding one observation and one
//! settlement costs a host by document size, and what observing a program
//! costs the VM against the same program unobserved.
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use futures_util::StreamExt as _;
use lash::process::ProcessObservationStreamItem;
use lash::workflow::{
    WorkflowDocumentRead, WorkflowExecutionOverlayAccumulator, WorkflowOverlaySettlement,
    WorkflowOverlayTerminal,
};
use lash_core::{ProcessDocumentIdentity, ProcessObservationEventPayload, ProcessReadView};
use lash_vm::{
    AbilityOp, AbilityOutcome, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    LashVmExecutionObservation, State,
};

use super::super::{Args, Case, Meter, Receipt};
use super::{Flavor, Fleet, loop_source, publish, start, within, within_population};

pub(super) async fn run(args: &Args) -> Result<Receipt> {
    let meter = Meter::default();
    match args.case {
        Case::OverlayFold => Box::pin(fold(args, &meter)).await,
        Case::OverlayAttribution => attribution(args, &meter).await,
        other => anyhow::bail!("{other:?} is not an overlay workload"),
    }
}

fn sites(body: &lash::vm::ir::WorkflowSubgraph, nodes: &mut usize, count: &mut usize) {
    for node in body.nodes() {
        *nodes += 1;
        *count += node.execution_sites.len();
        if let lash::vm::ir::WorkflowNodeKind::Container(container) = &node.kind {
            for (_, child) in container.child_subgraphs() {
                sites(child, nodes, count);
            }
        }
    }
}

/// One process of `--callers` step sites runs `--operations` loop turns on a
/// served node. A host folds what its feed delivered into the execution
/// overlay: one fold and one snapshot per observation, then the settlement.
async fn fold(args: &Args, meter: &Meter) -> Result<Receipt> {
    let fleet = Fleet::open(args, meter, 1, Flavor::Workflow, |builder, _| Ok(builder)).await?;
    let result = Box::pin(async {
        let node = &fleet.nodes[0];
        let published = publish(&node.core, &loop_source(args.operations, args.callers)).await?;
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
        let (mut nodes, mut site_count) = (0, 0);
        let entry = document
            .entry_name()
            .and_then(|entry| document.graph().process(entry))
            .context("the document's entry process")?;
        sites(&entry.body, &mut nodes, &mut site_count);

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
                    meter.sample("overlay.fold.language", start.elapsed());
                }
                ProcessObservationEventPayload::StepBodyStarted(observation) => {
                    overlay.step_body_started(observation)?;
                    meter.sample("overlay.fold.step_body", start.elapsed());
                }
                ProcessObservationEventPayload::Committed { .. } => continue,
            }
            folded += 1;
            // A host reads the overlay after each observation it folds.
            let start = Instant::now();
            snapshots += usize::from(overlay.snapshot().is_some());
            meter.sample("overlay.snapshot", start.elapsed());
        }
        meter.window("overlay.fold.observations", folded, window);
        let start = Instant::now();
        overlay.settle(WorkflowOverlaySettlement {
            terminal: WorkflowOverlayTerminal::Completed,
            occurred_at: Some(chrono::Utc::now()),
        });
        let settled = overlay.snapshot().context("the settled overlay")?;
        meter.record("overlay.settlement", 1, start);
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

/// A VM host that answers every step at once and counts what it observes.
struct Host {
    observes: bool,
    observed: AtomicU64,
}

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            other => Err(ExecutionHostError::new(format!(
                "unexpected attribution ability: {other:?}"
            ))),
        }
    }
    fn observes_lash_vm_execution(&self) -> bool {
        self.observes
    }
    fn observe_lash_vm_execution(&self, _: LashVmExecutionObservation) {
        self.observed.fetch_add(1, Ordering::Relaxed);
    }
}

const ROUNDS: usize = 9;

/// One program of `--operations` loop turns and `--callers` branch sites per
/// turn runs in the VM unobserved and observed, `ROUNDS` times each. No store
/// takes part: the receipt isolates what building site-attributed
/// observations costs the execution.
async fn attribution(args: &Args, meter: &Meter) -> Result<Receipt> {
    let items = (0..args.operations)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut source = format!("let total = 0;\nfor (const item of [{items}]) {{\n");
    for _ in 0..args.callers {
        source.push_str("  if (item >= 0) { total = total + item; }\n");
    }
    source.push_str("}\nfinish(total);\n");
    let program = lash_typescript::parse_with_globals(&source, &BTreeSet::new())
        .map_err(|error| anyhow::anyhow!("parse TypeScript: {error}"))?;
    let spans = program.spans.clone();
    let artifact = lash_vm::ModuleArtifact::from_program(program)
        .map_err(|error| anyhow::anyhow!("create module artifact: {error}"))?;
    let compiled = lash_vm::compile(&artifact, lash_vm::Entry::Main, Some(&spans))
        .map_err(|error| anyhow::anyhow!("compile TypeScript: {error}"))?;

    let mut observations = 0;
    let mut medians = Vec::new();
    for (observes, boundary) in [
        (false, "vm.execute.unobserved"),
        (true, "vm.execute.observed"),
    ] {
        let mut elapsed = Vec::new();
        for _ in 0..ROUNDS {
            let host = Host {
                observes,
                observed: AtomicU64::new(0),
            };
            let start = Instant::now();
            let outcome = lash_vm::execute(&compiled, &mut State::new(), &host).await?;
            elapsed.push(start.elapsed());
            meter.sample(boundary, start.elapsed());
            ensure!(
                matches!(outcome, ExecutionOutcome::Finished(_)),
                "the attribution program did not finish: {outcome:?}"
            );
            if observes {
                observations = host.observed.load(Ordering::Relaxed);
            } else {
                ensure!(
                    host.observed.load(Ordering::Relaxed) == 0,
                    "an unobserving host was handed observations"
                );
            }
        }
        elapsed.sort();
        medians.push(elapsed[ROUNDS / 2]);
    }
    ensure!(observations > 0, "the observed run produced no observation");
    let overhead = medians[1].saturating_sub(medians[0]);
    Ok(Receipt::measured(
        args.case,
        "vm-execute",
        "none",
        args.operations,
        meter,
        serde_json::json!({
            "observations_per_run": observations,
            "rounds_per_mode": ROUNDS,
            "median_unobserved_us": medians[0].as_micros(),
            "median_observed_us": medians[1].as_micros(),
            "median_overhead_us": overhead.as_micros(),
            "overhead_ns_per_observation": overhead.as_nanos() as f64 / observations as f64,
        }),
        serde_json::json!({
            "loop_turns": args.operations,
            "branch_sites_per_turn": args.callers,
            "observations_per_run": observations,
        }),
    ))
}
