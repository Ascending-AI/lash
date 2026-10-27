//! Opt-in physical SQL windows. The caller supplies a joinable operation identity.
//! Counts exclude pool pings, preparation and protocol messages. SQL values are
//! discarded before the bounded shape map retains a key.
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, RwLock};

use serde::{Deserialize, Serialize};

/// Configured limits, not measured cardinalities.
pub const SHAPE_LIMIT: usize = 64;
const SQL_BYTES_LIMIT: usize = 4096;

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct SqliteWork {
    pub vm_steps: u64,
    /// SQLite FullscanStep: advances during a full table scan, not all scanned rows.
    pub fullscan_steps: u64,
    pub sorts: u64,
    pub autoindex_rows: u64,
    pub reprepares: u64,
}
impl SqliteWork {
    pub fn difference(self, before: Self) -> Self {
        Self {
            vm_steps: self.vm_steps.saturating_sub(before.vm_steps),
            fullscan_steps: self.fullscan_steps.saturating_sub(before.fullscan_steps),
            sorts: self.sorts.saturating_sub(before.sorts),
            autoindex_rows: self.autoindex_rows.saturating_sub(before.autoindex_rows),
            reprepares: self.reprepares.saturating_sub(before.reprepares),
        }
    }
    fn add(&mut self, work: Self) {
        self.vm_steps += work.vm_steps;
        self.fullscan_steps += work.fullscan_steps;
        self.sorts += work.sorts;
        self.autoindex_rows += work.autoindex_rows;
        self.reprepares += work.reprepares;
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Shape {
    pub statements: u64,
    pub rows_returned: u64,
    pub rows_affected: u64,
    /// PostgreSQL encoded parameter payload; SQLite expanded bound SQL spelling.
    pub bound_bytes: u64,
    /// Bind payload could not be measured; these are unknown, not zero-byte binds.
    pub unmeasured_bound_statements: u64,
    /// Returned scalar payload (text/blob bytes; numeric storage width).
    pub result_bytes: u64,
    pub sqlite_work: Option<SqliteWork>,
}

/// Counts sum physical work in the caller's operation window. Wall time is
/// elapsed nanoseconds; owner epochs/revisions are identities, not measures.
/// PostgreSQL server work is unknown, represented by None, never zero.
#[derive(Clone, Debug, Serialize)]
pub struct Receipt {
    pub owner: Option<OwnerIdentity>,
    /// Observed caller-window wall time; includes queueing and observation.
    pub elapsed_nanos: u64,
    pub operation: String,
    pub counts_statistic: &'static str,
    pub population: &'static str,
    pub parent_operation: Option<String>,
    pub backend: &'static str,
    pub rows_affected_unit: &'static str,
    pub bound_bytes_unit: &'static str,
    pub result_bytes_unit: &'static str,
    pub statements: u64,
    pub rows_returned: u64,
    pub rows_affected: u64,
    /// PostgreSQL encoded parameter payload; SQLite expanded bound SQL spelling.
    pub bound_bytes: u64,
    /// Bind payload could not be measured; these are unknown, not zero-byte binds.
    pub unmeasured_bound_statements: u64,
    /// Returned scalar payload (text/blob bytes; numeric storage width).
    pub result_bytes: u64,
    pub sqlite_work: Option<SqliteWork>,
    /// New canonical node bytes inserted in this attempt; only committed on success.
    pub newly_installed_graph_bytes: u64,
    pub shapes: BTreeMap<String, Shape>,
    pub unretained_shape_statements: u64,
    pub configured_shape_limit: usize,
    /// Statement texts this window's observations normalized into shape keys.
    pub normalized_sql_texts: u64,
}

/// Retained durable actor fence: a physical summary joins the owner commit
/// without putting session/process identifiers in metric attributes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct OwnerIdentity {
    pub actor: String,
    pub epoch: i64,
    pub revision: i64,
}
tokio::task_local! { static OWNER: OwnerIdentity; }
pub async fn with_owner<T>(
    actor: String,
    epoch: i64,
    revision: i64,
    future: impl Future<Output = T>,
) -> T {
    OWNER
        .scope(
            OwnerIdentity {
                actor,
                epoch,
                revision,
            },
            future,
        )
        .await
}

#[derive(Clone)]
pub struct Window {
    started: std::time::Instant,
    turn: Option<super::turn::Context>,
    receipt: Arc<Mutex<Receipt>>,
    parent: Option<Box<Window>>,
}
tokio::task_local! { static CURRENT: Window; }
thread_local! { static WORKER: RefCell<Option<Window>> = const { RefCell::new(None) }; }

impl Window {
    pub fn new(operation: impl Into<String>, backend: &'static str) -> Self {
        let parent = Self::current();
        let parent_operation = parent.as_ref().map(|p| p.identity().1);
        Self {
            started: std::time::Instant::now(),
            turn: super::turn::Context::current()
                .or_else(|| parent.as_ref().and_then(|p| p.turn.clone())),
            receipt: Arc::new(Mutex::new(Receipt {
                owner: OWNER.try_with(Clone::clone).ok(),
                elapsed_nanos: 0,
                operation: operation.into(),
                counts_statistic: "sum in operation window",
                population: "caller task and delegated connection work",
                parent_operation,
                backend,
                rows_affected_unit: if backend == "sqlite" {
                    "sqlite3_total_changes64 delta; includes trigger and foreign-key writes; attempted changes, committed only on success"
                } else {
                    "SQLx server command-completion rows; includes SELECT rows, excludes hidden CTE/trigger mutations"
                },
                bound_bytes_unit: if backend == "sqlite" {
                    "UTF-8 bytes of expanded bound SQL literals, repeated occurrences included"
                } else {
                    "binary parameter payload bytes, protocol framing excluded"
                },
                result_bytes_unit: if backend == "sqlite" {
                    "text/blob payload bytes and 8 bytes per integer/float, NULL zero"
                } else {
                    "returned column format payload bytes, protocol framing excluded"
                },
                statements: 0,
                rows_returned: 0,
                rows_affected: 0,
                bound_bytes: 0,
                unmeasured_bound_statements: 0,
                result_bytes: 0,
                newly_installed_graph_bytes: 0,
                sqlite_work: (backend == "sqlite").then(SqliteWork::default),
                shapes: BTreeMap::new(),
                unretained_shape_statements: 0,
                configured_shape_limit: SHAPE_LIMIT,
                normalized_sql_texts: 0,
            })),
            parent: parent.map(Box::new),
        }
    }
    pub async fn within<T>(&self, future: impl Future<Output = T>) -> T {
        CURRENT.scope(self.clone(), future).await
    }

