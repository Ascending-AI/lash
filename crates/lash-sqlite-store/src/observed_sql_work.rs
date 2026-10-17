//! Per-execution join for the existing trace callback. PROFILE can precede
//! total_changes updates (notably DELETE's truncate path). Read the final
//! delta at the next statement start or the connection-call boundary.
use super::*;
use lash_core_ids::perf_witness::sql_shapes;
use lash_core_ids::perf_witness::sql_shapes::work::Backend;
use lash_core_ids::perf_witness::sql_shapes::work::Execution;
thread_local! {
    static ACTIVE: RefCell<HashMap<usize, (Execution, i64)>> = RefCell::new(HashMap::new());
    static FINISHED: RefCell<HashMap<usize, (Execution, i64)>> = RefCell::new(HashMap::new());
}

#[expect(
    unsafe_code,
    reason = "trace callback owns these live statement/connection handles"
)]
pub(super) unsafe fn start(statement: *mut ffi::sqlite3_stmt, sql: &str) {
    if !sql_shapes::enabled() {
        return;
    }
    // FK/trigger subprograms can repeat STMT on the outer live handle,
    // even with the original SQL text. One PROFILE owns that invocation.
    if ACTIVE.with(|active| active.borrow().contains_key(&statement.addr())) {
        return;
    }
    unsafe {
        let connection = ffi::sqlite3_db_handle(statement);
        flush_handle(connection);
        let Some(token) = Execution::start(
            Backend::Sqlite,
            sql,
            sql_shapes::work::origin(),
            lash_store_sql::relation_set(sql),
            ffi::sqlite3_stmt_readonly(statement) != 0,
        ) else {
            return;
        };
        let expanded = ffi::sqlite3_expanded_sql(statement);
        if expanded.is_null() {
            token.bindings(None, None, "unknown");
        } else {
            let text = CStr::from_ptr(expanded).to_bytes();
            let bytes =
                (text.len() + super::placeholder_bytes(sql)).saturating_sub(sql.len()) as u64;
            let (class, exact) = identity_class(sql, text);
            let identity = (exact && text.len() + sql.len() + 8 <= sql_shapes::work::VALUE_LIMIT)
                .then(|| {
                    let mut key = (sql.len() as u64).to_le_bytes().to_vec();
                    key.extend_from_slice(sql.as_bytes());
                    key.extend_from_slice(text);
                    key
                });
            token.bindings(Some(bytes), identity, &class);
            ffi::sqlite3_free(expanded.cast());
        }
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            if active.len() >= 1024 {
                active.clear();
            }
            active.insert(
                statement.addr(),
                (token, ffi::sqlite3_total_changes64(connection)),
            );
        });
    }
}

// Expanded REAL spelling uses only 15 significant digits. TEXT identity is
// exact modulo embedded NUL (FIG-6621 owner ruling); values never leave memory.
fn identity_class(sql: &str, expanded: &[u8]) -> (String, bool) {
    let mut classes = std::collections::BTreeSet::new();
    let mut exact = true;
    let mut original = 0;
    let mut at = 0;
    for span in super::placeholders(sql) {
        at += span.start - original;
        original = span.end;
        let Some(rest) = expanded.get(at..) else {
            return ("unknown".into(), false);
        };
        let (spelled, _) = super::literal(rest);
        let Some(value) = rest.get(..spelled) else {
            return ("unknown".into(), false);
        };
        let class = if rest.starts_with(b"'") {
            "text_modulo_embedded_nul"
        } else if rest.starts_with(b"x'")
            || rest.starts_with(b"X'")
            || rest.starts_with(b"zeroblob(")
        {
            "blob"
        } else if rest.starts_with(b"NULL") {
            "null"
        } else if value.contains(&b'.') || value.contains(&b'e') || value.contains(&b'E') {
            exact = false;
            "real_unqualified"
        } else {
            "integer"
        };
        classes.insert(class);
        at += if rest.starts_with(b"zeroblob(") {
            let Some(end) = rest.iter().position(|b| *b == b')') else {
                return ("unknown".into(), false);
            };
            end + 1
        } else {
            spelled
        };
    }
    if classes.is_empty() {
        classes.insert("no_bind");
    }
    (classes.into_iter().collect::<Vec<_>>().join("+"), exact)
}

#[expect(
    unsafe_code,
    reason = "borrowed row columns are live during SQLITE_TRACE_ROW"
)]
pub(super) unsafe fn row(statement: *mut ffi::sqlite3_stmt) {
    if !sql_shapes::enabled() {
        return;
    }
    ACTIVE.with(|active| {
        if let Some((token, _)) = active.borrow().get(&statement.addr()) {
            let bytes = unsafe {
                (0..ffi::sqlite3_column_count(statement))
                    .map(|col| match ffi::sqlite3_column_type(statement, col) {
                        ffi::SQLITE_INTEGER | ffi::SQLITE_FLOAT => 8,
                        ffi::SQLITE_TEXT | ffi::SQLITE_BLOB => {
                            ffi::sqlite3_column_bytes(statement, col) as u64
                        }
                        _ => 0,
                    })
                    .sum()
            };
            token.row(bytes);
        }
    });
}
#[expect(unsafe_code, reason = "SQLite supplies the borrowed connection handle")]
pub(super) unsafe fn profile(statement: *mut ffi::sqlite3_stmt) {
    if !sql_shapes::enabled() {
        return;
    }
    if let Some(value) = ACTIVE.with(|active| active.borrow_mut().remove(&statement.addr())) {
        let connection = unsafe { ffi::sqlite3_db_handle(statement) };
        FINISHED.with(|finished| {
            let mut finished = finished.borrow_mut();
            if finished.len() >= 1024 {
                finished.clear();
            }
            finished.insert(connection.addr(), value);
        });
    }
}
#[expect(
    unsafe_code,
    reason = "connection-call boundary retains this live handle"
)]
pub(super) fn flush(connection: &rusqlite::Connection) {
    if !sql_shapes::enabled() {
        return;
    }
    unsafe {
        flush_handle(connection.handle());
    }
}
#[expect(
    unsafe_code,
    reason = "reads the live connection's completed change counter"
)]
unsafe fn flush_handle(connection: *mut ffi::sqlite3) {
    if let Some((token, before)) =
        FINISHED.with(|finished| finished.borrow_mut().remove(&connection.addr()))
    {
        let after = unsafe { ffi::sqlite3_total_changes64(connection) };
        token.affected(after.saturating_sub(before).max(0) as u64);
        token.finish();
    }
}
