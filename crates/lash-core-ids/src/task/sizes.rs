//! Unsupported diagnostic surface, compiled only with `perf-witness`.
//! Sizes describe inline future state, excluding runtime headers and pointed-to
//! allocations. Counts are constructions in this window, never live instances.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

static ENABLED: AtomicBool = AtomicBool::new(false);
type SizesByKind = BTreeMap<(&'static str, &'static str), FutureSize>;
static ROWS: Mutex<Option<SizesByKind>> = Mutex::new(None);

fn rows() -> MutexGuard<'static, Option<SizesByKind>> {
    ROWS.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One concrete kind observed before instrumentation or boxing.
#[derive(Clone, Debug, serde::Serialize)]
pub struct FutureSize {
    pub kind: &'static str,
    pub concrete_type: &'static str,
    pub max_inline_bytes: usize,
    pub constructions: u64,
}

/// A bounded process-wide collection window. Only one may be open at a time.
pub struct Window {
    started: Instant,
    finished: bool,
}

/// Top kinds ranked by maximum observed inline bytes, with deterministic ties.
#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub process_id: u32,
    pub window_elapsed_ns: u128,
    pub quantity: &'static str,
    pub unit: &'static str,
    pub statistic: &'static str,
    pub count_statistic: &'static str,
    pub omitted_kinds: usize,
    pub rows: Vec<FutureSize>,
}

impl Window {
    /// Refuse overlapping windows instead of resetting another observer.
    pub fn start() -> Option<Self> {
        let mut rows = rows();
        if rows.is_some() {
            return None;
        }
        *rows = Some(BTreeMap::new());
        ENABLED.store(true, Ordering::Release);
        Some(Self {
            started: Instant::now(),
            finished: false,
        })
    }

    pub fn finish(mut self, top: usize) -> Report {
        let elapsed = self.started.elapsed().as_nanos();
        ENABLED.store(false, Ordering::Release);
        let mut values: Vec<_> = rows().take().unwrap_or_default().into_values().collect();
        self.finished = true;
        values.sort_by(|a, b| {
            b.max_inline_bytes
                .cmp(&a.max_inline_bytes)
                .then_with(|| a.kind.cmp(b.kind))
                .then_with(|| a.concrete_type.cmp(b.concrete_type))
        });
        let omitted_kinds = values.len().saturating_sub(top);
        values.truncate(top);
        Report {
            process_id: std::process::id(),
            window_elapsed_ns: elapsed,
            quantity: "concrete_future_inline_state_before_erasure",
            unit: "bytes",
            statistic: "maximum_per_kind_and_concrete_type_in_window",
            count_statistic: "constructions_in_window",
            omitted_kinds,
            rows: values,
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // finish already took the map. The lock serializes teardown with start.
        let mut rows = rows();
        if rows.is_some() {
            ENABLED.store(false, Ordering::Release);
            *rows = None;
        }
    }
}

/// Called with the concrete value, before a wrapper changes its layout.
pub fn record<F: std::future::Future>(kind: &'static str, future: &F) {
    if !ENABLED.load(Ordering::Acquire) {
        return;
    }
    let concrete_type = std::any::type_name::<F>();
    let size = std::mem::size_of_val(future);
    if let Some(rows) = rows().as_mut() {
        let row = rows.entry((kind, concrete_type)).or_insert(FutureSize {
            kind,
            concrete_type,
            max_inline_bytes: size,
            constructions: 0,
        });
        row.max_inline_bytes = row.max_inline_bytes.max(size);
        row.constructions += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reported layout must be the captured array, before Tokio's tracing
    /// wrapper or a Box handle can replace the concrete future's size.
    #[test]
    fn spawn_reports_the_known_concrete_future_before_erasure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let window = Window::start().expect("exclusive window");
        assert!(Window::start().is_none());
        let payload = [7_u8; 4096];
        let future = async move {
            std::future::ready(()).await;
            std::hint::black_box(payload)
        };
        let expected = std::mem::size_of_val(&future);
        assert!(expected >= 4096);
        runtime.block_on(async { crate::task::spawn(future).await.expect("task") });
        let report = window.finish(1);
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].max_inline_bytes, expected);
        assert_eq!(report.rows[0].constructions, 1);
        assert_eq!(report.rows[0].kind, "task.spawn");
        assert_eq!(report.omitted_kinds, 0);

        let window = Window::start().expect("next window");
        record("small", &std::future::ready(()));
        record("large", &std::future::ready([0_u8; 128]));
        record("large", &std::future::ready([0_u8; 128]));
        let report = window.finish(1);
        assert_eq!(report.rows[0].kind, "large");
        assert_eq!(report.rows[0].constructions, 2);
        assert_eq!(report.omitted_kinds, 1);
    }
}
