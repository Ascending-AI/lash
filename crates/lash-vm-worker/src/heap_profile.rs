//! One finite window per exec: bootstrap validation through the first clean
//! reset. Flush before ResetDone, because pool retirement kills the worker.
use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Instant;

struct Window {
    profiler: dhat::Profiler,
    path: PathBuf,
    started: Instant,
}
thread_local! {
    static WINDOW: RefCell<Option<Window>> = const { RefCell::new(None) };
}

#[expect(
    clippy::disallowed_methods,
    reason = "opt-in worker instrumentation creates only the host-supplied profile directory"
)]
pub(super) fn start(args: &[String]) -> std::io::Result<()> {
    let Some(index) = args.iter().position(|arg| arg == "--heap-profile-dir") else {
        return Ok(());
    };
    let directory = args
        .get(index + 1)
        .ok_or_else(|| std::io::Error::other("--heap-profile-dir requires a directory"))?;
    std::fs::create_dir_all(directory)?;
    let path = PathBuf::from(directory).join(format!("vm-worker-{}.dhat.json", std::process::id()));
    let profiler = dhat::Profiler::builder()
        .file_name(&path)
        .trim_backtraces(Some(16))
        .build();
    WINDOW.with(|window| {
        *window.borrow_mut() = Some(Window {
            profiler,
            path,
            started: Instant::now(),
        })
    });
    Ok(())
}

#[expect(
    clippy::disallowed_methods,
    reason = "opt-in worker instrumentation writes its profile receipt at the host-supplied path"
)]
pub(super) fn finish() -> std::io::Result<()> {
    let Some(window) = WINDOW.with(|window| window.borrow_mut().take()) else {
        return Ok(());
    };
    let elapsed = window.started.elapsed().as_nanos();
    drop(window.profiler);
    std::fs::write(
        window.path.with_extension("receipt.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "kind": "lash.heap-profile", "role": "vm-worker", "process_id": std::process::id(),
            "window": "after_empty_environment_check_through_bootstrap_validation_and_first_clean_reset_before_reset_acknowledgement",
            "window_elapsed_ns": elapsed, "elapsed_statistic": "single_window_wall_duration",
            "allocator": "dhat-heap", "profile": window.path, "completed": true,
            "scope": "this_process_rust_allocator_in_profile_window", "unit": "bytes",
            "profile_quantities": {"tb": "cumulative_requested_bytes", "mb": "site_max_live_bytes",
                "gb": "site_live_bytes_at_process_heap_peak", "eb": "site_live_bytes_at_window_end"},
            "certifying": false,
        }))?,
    )
}