    pub fn current() -> Option<Self> {
        CURRENT
            .try_with(Clone::clone)
            .ok()
            .or_else(|| WORKER.with(|slot| slot.borrow().clone()))
    }
    pub fn turn_context(&self) -> Option<super::turn::Context> {
        self.turn.clone()
    }
    /// Correlation fields without cloning the operation's shape map.
    pub fn identity(&self) -> (Option<OwnerIdentity>, String) {
        let receipt = self.receipt.lock().unwrap_or_else(PoisonError::into_inner);
        (receipt.owner.clone(), receipt.operation.clone())
    }

    pub fn snapshot(&self) -> Receipt {
        let mut receipt = self
            .receipt
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        receipt.elapsed_nanos =
            u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        receipt
    }
    /// Carry the task window onto SQLite's connection thread. Restore the
    /// previous context even if the callback unwinds.
    pub fn on_worker<T>(window: Option<Self>, work: impl FnOnce() -> T) -> T {
        struct Restore(Option<Window>);
        impl Drop for Restore {
            fn drop(&mut self) {
                WORKER.with(|slot| *slot.borrow_mut() = self.0.take());
            }
        }
        let _restore = Restore(WORKER.with(|slot| slot.replace(window)));
        work()
    }
    pub fn statement(&self, sql: &str) {
        let key = ShapeKey::of(sql);
        self.turn_work(&key, 1, 0, 0, 0, 0);
        self.record(&key, true, false, None);
    }
    pub fn row(&self, sql: &str) {
        let key = ShapeKey::of(sql);
        self.turn_work(&key, 0, 1, 0, 0, 0);
        self.record(&key, false, true, None);
    }
    pub fn work(&self, sql: &str, work: SqliteWork) {
        let key = ShapeKey::of(sql);
        self.record(&key, false, false, Some(work));
    }
    /// Physical payload counters, propagated through nested operation windows.
    pub fn payload(&self, sql: &str, rows_affected: u64, bound_bytes: u64, result_bytes: u64) {
        let key = ShapeKey::of(sql);
        self.turn_work(&key, 0, 0, rows_affected, bound_bytes, result_bytes);
        self.add_payload(&key, rows_affected, bound_bytes, result_bytes);
    }
    fn turn_work(
        &self,
        key: &ShapeKey,
        statements: u64,
        rows: u64,
        affected: u64,
        bound: u64,
        result: u64,
    ) {
        if let Some(turn) = super::turn::Context::current().or_else(|| self.turn.clone()) {
            let receipt = self.receipt.lock().unwrap_or_else(PoisonError::into_inner);
            turn.sql(lash_trace::TurnSqlWork {
                owner_commit: receipt.owner.is_some(),
                origin: receipt.operation.clone(),
                backend: receipt.backend.into(),
                shape: key.shape.to_string(),
                statements,
                round_trips: if receipt.backend == "postgres" {
                    statements
                } else {
                    0
                },
                rows_returned: rows,
                rows_affected: affected,
                bound_bytes: bound,
                result_bytes: result,
                ..Default::default()
            });
        }
    }
    fn add_payload(&self, key: &ShapeKey, rows_affected: u64, bound_bytes: u64, result_bytes: u64) {
        if let Some(parent) = &self.parent {
            parent.add_payload(key, rows_affected, bound_bytes, result_bytes);
        }
        let mut receipt = self.receipt.lock().unwrap_or_else(PoisonError::into_inner);
        receipt.normalized_sql_texts += u64::from(key.normalized);
        receipt.rows_affected += rows_affected;
        receipt.bound_bytes += bound_bytes;
        receipt.result_bytes += result_bytes;
        if let Some(shape) = receipt.shapes.get_mut(&*key.shape) {
            shape.rows_affected += rows_affected;
            shape.bound_bytes += bound_bytes;
            shape.result_bytes += result_bytes;
        }
    }
    pub fn unmeasured_bound(&self, sql: &str) {
        self.add_unmeasured_bound(&ShapeKey::of(sql));
    }
    fn add_unmeasured_bound(&self, key: &ShapeKey) {
        if let Some(parent) = &self.parent {
            parent.add_unmeasured_bound(key);
        }
        let mut receipt = self.receipt.lock().unwrap_or_else(PoisonError::into_inner);
        receipt.normalized_sql_texts += u64::from(key.normalized);
        receipt.unmeasured_bound_statements += 1;
        if let Some(shape) = receipt.shapes.get_mut(&*key.shape) {
            shape.unmeasured_bound_statements += 1;
        }
    }
    pub fn installed_graph(&self, bytes: u64) {
        if let Some(turn) = &self.turn {
            turn.installed(bytes);
        }
        self.add_installed_graph(bytes);
    }
    fn add_installed_graph(&self, bytes: u64) {
        if let Some(parent) = &self.parent {
            parent.add_installed_graph(bytes);
        }
        self.receipt
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .newly_installed_graph_bytes += bytes;
    }
    fn record(&self, key: &ShapeKey, statement: bool, row: bool, work: Option<SqliteWork>) {
        if let Some(parent) = &self.parent {
            parent.record(key, statement, row, work);
        }
        let mut receipt = self.receipt.lock().unwrap_or_else(PoisonError::into_inner);
        receipt.normalized_sql_texts += u64::from(key.normalized);
        receipt.statements += u64::from(statement);
        receipt.rows_returned += u64::from(row);
        if let (Some(total), Some(work)) = (&mut receipt.sqlite_work, work) {
            total.add(work);
        }
        if !receipt.shapes.contains_key(&*key.shape) {
            if receipt.shapes.len() == SHAPE_LIMIT {
                receipt.unretained_shape_statements += u64::from(statement);
                return;
            }
            let sqlite_work = receipt.sqlite_work.map(|_| SqliteWork::default());
            receipt.shapes.insert(
                key.shape.to_string(),
                Shape {
                    sqlite_work,
                    ..Shape::default()
                },
            );
        }
        let Some(shape) = receipt.shapes.get_mut(&*key.shape) else {
            return;
        };
        shape.statements += u64::from(statement);
        shape.rows_returned += u64::from(row);
        if let (Some(total), Some(work)) = (&mut shape.sqlite_work, work) {
            total.add(work);
        }
    }
}

/// Distinct statement texts whose shape keys the process keeps. Past it, a
/// new text is normalized on each observation rather than retained.
const SHAPE_KEY_CACHE_LIMIT: usize = 4096;

/// A statement text's length and SipHash digest: no statement text, nor any
/// literal in it, is retained.
type TextDigest = (usize, u64);

/// Shape keys by statement text digest.
static SHAPE_KEYS: LazyLock<RwLock<HashMap<TextDigest, Arc<str>>>> = LazyLock::new(RwLock::default);

/// A statement text's shape key, and whether this observation normalized it.
struct ShapeKey {
    shape: Arc<str>,
    normalized: bool,
}

impl ShapeKey {
    /// Each distinct text is normalized once per process: every later start,
    /// row, end and payload observation of it reuses its key (FIG-6276).
    fn of(sql: &str) -> Self {
        let digest = (sql.len(), {
            let mut hasher = std::hash::DefaultHasher::new();
            std::hash::Hash::hash(sql, &mut hasher);
            std::hash::Hasher::finish(&hasher)
        });
        let cached = SHAPE_KEYS
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&digest)
            .cloned();
        if let Some(shape) = cached {
            return Self {
                shape,
                normalized: false,
            };
        }
        let shape: Arc<str> = normalize(sql).into();
        let mut keys = SHAPE_KEYS.write().unwrap_or_else(PoisonError::into_inner);
        if keys.len() < SHAPE_KEY_CACHE_LIMIT {
            keys.insert(digest, Arc::clone(&shape));
        }
        Self {
            shape,
            normalized: true,
        }
    }
}

