//! Retention series on one real node, without a smoke-run growth threshold.
use super::{Args, Case, Meter, Receipt, facade};
use anyhow::{Result, ensure};
use lash_sansio::sync::MutexExt as _;
use std::io::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn sample(start: Instant, wave: usize, completed: usize, node_alive: bool) -> serde_json::Value {
    let memory = crate::perf_support::memory::process_memory_sample();
    let alloc = crate::GLOBAL_ALLOCATOR.stats();
    let heap = crate::perf_support::dhat::profiled_heap_sample();
    serde_json::json!({
        "role": "host+durable-node", "process_id": std::process::id(),
        "wave_index": wave, "completed_turns": completed, "node_alive": node_alive,
        "elapsed_ns": start.elapsed().as_nanos(),
        "elapsed_statistic": "monotonic_duration_since_population_start",
        "rust_live_requested_bytes": alloc.bytes_allocated as i128 - alloc.bytes_deallocated as i128,
        "rust_live_statistic": "process_allocator_requested_bytes_outstanding_at_endpoint",
        "profile_window_live_bytes": heap.map(|(live, _)| live),
        "profile_window_live_statistic": "dhat_window_outstanding_bytes_at_endpoint",
        "profile_window_high_water_bytes": heap.map(|(_, peak)| peak),
        "profile_window_high_water_statistic": "dhat_window_simultaneous_live_bytes_maximum",
        "rust_allocated_bytes": alloc.bytes_allocated, "rust_allocation_count": alloc.allocations,
        "allocation_statistic": "process_lifetime_cumulative",
        "rss_kib": memory.rss_kb, "rss_statistic": "process_resident_endpoint",
        "rss_high_water_kib": memory.hwm_kb, "rss_high_water_statistic": "process_lifetime_maximum",
    })
}

pub(super) async fn run(args: &Args) -> Result<Receipt> {
    let meter = Meter::default();
    let start = Instant::now();
    let stores = lash_sqlite_store::SqliteStoreSet::open(
        args.store_dir.join("lash.db"),
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await?;
    let core = facade::build(
        facade::backend(Arc::new(stores))?,
        "persistent-wave-node",
        true,
        true,
        facade::provider(&meter, false),
    )?;
    let session = facade::create(&core, "persistent-wave-session").await?;
    let series_path = args.out.with_extension("waves.jsonl");
    let mut series = std::io::BufWriter::new(std::fs::File::create(&series_path)?);
    let mut write_sample = |wave, completed, alive| -> Result<()> {
        serde_json::to_writer(&mut series, &sample(start, wave, completed, alive))?;
        writeln!(&mut series)?;
        series.flush()?;
        Ok(())
    };
    write_sample(0, 0, true)?;
    let result = async {
        for wave in 1..=args.operations {
            for turn in 0..args.callers {
                facade::send(&session, &format!("wave-{wave}-turn-{turn}"), &meter).await?;
            }
            // Submission is paused. Read authoritative settlement; this is not
            // a claim that background node maintenance or allocator work stops.
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    if core.drain_status(false).await?.drained() {
                        return anyhow::Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await??;
            ensure!(
                meter.count("send.settle") == args.callers,
                "wave did not settle every send"
            );
            // Bounded observer retention: do not keep all prior samples or
            // per-turn phases alive and mistake harness growth for node growth.
            meter.phases.lock_recover().clear();
            write_sample(wave, wave * args.callers, true)?;
        }
        anyhow::Ok(())
    }
    .await;
    drop(session);
    core.shutdown().await?;
    drop(core);
    result?;
    write_sample(args.operations, args.operations * args.callers, false)?;
    meter.record("waves.settled", args.operations * args.callers, start);
    Ok(Receipt::new(
        Case::PersistentNodeWaves,
        "facade-send",
        "sqlite-file-product",
        args.operations * args.callers,
        &meter,
        serde_json::json!({
            "waves": args.operations, "turns_per_wave": args.callers,
            "node_boots": 1, "node_shutdowns": 1, "growth_gate": null,
            "allocator": crate::ALLOCATION_MODE,
            "heap_scope": "process_rust_allocator_including_harness_and_background_work",
            "quiescence": "submission_paused_and_authoritative_drain_status_drained",
            "samples_jsonl": series_path, "sample_count": args.operations + 2,
        }),
    ))
}
