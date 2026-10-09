//! Opt-in physical SQL windows. The caller supplies a joinable operation identity.
//! Counts exclude pool pings, preparation and protocol messages. SQL values are
//! discarded before the bounded shape map retains a key.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;

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
    pub statements: u64,
    pub rows_returned: u64,
    pub sqlite_work: Option<SqliteWork>,
    pub shapes: BTreeMap<String, Shape>,
    pub unretained_shape_statements: u64,
    pub configured_shape_limit: usize,
}

/// Retained durable actor fence: a physical summary joins the owner commit
/// without putting session/process identifiers in metric attributes.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
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
    receipt: Arc<Mutex<Receipt>>,
    parent: Option<Box<Window>>,
}
tokio::task_local! { static CURRENT: Window; }
thread_local! { static WORKER: RefCell<Option<Window>> = const { RefCell::new(None) }; }

impl Window {
    pub fn new(operation: impl Into<String>, backend: &'static str) -> Self {
        let parent = Self::current();
        let parent_operation = parent.as_ref().map(|p| p.snapshot().operation);
        Self {
            started: std::time::Instant::now(),
            receipt: Arc::new(Mutex::new(Receipt {
                owner: OWNER.try_with(Clone::clone).ok(),
                elapsed_nanos: 0,
                operation: operation.into(),
                counts_statistic: "sum in operation window",
                population: "caller task and delegated connection work",
                parent_operation,
                backend,
                statements: 0,
                rows_returned: 0,
                sqlite_work: (backend == "sqlite").then(SqliteWork::default),
                shapes: BTreeMap::new(),
                unretained_shape_statements: 0,
                configured_shape_limit: SHAPE_LIMIT,
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
        self.record(sql, true, false, None);
    }
    pub fn row(&self, sql: &str) {
        self.record(sql, false, true, None);
    }
    pub fn work(&self, sql: &str, work: SqliteWork) {
        self.record(sql, false, false, Some(work));
    }
    fn record(&self, sql: &str, statement: bool, row: bool, work: Option<SqliteWork>) {
        if let Some(parent) = &self.parent {
            parent.record(sql, statement, row, work);
        }
        let key = normalize(sql);
        let mut receipt = self.receipt.lock().unwrap_or_else(PoisonError::into_inner);
        receipt.statements += u64::from(statement);
        receipt.rows_returned += u64::from(row);
        if let (Some(total), Some(work)) = (&mut receipt.sqlite_work, work) {
            total.add(work);
        }
        if !receipt.shapes.contains_key(&key) && receipt.shapes.len() == SHAPE_LIMIT {
            receipt.unretained_shape_statements += u64::from(statement);
            return;
        }
        let sqlite_work = receipt.sqlite_work.map(|_| SqliteWork::default());
        let shape = receipt.shapes.entry(key).or_insert_with(|| Shape {
            sqlite_work,
            ..Shape::default()
        });
        shape.statements += u64::from(statement);
        shape.rows_returned += u64::from(row);
        if let (Some(total), Some(work)) = (&mut shape.sqlite_work, work) {
            total.add(work);
        }
    }
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
        let receipt = window.snapshot();
        assert_eq!(receipt.shapes.len(), SHAPE_LIMIT);
        assert_eq!(receipt.statements, SHAPE_LIMIT as u64 + 2);
        assert_eq!(receipt.unretained_shape_statements, 1);
        assert_eq!(receipt.shapes["select ?, ?, ?"].statements, 2);
        assert_eq!(normalize("select $body$private$body$"), "<opaque SQL>");
    }
}
