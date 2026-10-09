use std::path::{Path, PathBuf};

use super::paths::default_dhat_output_path;
use super::report::ensure_parent_dir;

#[cfg(feature = "dhat-heap")]
static PROFILING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Exact DHAT-window live and high-water bytes; absent when profiling is off.
pub fn profiled_heap_sample() -> Option<(usize, usize)> {
    #[cfg(feature = "dhat-heap")]
    if PROFILING.load(std::sync::atomic::Ordering::Acquire) {
        let stats = dhat::HeapStats::get();
        return Some((stats.curr_bytes, stats.max_bytes));
    }
    None
}

/// A bounded heap/future diagnostic; ordinary runs collect neither.
#[derive(Debug, Default, clap::Args)]
pub struct ProfileArgs {
    #[arg(long)]
    pub dhat_out: Option<PathBuf>,
    #[arg(long, requires = "dhat_out")]
    pub dhat_frames: Option<usize>,
    #[arg(long)]
    pub future_out: Option<PathBuf>,
    #[arg(long, default_value_t = 20, requires = "future_out")]
    pub future_top: usize,
}

/// Owns a process-specific window, including setup and teardown of its workload.
pub struct ProfileWindow {
    role: &'static str,
    window: &'static str,
    started: std::time::Instant,
    path: Option<PathBuf>,
    future_path: Option<PathBuf>,
    future_top: usize,
    futures: Option<lash_core::task::sizes::Window>,
    #[cfg(feature = "dhat-heap")]
    profiler: Option<dhat::Profiler>,
    #[cfg(not(feature = "dhat-heap"))]
    profiler: Option<()>,
}

impl ProfileWindow {
    pub fn start(
        role: &'static str,
        window: &'static str,
        args: &ProfileArgs,
    ) -> anyhow::Result<Self> {
        ensure_dhat_parent(args.dhat_out.as_ref())?;
        if let Some(path) = &args.future_out {
            ensure_parent_dir(path, "future report")?;
        }
        let profiler = start_dhat_profiler(
            args.dhat_out.clone(),
            args.dhat_frames,
            "--dhat-out requires a dhat-heap build",
        )?;
        let futures = if args.future_out.is_some() {
            Some(
                lash_core::task::sizes::Window::start()
                    .ok_or_else(|| anyhow::anyhow!("future-size collection already active"))?,
            )
        } else {
            None
        };
        Ok(Self {
            role,
            window,
            started: std::time::Instant::now(),
            path: args.dhat_out.clone(),
            future_path: args.future_out.clone(),
            future_top: args.future_top,
            futures,
            profiler,
        })
    }

    pub fn finish(self, completed: bool) -> anyhow::Result<()> {
        let elapsed_ns = self.started.elapsed().as_nanos();
        let futures = self.futures.map(|window| window.finish(self.future_top));
        finish_dhat_profiler(self.profiler);
        if let (Some(path), Some(report)) = (self.future_path, futures) {
            std::fs::write(
                path,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "role": self.role, "window": self.window, "completed": completed,
                    "report": report,
                }))?,
            )?;
        }
        if let Some(path) = self.path {
            let receipt_path = path.with_extension("receipt.json");
            std::fs::write(
                receipt_path,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "kind": "lash.heap-profile", "role": self.role,
                    "process_id": std::process::id(), "window": self.window,
                    "window_elapsed_ns": elapsed_ns, "elapsed_statistic": "single_window_wall_duration",
                    "allocator": crate::ALLOCATION_MODE, "profile": path, "completed": completed,
                    "profile_quantities": {"tb": "cumulative_requested_bytes", "mb": "site_max_live_bytes",
                        "gb": "site_live_bytes_at_process_heap_peak", "eb": "site_live_bytes_at_window_end"},
                    "unit": "bytes", "scope": "this_process_rust_allocator_in_profile_window",
                    "certifying": false,
                }))?,
            )?;
        }
        Ok(())
    }
}

pub fn resolve_dhat_output_path(
    enable_dhat: bool,
    report_out: &Path,
    dhat_out: Option<PathBuf>,
    fallback_stem: &str,
) -> Option<PathBuf> {
    if enable_dhat {
        Some(dhat_out.unwrap_or_else(|| default_dhat_output_path(report_out, fallback_stem)))
    } else {
        None
    }
}

pub fn ensure_dhat_parent(path: Option<&PathBuf>) -> anyhow::Result<()> {
    if let Some(path) = path {
        ensure_parent_dir(path, "dhat output")?;
    }
    Ok(())
}

#[cfg(feature = "dhat-heap")]
pub fn start_dhat_profiler(
    dhat_out: Option<PathBuf>,
    dhat_frames: Option<usize>,
    _feature_error: &'static str,
) -> anyhow::Result<Option<dhat::Profiler>> {
    let Some(path) = dhat_out else {
        return Ok(None);
    };
    let profiler = dhat::Profiler::builder()
        .file_name(path)
        .trim_backtraces(dhat_frames)
        .build();
    PROFILING.store(true, std::sync::atomic::Ordering::Release);
    Ok(Some(profiler))
}

#[cfg(not(feature = "dhat-heap"))]
pub fn start_dhat_profiler(
    dhat_out: Option<PathBuf>,
    _dhat_frames: Option<usize>,
    feature_error: &'static str,
) -> anyhow::Result<Option<()>> {
    if dhat_out.is_some() {
        anyhow::bail!(feature_error);
    }
    Ok(None)
}

#[cfg(feature = "dhat-heap")]
pub fn finish_dhat_profiler(profiler: Option<dhat::Profiler>) {
    if profiler.is_some() {
        PROFILING.store(false, std::sync::atomic::Ordering::Release);
    }
    drop(profiler);
}

#[cfg(not(feature = "dhat-heap"))]
pub fn finish_dhat_profiler(_profiler: Option<()>) {}