/// Reuse the bounded, literal-redacting SQL key cache for process witnesses.
#[cfg(feature = "perf-witness")]
pub(crate) fn normalized_shape(sql: &str) -> Arc<str> {
    ShapeKey::of(sql).shape
}

pub async fn collect<T>(
    operation: impl Into<String>,
    backend: &'static str,
    future: impl Future<Output = T>,
) -> (T, Receipt) {
    let window = Window::new(operation, backend);
    let result = CURRENT.scope(window.clone(), future).await;
    (result, window.snapshot())
}

/// Whitespace/case normalization and literal redaction. Oversized and
/// dollar-quoted SQL gets an opaque key rather than retaining payload text.
fn normalize(sql: &str) -> String {
    if sql.len() > SQL_BYTES_LIMIT || sql.contains("$$") {
        return "<opaque SQL>".into();
    }
    let mut chars = sql.chars().peekable();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        if c == '-' && chars.peek() == Some(&'-') {
            for next in chars.by_ref() {
                if next == '\n' {
                    break;
                }
            }
            out.push(' ');
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut depth = 1;
            while let Some(next) = chars.next() {
                if next == '/' && chars.peek() == Some(&'*') {
                    chars.next();
                    depth += 1;
                } else if next == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            out.push(' ');
        } else if c == '\'' {
            while let Some(next) = chars.next() {
                if next == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        break;
                    }
                } else if next == '\\' {
                    chars.next();
                }
            }
            out.push('?');
        } else if (c == '?' || c == '$') && chars.peek().is_some_and(char::is_ascii_digit) {
            while chars.peek().is_some_and(char::is_ascii_digit) {
                chars.next();
            }
            out.push('?');
        } else if c.is_ascii_digit()
            && !out
                .chars()
                .last()
                .is_some_and(|p| p.is_ascii_alphanumeric() || p == '_')
        {
            while chars
                .peek()
                .is_some_and(|p| p.is_ascii_alphanumeric() || matches!(*p, '.' | '+' | '-'))
            {
                chars.next();
            }
            out.push('?');
        } else if c == '$'
            && chars
                .peek()
                .is_some_and(|p| p.is_ascii_alphabetic() || *p == '_')
        {
            return "<opaque SQL>".into();
        } else {
            out.push(c.to_ascii_lowercase());
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    /// SQL-WINDOW: values do not become keys; retained cardinality is bounded
    /// without dropping statement totals, and cached bind positions coalesce.
    #[test]
    fn shape_keys_redact_values_and_bound_cardinality_without_losing_totals() {
        let window = Window::new("bounded", "sqlite");
        window.statement("SELECT 'secret', 42, ?1 -- secret comment");
        window.statement("select 'other', 99, ?2");
        for index in 0..SHAPE_LIMIT {
            window.statement(&format!("select col_{index} from table_{index}"));
        }
        window.unmeasured_bound("select 'unknown', 123, ?3");
        window.unmeasured_bound("select col_63 from table_63");
        let receipt = window.snapshot();
        assert_eq!(receipt.unmeasured_bound_statements, 2);
        assert_eq!(
            receipt.shapes["select ?, ?, ?"].unmeasured_bound_statements,
            1
        );
        assert_eq!(receipt.shapes.len(), SHAPE_LIMIT);
        assert_eq!(receipt.statements, SHAPE_LIMIT as u64 + 2);
        assert_eq!(receipt.unretained_shape_statements, 1);
        assert_eq!(receipt.shapes["select ?, ?, ?"].statements, 2);
        assert_eq!(normalize("select $body$private$body$"), "<opaque SQL>");
    }
}

