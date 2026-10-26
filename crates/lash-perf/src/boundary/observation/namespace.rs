//! The host's saved-workflow authoring lifecycle, distinct from execution tails.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use lash::package::*;
use lash::process::HostArtifactPin;
use lash_core::perf_witness::{Collector, Snapshot};
use serde::Serialize;

use super::super::{Args, Case, Meter, Receipt};
use super::{Flavor, Fleet, Published, environment, start, within_population};
use crate::perf_support::instructions::InstructionCounter;

#[derive(Serialize)]
struct Window {
    identity: String,
    temperature: &'static str,
    definition: &'static str,
    operation: &'static str,
    wall_us: u128,
    instructions: u64,
    sql_statements: u64,
    sql_rows_written: u64,
    sql_bytes_written: u64,
    store_pages_written: u64,
    sql_shapes: Vec<lash_core::perf_witness::sql_shapes::Row>,
    physical_writes: lash_sqlite_store::write_observer::Receipt,
}

pub(in crate::boundary) async fn run(
    args: &Args,
    instructions: &InstructionCounter,
) -> Result<Receipt> {
    ensure!(
        args.postgres_url.is_none(),
        "workflow-namespace uses SQLite file stores"
    );
    let meter = Meter::new(args.ledger_cap);
    let witness = Collector::install_with_sql_shapes()?;
    let writes =
        lash_sqlite_store::write_observer::Collector::install().map_err(anyhow::Error::msg)?;
    let mut windows = Vec::new();
    let mut definitions = Vec::new();
    for definition in ["unchanged", "changed"] {
        let mut population_args = args.clone();
        population_args.store_dir = args.store_dir.join(definition);
        std::fs::create_dir(&population_args.store_dir)?;
        let fleet =
            Fleet::open(&population_args, &meter, 1, Flavor::Workflow, |b, _| Ok(b)).await?;
        let result = async {
            let core = &fleet.nodes[0].core;
            for cycle in 0..=args.operations {
                let temperature = if cycle == 0 { "cold" } else { "warm" };
                let pin = HostArtifactPin::mint();
                let env = environment(false)?;
                let context = PublicationContext {
                    domain: PublicationDomain(core.backend().binding_identity().to_string()),
                    scope: PublicationScope::Host,
                    publisher: PublisherInstance("workflow-namespace-v1".into()),
                    root: Segment::new("workload")?,
                };
                let mut saved = None;
                let mut original = None;
                for operation in ["create", "replace", "start"] {
                    let identity = format!("{definition}/{temperature}/{cycle}/{operation}");
                    let before = idle_cut(&witness, &writes).await?;
                    lash_core::perf_witness::sql_shapes::work::set_window(Some(cycle), operation);
                    let instruction_start = instructions.read()?;
                    let begin = Instant::now();
                    let packages = core.host_artifacts().process_namespace(&pin, &env, context.clone()).await?;
                    let wall_us = begin.elapsed().as_micros();
                    let instruction_end = instructions.read()?;
                    meter.operation(&format!("process_namespace.{temperature}.{definition}.{operation}"), &identity, "ok", begin);
                    let after = idle_cut(&witness, &writes).await?;
                    windows.push(window(identity, temperature, definition, operation, wall_us,
                        instruction_end - instruction_start, before, after)?);
                    lash_core::perf_witness::sql_shapes::work::set_window(None, "workflow_lifecycle_outside_namespace");
                    let request = lash::vm::WorkflowEnvironmentRequest {
                        snapshot: packages.snapshot.clone(),
                        context: context.clone(),
                        attachment: Attachment {
                            members: Default::default(),
                            deferred: Default::default(),
                            packages: packages.snapshot.advertisements.iter().filter_map(|a| match &a.advertisement {
                                PackageAdvertisement::Enumerated(p) => Some(p.path.clone()),
                                _ => None,
                            }).collect(),
                        },
                    };
                    if operation != "start" {
                        let result = i32::from(definition == "changed" && operation == "replace");
                        let document = lash::workflow::document::DocumentBytes::from(
                            lash::workflow::document::parse_document(&format!("kernel 1\nnumbers float\nmain {{ return {result} }}\n"))?
                        );
                        let linked = core.host_artifacts().publish_workflow(&pin, &document,
                            &StartSelector::Main, &request, None, None).await?;
                        let draft = lash::vm::definition_draft(&lash::process::KernelProcessDefinition {
                            code: lash::vm::KernelInlineRequest::Linked { start: linked.clone() },
                        })?;
                        let published_definition = core.host_artifacts().publish_definition(&pin, &draft).await?;
                        if operation == "create" {
                            original = Some(published_definition.id.clone());
                        } else {
                            ensure!((original.as_ref() == Some(&published_definition.id)) == (definition == "unchanged"),
                                "replacement definition identity does not match the definition change");
                            definitions.push(serde_json::json!({"temperature":temperature,"definition":definition,
                                "cycle":cycle,"original":original,"replacement":published_definition.id,
                                "link":linked,"document":document.id()}));
                        }
                        // The host's saved-workflow slot is replaced by this immutable Link.
                        saved = Some((linked, published_definition));
                    } else {
                        let (linked, saved_definition) = saved.take().context("the host saved no replacement")?;
                        ensure!(core.host_artifacts().get_definition(&saved_definition.id).await? == Some(saved_definition),
                            "the saved replacement definition was not retained");
                        let linked_env = core.host_artifacts().link_process_env(&env,
                            &lash::process::LinkTarget::Linked(linked.clone()), &packages, &Default::default()).await?;
                        let published = Published { pin: pin.clone(), start: linked, environment: request,
                            env: linked_env, settings: core.processes().engine_defaults(
                                lash::process::LASH_VM_ENGINE_KIND, &serde_json::Value::Null)? };
                        let process = Box::pin(start(core, &published, &format!("namespace-{definition}-{cycle}"))).await?;
                        let output = within_population("saved workflow terminal", core.processes().await_output(&process)).await??;
                        let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output else {
                            anyhow::bail!("saved workflow did not settle: {output:?}");
                        };
                        ensure!(output.is_success(), "saved workflow failed: {output:?}");
                        let expected = i32::from(definition == "changed");
                        ensure!(output.value_for_projection() == serde_json::json!(expected),
                            "saved workflow ran the wrong replacement: {output:?}");
                    }
                }
                core.host_artifacts().release(pin).await?;
            }
            anyhow::Ok(())
        }.await;
        fleet.close().await?;
        result?;
    }
    println!(
        "Workflow namespace v1; SQLite files; wall time is noise-affected; instructions:u includes inherited descendants"
    );
    println!("window | wall_us | instructions | SQL | rows_written | VFS_writes | VFS_bytes");
    for w in &windows {
        println!(
            "{} | {} | {} | {} | {} | {} | {}",
            w.identity,
            w.wall_us,
            w.instructions,
            w.sql_statements,
            w.sql_rows_written,
            w.physical_writes.write_calls,
            w.physical_writes.total_bytes
        );
        println!("  shape | origin | statements");
        for row in &w.sql_shapes {
            println!(
                "  {} | {} | {}",
                row.shape, row.store_origin, row.counts.statements
            );
        }
    }
    Ok(Receipt::measured(
        Case::WorkflowNamespace,
        "host-artifacts-saved-workflow",
        "sqlite-file-product+memory-replay",
        windows.len(),
        &meter,
        serde_json::json!({"namespace_windows":windows}),
        serde_json::json!({"workload":"workflow-namespace-v1","definition_identity":definitions,
            "oracle":"published definition identity unchanged iff content unchanged; retained replacement starts successfully and returns replacement value",
            "cold":"first lifecycle on a fresh core and database per definition variant",
            "warm":"later lifecycles on the same core and database; fresh host pins",
            "sql_scope":"all SQL completed in each isolated namespace interval; callbacks are not writes",
            "instructions_scope":"host and inherited descendants created after counter opened; no per-process split",
            "wall_time":"noise-affected diagnostic, never a gate"}),
    ))
}

