//! Opt-in process-wide SQL shape attribution, sharing the witness snapshot cut.
pub mod work;

use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

/// Cumulative counts for a normalized SQL shape and its executing store closure.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub statements: u64,
    pub selects: u64,
    pub rows_returned: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub shape: String,
    pub store_origin: String,
    #[serde(flatten)]
    pub counts: Counts,
}

/// Missing attribution is explicit; a replay refuses an incomplete sidecar.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub work: work::Receipt,
    pub rows: Vec<Row>,
    pub overflow: Counts,
}

#[derive(Default)]
pub(super) struct State {
    work: work::State,
    rows: BTreeMap<(Arc<str>, &'static str), Counts>,
    overflow: Counts,
}
impl State {
    pub(super) fn receipt(&self) -> Receipt {
        Receipt {
            work: self.work.receipt(),
            rows: self
                .rows
                .iter()
                .map(|((shape, origin), counts)| Row {
                    shape: shape.to_string(),
                    store_origin: (*origin).to_owned(),
                    counts: counts.clone(),
                })
                .collect(),
            overflow: self.overflow.clone(),
        }
    }
}
/// A whole replay exceeds the operation window's 64 shapes. Refuse export
/// rather than silently truncate if 4096 shape × origin pairs are exceeded.
pub const ROW_LIMIT: usize = 4096;
static ENABLED: AtomicBool = AtomicBool::new(false);
thread_local! {
    static ORIGIN: Cell<&'static str> = const { Cell::new("connection/batch envelope") };
}

/// Only the opt-in observer pays for tags, normalization, locks or aggregation.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Scope the actual store closure on the connection worker, including nesting.
pub fn with_origin<T>(origin: &'static str, work: impl FnOnce() -> T) -> T {
    if !enabled() {
        return work();
    }
    struct Restore(&'static str);
    impl Drop for Restore {
        fn drop(&mut self) {
            ORIGIN.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(ORIGIN.with(|slot| slot.replace(origin)));
    work()
}

pub(super) fn record(state: &mut Option<State>, sql: &str, statement: bool) {
    let Some(state) = state else {
        return;
    };
    let key = (
        crate::operational_metrics::sql::normalized_shape(sql),
        ORIGIN.with(Cell::get),
    );
    let counts = if state.rows.contains_key(&key) || state.rows.len() < ROW_LIMIT {
        state.rows.entry(key).or_default()
    } else {
        &mut state.overflow
    };
    if statement {
        counts.statements += 1;
        counts.selects += u64::from(super::SqlVerb::classify(sql) == super::SqlVerb::Select);
    } else {
        counts.rows_returned += 1;
    }
}

static STATE: LazyLock<Mutex<Option<State>>> = LazyLock::new(|| Mutex::new(None));
pub(super) fn lock() -> MutexGuard<'static, Option<State>> {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
pub(super) fn enable() {
    *lock() = Some(State::default());
    ENABLED.store(true, Ordering::Relaxed);
}
pub(super) fn disable() {
    ENABLED.store(false, Ordering::Relaxed);
    *lock() = None;
}
