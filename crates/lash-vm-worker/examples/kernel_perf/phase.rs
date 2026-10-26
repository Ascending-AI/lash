#![expect(
    clippy::expect_used,
    reason = "diagnostic serialization of admitted results and paths"
)]
//! Run-only, acknowledged instruction windows for FIG-6622. Preparation,
//! machine start/drop and validation stay outside the window. The zero-work
//! control executes the same window and loop without driving a machine.
use super::*;
use std::io::{BufRead as _, Write as _};

#[expect(
    clippy::disallowed_methods,
    reason = "explicit profiling process environment"
)]
fn control(command: &str) -> Result<bool, String> {
    let Some(path) = std::env::var_os("LASH_PERF_CONTROL") else {
        return Ok(false);
    };
    let ack = std::env::var_os("LASH_PERF_ACK").ok_or("missing LASH_PERF_ACK")?;
    let mut writer = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    let reader = std::fs::File::open(ack).map_err(|error| error.to_string())?;
    writeln!(writer, "{command}").map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())?;
    let mut line = String::new();
    std::io::BufReader::new(reader)
        .read_line(&mut line)
        .map_err(|error| error.to_string())?;
    if line.trim() != "ack" {
        return Err(format!("unacknowledged perf control: {line:?}"));
    }
    Ok(true)
}

pub(super) fn phase_counts(counts: lash_kernel_vm::perf_witness::Work) -> Json {
    let mut heap = serde_json::to_value(counts.heap).expect("fixed heap counters JSON");
    if counts.heap.completed_refinements == 0
        && let Some(fields) = heap.as_object_mut()
    {
        for field in [
            "trace_bitmap_peak_bytes",
            "trace_work_list_peak_bytes",
            "trace_budget_peak_bytes",
        ] {
            fields.remove(field);
        }
    }
    json!({
        "vm_steps": counts.vm_steps,
        "root_publishes": counts.root_publishes,
        "task_resolutions": counts.task_resolutions,
        "frame_resolutions": counts.frame_resolutions,
        "register_slots_initialized": counts.register_slots_initialized,
        "register_slots_cleared": counts.register_slots_cleared,
        "heap": heap,
    })
}

#[expect(
    clippy::disallowed_methods,
    reason = "producer experiment identity from maintained caller"
)]
pub(super) fn identity(
    name: &str,
    source: &str,
    artifact: &str,
    result: &Datum,
    charge: u64,
    path: &lash_kernel_value::observation::ExecutionPath,
) -> Json {
    let mut result = serde_json::to_value(result).expect("Datum JSON");
    result.sort_all_objects();
    json!({
        "experiment_id": std::env::var("LASH_PERF_EXPERIMENT_ID")
            .unwrap_or_else(|_| format!("standalone-{}", std::process::id())),
        "host_id": std::env::var("LASH_PERF_HOST_ID").unwrap_or_else(|_| "unqualified-standalone".into()),
        "program": name,
        "workload_sha256": format!("{:x}", Sha256::digest(source.as_bytes())),
        "artifact_sha256": artifact,
        "result_sha256": format!("{:x}", Sha256::digest(serde_json::to_vec(&result).expect("Datum JSON"))),
        "statement_path_sha256": format!("{:x}", Sha256::digest(serde_json::to_vec(path).expect("statement path JSON"))),
        "charged": charge,
        "backend": "kernel-page-fixture-memory",
        "worker": "in-process-kernel; no worker process",
        "window": "run-to-terminal; start/drop/validation excluded; scripted host included",
        "terminal": "finished",
        "compiler": lash_dialect_typescript::BUILD_IDENTITY.0,
        "build_profile": lash_dialect_typescript::BUILD_IDENTITY.1,
        "allocator": "system+stats_alloc",
    })
}

#[expect(
    clippy::disallowed_methods,
    reason = "explicit profiling process environment"
)]
pub(super) fn measure(
    name: &str,
    program: &Program,
    source: &str,
    artifact: &str,
) -> Result<Json, String> {
    let iterations: u32 = std::env::var("KERNEL_PHASE_ITERATIONS")
        .unwrap_or_else(|_| "100".into())
        .parse()
        .map_err(|error| format!("iterations: {error}"))?;
    if iterations == 0 {
        return Err("iterations must be positive".into());
    }
    let observer = std::env::var("KERNEL_PHASE_OBSERVER").unwrap_or_else(|_| "off".into());
    if !matches!(observer.as_str(), "off" | "on" | "zero") {
        return Err("KERNEL_PHASE_OBSERVER must be off, on or zero".into());
    }
    let plain = to_end(&mut start(program), name);
    let mut machines: Vec<_> = (0..iterations).map(|_| start(program)).collect();
    let mut results = Vec::with_capacity(iterations as usize);
    lash_kernel_vm::perf_witness::disable_work();
    if observer == "on" {
        lash_kernel_vm::perf_witness::enable_work();
    }
    let enabled_ack = control("enable")?;
    for machine in &mut machines {
        if observer == "zero" {
            std::hint::black_box(machine);
        } else {
            results.push(to_end(machine, name));
        }
    }
    let disabled_ack = control("disable")?;
    let counts = lash_kernel_vm::perf_witness::take_work().map(phase_counts);
    lash_kernel_vm::perf_witness::disable_work();
    for result in &results {
        assert_eq!(result, &plain, "{name}: instruction-window result/charge");
    }
    // Capture the statement path in a separate pass; never charge trace
    // recording to the fixed-counter observer. Validate its useful result too.
    let mut reference = start(program);
    reference.set_options(lash_kernel_vm::MachineOptions {
        trace_path: true,
        ..Default::default()
    });
    assert_eq!(to_end(&mut reference, name), plain);
    let path = reference.take_path().map_err(|error| error.to_string())?;
    Ok(json!({
        "identity": identity(name, source, artifact, &plain.1, plain.0, &path),
        "observer": observer,
        "pid": std::process::id(),
        "iterations": iterations,
        "completed_runs": results.len(),
        "acknowledged_windows": u32::from(enabled_ack && disabled_ack),
        "phase_counts_total": counts,
    }))
}