async fn idle_cut(
    witness: &Collector,
    writes: &lash_sqlite_store::write_observer::Collector,
) -> Result<(Snapshot, lash_sqlite_store::write_observer::Receipt)> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let (snapshot, physical) = writes.cut(|| witness.snapshot());
        if physical.active_operations == 0 {
            ensure!(physical.complete, "incomplete physical-write attribution");
            return Ok((snapshot, physical));
        }
        ensure!(
            Instant::now() < deadline,
            "namespace SQL did not reach an idle cut"
        );
        tokio::task::yield_now().await;
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "one named operation window and its two cuts"
)]
fn window(
    identity: String,
    temperature: &'static str,
    definition: &'static str,
    operation: &'static str,
    wall_us: u128,
    instructions: u64,
    before: (Snapshot, lash_sqlite_store::write_observer::Receipt),
    after: (Snapshot, lash_sqlite_store::write_observer::Receipt),
) -> Result<Window> {
    let old: BTreeMap<_, _> = before
        .0
        .sql_shapes
        .context("missing before SQL shapes")?
        .rows
        .into_iter()
        .map(|r| ((r.shape, r.store_origin), r.counts))
        .collect();
    let shapes = after.0.sql_shapes.context("missing after SQL shapes")?;
    ensure!(
        shapes.overflow.statements == 0,
        "SQL shape observer overflowed"
    );
    let sql_shapes: Vec<_> = shapes
        .rows
        .into_iter()
        .filter_map(|mut row| {
            let old = old
                .get(&(row.shape.clone(), row.store_origin.clone()))
                .cloned()
                .unwrap_or_default();
            row.counts.statements -= old.statements;
            row.counts.selects -= old.selects;
            row.counts.rows_returned -= old.rows_returned;
            (row.counts.statements > 0).then_some(row)
        })
        .collect();
    let sql_statements = after.0.sql_statements - before.0.sql_statements;
    ensure!(
        sql_shapes.iter().map(|r| r.counts.statements).sum::<u64>() == sql_statements,
        "namespace SQL shape totals do not conserve"
    );
    let physical_writes = after
        .1
        .difference(&before.1)
        .context("incomplete namespace physical-write interval")?;
    Ok(Window {
        identity,
        temperature,
        definition,
        operation,
        wall_us,
        instructions,
        sql_statements,
        sql_rows_written: after.0.sql_rows_written - before.0.sql_rows_written,
        sql_bytes_written: after.0.sql_bytes_written - before.0.sql_bytes_written,
        store_pages_written: after.0.store_pages_written - before.0.store_pages_written,
        sql_shapes,
        physical_writes,
    })
}
