//! Task-scoped physical turn measurements. No process-global collector.
use lash_trace::{TurnPhaseInterval, TurnProfile, TurnSqlWork};
use std::{
    future::Future,
    sync::{Arc, Mutex, PoisonError},
    time::Instant,
};
const INTERVAL_LIMIT: usize = 512;
const SQL_KEY_LIMIT: usize = 512;

#[derive(Clone)]
pub struct Context {
    started: Instant,
    profile: Arc<Mutex<TurnProfile>>,
    phase: &'static str,
    phase_index: Option<usize>,
    depth: u32,
    revision: Option<i64>,
}
tokio::task_local! { static CURRENT: Context; }
impl Default for Context {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            profile: Arc::new(Mutex::new(TurnProfile {
                started_at_unix_ns: nanos(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default(),
                ),
                ..Default::default()
            })),
            phase: "owner orchestration",
            phase_index: None,
            depth: 0,
            revision: None,
        }
    }
}
impl Context {
    pub fn current() -> Option<Self> {
        CURRENT.try_with(Clone::clone).ok()
    }
    pub fn within<T>(&self, future: impl Future<Output = T>) -> impl Future<Output = T> {
        CURRENT.scope(self.clone(), Box::pin(future))
    }
    pub fn sync<T>(&self, work: impl FnOnce() -> T) -> T {
        CURRENT.sync_scope(self.clone(), work)
    }
    pub fn snapshot(&self) -> TurnProfile {
        let mut result = self
            .profile
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        result.elapsed_ns = nanos(self.started.elapsed());
        for phase in &mut result.phases {
            phase.end_ns = phase.end_ns.min(result.elapsed_ns);
        }
        result
    }
    pub fn sql(&self, work: TurnSqlWork) {
        let TurnSqlWork {
            origin,
            owner_commit,
            backend,
            shape,
            statements,
            round_trips,
            rows_returned: rows,
            rows_affected: affected,
            bound_bytes: bound,
            result_bytes: result,
            ..
        } = work;
        let mut profile = self.profile.lock().unwrap_or_else(PoisonError::into_inner);
        profile.statements += statements;
        if let Some(index) = self.phase_index {
            let interval = &mut profile.phases[index];
            interval.statements += statements;
            interval.round_trips += round_trips;
            interval.rows_returned += rows;
            interval.rows_affected += affected;
            interval.bound_bytes += bound;
            interval.result_bytes += result;
        }
        let verb = shape.split_whitespace().next().unwrap_or_default();
        if matches!(verb, "insert" | "update" | "delete" | "replace") {
            profile.dml += statements;
        }
        if verb == "begin" {
            profile.transactions += statements;
        }
        let existing = profile.sql.iter().position(|r| {
            r.phase_index == self.phase_index
                && r.phase == self.phase
                && r.origin == origin
                && r.owner_commit == owner_commit
                && r.shape == shape
                && r.backend == backend
                && r.revision == self.revision
        });
        let index = match existing {
            Some(index) => index,
            None if profile.sql.len() < SQL_KEY_LIMIT => {
                profile.sql.push(TurnSqlWork {
                    phase: self.phase.into(),
                    phase_index: self.phase_index,
                    origin,
                    owner_commit,
                    shape,
                    backend,
                    revision: self.revision,
                    ..Default::default()
                });
                profile.sql.len() - 1
            }
            None => {
                profile.omitted_sql_keys += 1;
                return;
            }
        };
        let row = &mut profile.sql[index];
        row.statements += statements;
        row.round_trips += round_trips;
        row.rows_returned += rows;
        row.rows_affected += affected;
        row.bound_bytes += bound;
        row.result_bytes += result;
    }
    pub fn installed(&self, bytes: u64) {
        self.profile
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .store_bytes_written += bytes;
    }
}
fn nanos(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

struct Interval {
    context: Context,
    index: Option<usize>,
}
impl Drop for Interval {
    fn drop(&mut self) {
        if let Some(index) = self.index {
            self.context
                .profile
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .phases[index]
                .end_ns = nanos(self.context.started.elapsed());
        }
    }
}
fn interval(phase: &'static str, revision: Option<i64>) -> Option<Interval> {
    Context::current().map(|mut context| {
        context.phase = phase;
        context.depth += 1;
        context.revision = revision;
        let index = {
            let mut profile = context
                .profile
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if profile.phases.len() == INTERVAL_LIMIT {
                profile.omitted_intervals += 1;
                None
            } else {
                let index = profile.phases.len();
                profile.phases.push(TurnPhaseInterval {
                    phase: phase.into(),
                    start_ns: nanos(context.started.elapsed()),
                    end_ns: u64::MAX,
                    depth: context.depth,
                    revision,
                    ..Default::default()
                });
                Some(index)
            }
        };
        context.phase_index = index;
        Interval { index, context }
    })
}
pub fn phase<T>(
    name: &'static str,
    revision: Option<i64>,
    future: impl Future<Output = T>,
) -> impl Future<Output = T> {
    let future = Box::pin(future);
    async move {
        match interval(name, revision) {
            Some(guard) => guard.context.within(future).await,
            None => future.await,
        }
    }
}
pub fn sync_phase<T>(name: &'static str, work: impl FnOnce() -> T) -> T {
    match interval(name, None) {
        Some(guard) => CURRENT.sync_scope(guard.context.clone(), work),
        None => work(),
    }
}
pub fn callbacks(snapshots: u64, manifests: u64, links: u64) {
    if let Some(context) = Context::current() {
        let mut p = context
            .profile
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        p.snapshots += snapshots;
        p.manifests += manifests;
        p.link_payloads += links;
    }
}

/// Time a synchronous wait on the connection thread, using its carried context.
pub fn worker_wait<T>(name: &'static str, work: impl FnOnce() -> T) -> T {
    match super::sql::Window::current().and_then(|w| w.turn_context()) {
        Some(context) => CURRENT.sync_scope(context, || sync_phase(name, work)),
        None => work(),
    }
}

/// Attribute an inserted graph node to its bounded schema kind.
pub fn installed_kind(window: Option<&super::sql::Window>, kind: &str, bytes: u64) {
    if let Some(context) = Context::current().or_else(|| window.and_then(|w| w.turn_context())) {
        let mut p = context
            .profile
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(row) = p
            .writes
            .iter_mut()
            .find(|r| r.phase == context.phase && r.revision == context.revision && r.kind == kind)
        {
            row.nodes += 1;
            row.bytes += bytes;
        } else if p.writes.len() < SQL_KEY_LIMIT {
            p.writes.push(lash_trace::TurnStoreWrite {
                phase: context.phase.into(),
                revision: context.revision,
                kind: kind.into(),
                nodes: 1,
                bytes,
            });
        } else {
            p.omitted_sql_keys += 1;
        }
    }
}

pub struct Wait {
    _interval: Option<Interval>,
}
impl Wait {
    pub fn start(name: &'static str) -> Self {
        Self {
            _interval: interval(name, None),
        }
    }
}

/// Carry measurements to an explicitly spawned child task.
pub fn carry<T>(future: impl Future<Output = T>) -> impl Future<Output = T> {
    let context = Context::current();
    let window = super::sql::Window::current();
    let future = Box::pin(future);
    async move {
        match context {
            Some(context) => {
                context
                    .within(async {
                        match window {
                            Some(window) => window.within(future).await,
                            None => {
                                super::sql::Window::new("turn.child", "mixed")
                                    .within(future)
                                    .await
                            }
                        }
                    })
                    .await
            }
            None => future.await,
        }
    }
}

/// Collect one claim attempt only when its node has turn telemetry installed.
pub async fn claim<T>(context: Option<&Context>, future: impl Future<Output = T>) -> T {
    let future = Box::pin(future);
    match context {
        Some(context) => {
            context
                .within(async {
                    let window = super::sql::Window::new("owner claim", "mixed");
                    window.within(phase("claim", None, future)).await
                })
                .await
        }
        None => future.await,
    }
}
impl Context {
    /// Give each actor an independent profile of its shared claim. Physical SQL
    /// belongs to the first claimed actor; every member retains the wall phase.
    pub fn claimed_actor(&self, first: bool) -> Self {
        let mut profile = self.snapshot();
        if !first {
            profile.sql.clear();
            for interval in &mut profile.phases {
                interval.statements = 0;
                interval.round_trips = 0;
                interval.rows_returned = 0;
                interval.rows_affected = 0;
                interval.bound_bytes = 0;
                interval.result_bytes = 0;
            }
            profile.writes.clear();
            profile.statements = 0;
            profile.dml = 0;
            profile.transactions = 0;
            profile.store_bytes_written = 0;
        }
        Self {
            profile: Arc::new(Mutex::new(profile)),
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TURN-PHASE-SQL: repeated and nested intervals own each physical SQL
    /// execution once, and a shared claim assigns SQL to only its first actor.
    #[tokio::test]
    async fn physical_sql_belongs_to_the_exact_deepest_interval() {
        let context = Context::default();
        context
            .within(async {
                let window = super::super::sql::Window::new("phase-law", "postgres");
                window
                    .within(async {
                        for _ in 0..2 {
                            phase("prompt/environment build", None, async {
                                window.statement("SELECT $1");
                                window.payload("SELECT $1", 0, 13, 17);
                                phase("nested", None, async {
                                    window.statement("SELECT $2");
                                })
                                .await;
                            })
                            .await;
                        }
                    })
                    .await;
            })
            .await;
        let profile = context.snapshot();
        assert_eq!(profile.statements, 4);
        assert_eq!(profile.phases.len(), 4);
        for (index, interval) in profile.phases.iter().enumerate() {
            assert_eq!(interval.statements, 1);
            assert_eq!(interval.round_trips, 1);
            assert_eq!(
                profile
                    .sql
                    .iter()
                    .filter(|row| row.phase_index == Some(index))
                    .map(|row| row.statements)
                    .sum::<u64>(),
                1
            );
        }
        assert_eq!(profile.phases[0].bound_bytes, 13);
        assert_eq!(profile.phases[0].result_bytes, 17);
        context
            .within(phase("overflow", None, async {
                let window = super::super::sql::Window::new("overflow", "postgres");
                for index in 0..=SQL_KEY_LIMIT {
                    window.statement(&format!("SELECT field{index}"));
                }
            }))
            .await;
        let bounded = context.snapshot();
        assert_eq!(
            bounded.phases.last().unwrap().statements,
            SQL_KEY_LIMIT as u64 + 1
        );
        assert_eq!(bounded.sql.len(), SQL_KEY_LIMIT);
        assert!(bounded.omitted_sql_keys > 0);
        let follower = context.claimed_actor(false).snapshot();
        assert_eq!(follower.statements, 0);
        assert!(follower.phases.iter().all(|phase| phase.statements == 0
            && phase.round_trips == 0
            && phase.bound_bytes == 0
            && phase.result_bytes == 0));
    }
}