/// Session-state reads counted at the adapter/decoder boundary. The actor
/// supplies attribution; independent host reads have no owner monitor.
#[derive(Clone, Copy, Debug)]
pub enum OwnerStateRead {
    Head,
    Phase,
}
/// Why an owner reads durable state.
#[derive(Clone, Copy, Debug)]
pub enum OwnerReadClass {
    Warm,
    Recovery,
    Historical,
}
type OwnerReadObserver = Arc<dyn Fn(OwnerStateRead, OwnerReadClass) + Send + Sync>;
tokio::task_local! {
    static OWNER_READ_OBSERVER: OwnerReadObserver;
    static OWNER_READ_CLASS: OwnerReadClass;
}
/// Attribute all underlying reads in an activation to its original claim.
pub async fn observe_owner_reads<T>(
    observer: OwnerReadObserver,
    future: impl Future<Output = T>,
) -> T {
    OWNER_READ_OBSERVER.scope(observer, future).await
}
/// Mark a coherent hydration or a genuinely historical read explicitly.
pub async fn classify_owner_reads<T>(class: OwnerReadClass, future: impl Future<Output = T>) -> T {
    OWNER_READ_CLASS.scope(class, future).await
}
/// Count where a complete head or committed phase is actually requested.
pub fn owner_state_read(source: OwnerStateRead) {
    let _ = OWNER_READ_OBSERVER.try_with(|observer| {
        observer(
            source,
            OWNER_READ_CLASS
                .try_with(|class| *class)
                .unwrap_or(OwnerReadClass::Warm),
        )
    });
}
