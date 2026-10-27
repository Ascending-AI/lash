//! SQLite's single trace callback: physical rows and per-execution work,
//! including cached statements, on the connection's own worker.
use lash_core_ids::operational_metrics::sql::SqliteWork;
use lash_core_ids::operational_metrics::sql::Window;
use rusqlite::{StatementStatus, ffi};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, c_int, c_uint, c_void};

#[cfg(feature = "perf-witness")]
#[path = "observed_sql_work.rs"]
mod joined;

thread_local! {
    static BEFORE: RefCell<HashMap<usize, Before>> = RefCell::new(HashMap::new());
}
#[cfg(feature = "perf-witness")]
thread_local! {
    static HISTORY_BEFORE: RefCell<HashMap<usize, SqliteWork>> = RefCell::new(HashMap::new());
}

struct Before {
    work: SqliteWork,
    changes: i64,
    #[cfg(feature = "perf-witness")]
    binding: Option<BindingBytes>,
}
#[cfg(feature = "perf-witness")]
struct BindingBytes {
    expanded: usize,
    value: Option<u64>,
}

// SQLite owns each callback's statement and connection for its entire duration.
// The public StmtRef omits the row/connection handles needed for these counters.
#[expect(
    unsafe_code,
    reason = "SQLite trace supplies live borrowed statement handles"
)]
unsafe fn counters(statement: *mut ffi::sqlite3_stmt) -> SqliteWork {
    let count = |status: StatementStatus| unsafe {
        u64::try_from(ffi::sqlite3_stmt_status(statement, status as c_int, 0)).unwrap_or_default()
    };
    SqliteWork {
        vm_steps: count(StatementStatus::VmStep),
        fullscan_steps: count(StatementStatus::FullscanStep),
        sorts: count(StatementStatus::Sort),
        autoindex_rows: count(StatementStatus::AutoIndex),
        reprepares: count(StatementStatus::RePrepare),
    }
}

#[expect(
    unsafe_code,
    reason = "installs the synchronous SQLite trace callback on a live connection"
)]
pub(crate) fn enable(connection: &rusqlite::Connection) {
    unsafe {
        ffi::sqlite3_trace_v2(
            connection.handle(),
            (ffi::SQLITE_TRACE_STMT | ffi::SQLITE_TRACE_ROW | ffi::SQLITE_TRACE_PROFILE) as c_uint,
            Some(trace),
            std::ptr::null_mut(),
        );
    }
}

// One expansion may feed several observers, without formatting the same
// immutable binding values again at statement completion.
#[cfg(test)]
thread_local! {
    static EXPANSIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
#[expect(
    unsafe_code,
    reason = "the trace callback supplies a live SQLite statement"
)]
unsafe fn expanded_sql(statement: *mut ffi::sqlite3_stmt) -> *mut std::ffi::c_char {
    #[cfg(test)]
    EXPANSIONS.with(|count| count.set(count.get() + 1));
    unsafe { ffi::sqlite3_expanded_sql(statement) }
}

#[derive(Clone, Copy)]
struct BlobBindings {
    sql: &'static str,
    #[cfg(feature = "perf-witness")]
    value_bytes: u64,
    literal_bytes: u64,
    #[cfg(feature = "perf-witness")]
    expanded_bytes: usize,
}

thread_local! {
    static BLOB_BINDINGS: std::cell::Cell<Option<BlobBindings>> = const { std::cell::Cell::new(None) };
}

fn blob_bindings(sql: &str) -> Option<BlobBindings> {
    BLOB_BINDINGS
        .with(std::cell::Cell::get)
        .filter(|bindings| bindings.sql == sql)
}

/// Supply the lengths of one-use blob parameters while their statement runs.
/// SQLite spells each as `x'...'`, two hex digits per byte plus three delimiters.
pub(crate) fn with_blob_bindings<R>(
    sql: &'static str,
    lengths: impl IntoIterator<Item = usize>,
    work: impl FnOnce() -> R,
) -> R {
    let (_value_bytes, literal_bytes) = lengths
        .into_iter()
        .fold((0_u64, 0_u64), |(value, literal), len| {
            (value + len as u64, literal + 2 * len as u64 + 3)
        });
    let bindings = BlobBindings {
        sql,
        #[cfg(feature = "perf-witness")]
        value_bytes: _value_bytes,
        literal_bytes,
        #[cfg(feature = "perf-witness")]
        expanded_bytes: sql.len() - placeholder_bytes(sql) + literal_bytes as usize,
    };
    struct Restore(Option<BlobBindings>);
    impl Drop for Restore {
        fn drop(&mut self) {
            BLOB_BINDINGS.with(|current| current.set(self.0));
        }
    }
    let _restore = Restore(BLOB_BINDINGS.with(|current| current.replace(Some(bindings))));
    work()
}

/// What one connection call wrote, for the performance witness: the rows its
/// statements changed and the pages its transactions wrote to the WAL.
#[cfg(feature = "perf-witness")]
pub(crate) struct WriteWitness {
    changes: u64,
}

#[cfg(feature = "perf-witness")]
impl WriteWitness {
    pub(crate) fn start(connection: &rusqlite::Connection) -> Self {
        // Pages written before this call belong to no measured caller.
        let _ = pages_written(connection);
        Self {
            changes: connection.total_changes(),
        }
    }

    /// Record the rows changed since the witness started or last recorded:
    /// a batch member's own rows (FIG-5968).
    pub(crate) fn member(&mut self, connection: &rusqlite::Connection) {
        joined::flush(connection);
        let changes = connection.total_changes();
        lash_core_ids::perf_witness::record_sql_writes(changes.saturating_sub(self.changes), 0);
        self.changes = changes;
    }

    pub(crate) fn finish(self, connection: &rusqlite::Connection) {
        joined::flush(connection);
        lash_core_ids::perf_witness::record_sql_writes(
            connection.total_changes().saturating_sub(self.changes),
            pages_written(connection),
        );
    }
}

/// The pages `connection` wrote to the WAL since the last read, which resets
/// the count (`SQLITE_DBSTATUS_CACHE_WRITE`).
#[cfg(feature = "perf-witness")]
fn pages_written(connection: &rusqlite::Connection) -> u64 {
    let mut current = 0;
    let mut highwater = 0;
    // SAFETY: the handle is this live connection's, used on its own thread
    // for one status read that neither retains nor closes it.
    #[expect(
        unsafe_code,
        reason = "rusqlite exposes no safe sqlite3_db_status; perf-witness builds only"
    )]
    let status = unsafe {
        rusqlite::ffi::sqlite3_db_status(
            connection.handle(),
            rusqlite::ffi::SQLITE_DBSTATUS_CACHE_WRITE,
            &mut current,
            &mut highwater,
            1,
        )
    };
    if status == rusqlite::ffi::SQLITE_OK {
        u64::try_from(current).unwrap_or_default()
    } else {
        0
    }
}

#[expect(
    unsafe_code,
    reason = "all callback pointers are borrowed from SQLite until this callback returns"
)]
unsafe extern "C" fn trace(
    code: c_uint,
    _: *mut c_void,
    ptr: *mut c_void,
    detail: *mut c_void,
) -> c_int {
    // Never unwind across SQLite's C boundary.
    let _ = std::panic::catch_unwind(|| unsafe {
        let statement = ptr.cast::<ffi::sqlite3_stmt>();
        let sql_ptr = ffi::sqlite3_sql(statement);
        if sql_ptr.is_null() {
            return;
        }
        let raw_sql = CStr::from_ptr(sql_ptr);
        // SQLite retains the UTF-8 SQL supplied by rusqlite. Validate it in
        // one pass; use lossy decoding only for an invalid external string.
        // Every callback still feeds the same observers and counters below.
        let sql = match raw_sql.to_str() {
            Ok(sql) => std::borrow::Cow::Borrowed(sql),
            Err(_) => raw_sql.to_string_lossy(),
        };
        let window = Window::current();
        match code {
            ffi::SQLITE_TRACE_STMT => {
                if !detail.is_null() {
                    let expanded_trace = CStr::from_ptr(detail.cast()).to_bytes();
                    // Trigger trace entries are synthetic comments on the
                    // outer statement. A real SQL statement may start with a
                    // comment too, and must still have one start receipt.
                    if expanded_trace.starts_with(b"--")
                        && expanded_trace != CStr::from_ptr(sql_ptr).to_bytes()
                    {
                        return;
                    }
                }
                #[cfg(feature = "perf-witness")]
                joined::start(statement, &sql);
                #[cfg(feature = "perf-witness")]
                if lash_core_ids::perf_witness::history::enabled() {
                    HISTORY_BEFORE.with(|before| {
                        before
                            .borrow_mut()
                            .entry(ptr.addr())
                            .or_insert_with(|| counters(statement));
                    });
                }
                if let Some(window) = &window {
                    #[cfg(feature = "perf-witness")]
                    let mut binding = None;
                    window.statement(&sql);
                    if let Some(known) = blob_bindings(&sql) {
                        window.payload(&sql, 0, known.literal_bytes, 0);
                        #[cfg(feature = "perf-witness")]
                        {
                            binding = Some(BindingBytes {
                                expanded: known.expanded_bytes,
                                value: Some(known.value_bytes),
                            });
                        }
                    } else {
                        let expanded = expanded_sql(statement);
                        if !expanded.is_null() {
                            let expanded_bytes = CStr::from_ptr(expanded).to_bytes();
                            let bytes = expanded_bytes.len();
                            #[cfg(feature = "perf-witness")]
                            if lash_core_ids::perf_witness::collecting() {
                                let value = (ffi::sqlite3_stmt_readonly(statement) == 0
                                    && ffi::sqlite3_bind_parameter_count(statement) > 0)
                                    .then(|| bound_value_bytes(&sql, expanded_bytes));
                                binding = Some(BindingBytes {
                                    expanded: bytes,
                                    value,
                                });
                            }
                            window.payload(
                                &sql,
                                0,
                                (bytes + placeholder_bytes(&sql)).saturating_sub(sql.len()) as u64,
                                0,
                            );
                            ffi::sqlite3_free(expanded.cast());
                        } else {
                            window.unmeasured_bound(&sql);
                        }
                    }
                    BEFORE.with(|before| {
                        before.borrow_mut().insert(
                            ptr.addr(),
                            Before {
                                work: counters(statement),
                                changes: ffi::sqlite3_total_changes64(ffi::sqlite3_db_handle(
                                    statement,
                                )),
                                #[cfg(feature = "perf-witness")]
                                binding,
                            },
                        )
                    });
                }
            }
            ffi::SQLITE_TRACE_ROW => {
                #[cfg(feature = "perf-witness")]
                joined::row(statement);
                #[cfg(feature = "perf-witness")]
                lash_core_ids::perf_witness::history::sql(&sql, 0, 0, 1);
                if let Some(window) = &window {
                    window.row(&sql);
                    let bytes = (0..ffi::sqlite3_column_count(statement))
                        .map(|column| match ffi::sqlite3_column_type(statement, column) {
                            ffi::SQLITE_INTEGER | ffi::SQLITE_FLOAT => 8,
                            ffi::SQLITE_TEXT | ffi::SQLITE_BLOB => {
                                ffi::sqlite3_column_bytes(statement, column) as u64
                            }
                            _ => 0,
                        })
                        .sum();
                    window.payload(&sql, 0, 0, bytes);
                }
                #[cfg(feature = "perf-witness")]
                lash_core_ids::perf_witness::record_sql_row_read(&sql);
            }
            ffi::SQLITE_TRACE_PROFILE => {
                #[cfg(feature = "perf-witness")]
                joined::profile(statement);
                #[cfg(feature = "perf-witness")]
                if let Some(before) =
                    HISTORY_BEFORE.with(|before| before.borrow_mut().remove(&ptr.addr()))
                {
                    lash_core_ids::perf_witness::history::sql(
                        &sql,
                        1,
                        counters(statement).difference(before).vm_steps,
                        0,
                    );
                }
                let before = BEFORE.with(|before| before.borrow_mut().remove(&ptr.addr()));
                if let Some(before) = &before
                    && let Some(window) = &window
                {
                    window.work(&sql, counters(statement).difference(before.work));
                    let affected = ffi::sqlite3_total_changes64(ffi::sqlite3_db_handle(statement))
                        - before.changes;
                    window.payload(&sql, affected.max(0) as u64, 0, 0);
                }
                #[cfg(feature = "perf-witness")]
                let mut binding = before.and_then(|before| before.binding).or_else(|| {
                    blob_bindings(&sql).map(|known| BindingBytes {
                        expanded: known.expanded_bytes,
                        value: Some(known.value_bytes),
                    })
                });
                #[cfg(feature = "perf-witness")]
                if lash_core_ids::perf_witness::collecting()
                    && ffi::sqlite3_stmt_readonly(statement) == 0
                    && ffi::sqlite3_bind_parameter_count(statement) > 0
                {
                    let value = binding
                        .as_ref()
                        .and_then(|binding| binding.value)
                        .or_else(|| {
                            let expanded = expanded_sql(statement);
                            if expanded.is_null() {
                                return None;
                            }
                            let bytes = CStr::from_ptr(expanded).to_bytes();
                            let value = bound_value_bytes(&sql, bytes);
                            binding = Some(BindingBytes {
                                expanded: bytes.len(),
                                value: Some(value),
                            });
                            ffi::sqlite3_free(expanded.cast());
                            Some(value)
                        });
                    if let Some(value) = value {
                        lash_core_ids::perf_witness::record_sql_bytes_written(value);
                    }
                }
                #[cfg(feature = "perf-witness")]
                if lash_core_ids::perf_witness::sql_receipts_enabled() {
                    let bytes = binding.map_or_else(
                        || {
                            let expanded = expanded_sql(statement);
                            if expanded.is_null() {
                                return 0;
                            }
                            let bytes = CStr::from_ptr(expanded).to_bytes().len();
                            ffi::sqlite3_free(expanded.cast());
                            bytes
                        },
                        |binding| binding.expanded,
                    );
                    lash_core_ids::perf_witness::record_sql_statement_bytes(&sql, bytes);
                } else {
                    lash_core_ids::perf_witness::record_sql_statement(&sql);
                }
            }
            _ => {}
        }
    });
    0
}

/// Count placeholder spelling in the original SQL, excluding quoted text and
/// comments. Expanded SQL changes only these spans; its delta gives actual
/// bound SQL literal bytes, including quoting/hex expansion and repeated uses.
fn placeholder_bytes(sql: &str) -> usize {
    placeholders(sql).map(|span| span.len()).sum()
}

/// The bytes of the values bound to `sql`, read from its `expanded` text:
/// a text's own bytes (not its quoting), a blob's bytes (not its hex
/// spelling), eight for a number and none for NULL. Every use of a repeated
/// parameter counts. Unlike WAL pages, this does not depend on where the
/// rows land in the B-trees, so the same writes count the same every run.
#[cfg(feature = "perf-witness")]
fn bound_value_bytes(sql: &str, expanded: &[u8]) -> u64 {
    let mut total = 0;
    // Expanded text is the original with each placeholder spelled as its
    // literal, so text between placeholders is identical in both.
    let mut original = 0;
    let mut at = 0;
    for span in placeholders(sql) {
        at += span.start - original;
        original = span.end;
        let Some(rest) = expanded.get(at..) else {
            break;
        };
        let (spelled, value) = literal(rest);
        at += spelled;
        total += value;
    }
    total
}

/// The spelling length and value bytes of the SQL literal `text` starts
/// with, as `sqlite3_expanded_sql` writes it.
#[cfg(feature = "perf-witness")]
fn literal(text: &[u8]) -> (usize, u64) {
    match text {
        [b'x', b'\'', hex @ ..] => {
            let digits = hex.iter().take_while(|byte| **byte != b'\'').count();
            (digits + 3, (digits / 2) as u64)
        }
        [b'\'', ..] => {
            let mut index = 1;
            let mut value = 0;
            while index < text.len() {
                if text[index] == b'\'' {
                    if text.get(index + 1) != Some(&b'\'') {
                        index += 1;
                        break;
                    }
                    index += 1;
                }
                value += 1;
                index += 1;
            }
            (index, value)
        }
        [b'N', b'U', b'L', b'L', ..] => (4, 0),
        _ => {
            let spelled = text
                .iter()
                .take_while(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-')
                })
                .count();
            (spelled, 8)
        }
    }
}

/// The byte spans of `sql`'s parameter placeholders, skipping quoted text
/// and comments.
fn placeholders(sql: &str) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
    let bytes = sql.as_bytes();
    let mut index = 0;
    std::iter::from_fn(move || {
        while index < bytes.len() {
            let start = index;
            match bytes[index] {
                b'\'' | b'"' | b'`' | b'[' => {
                    let quote = if bytes[index] == b'[' {
                        b']'
                    } else {
                        bytes[index]
                    };
                    index += 1;
                    while index < bytes.len() {
                        if bytes[index] == quote {
                            index += 1;
                            if bytes.get(index) != Some(&quote) {
                                break;
                            }
                        }
                        index += 1;
                    }
                }
                b'-' if bytes.get(index + 1) == Some(&b'-') => {
                    while index < bytes.len() && bytes[index] != b'\n' {
                        index += 1;
                    }
                }
                b'/' if bytes.get(index + 1) == Some(&b'*') => {
                    index += 2;
                    while index + 1 < bytes.len() && &bytes[index..index + 2] != b"*/" {
                        index += 1;
                    }
                    index = (index + 2).min(bytes.len());
                }
                b'?' | b':' | b'@' | b'$' => {
                    index += 1;
                    while bytes
                        .get(index)
                        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    {
                        index += 1;
                    }
                    return Some(start..index);
                }
                _ => index += 1,
            }
        }
        None
    })
}

/// Origin tags exist only in performance-witness builds; ordinary store calls
/// inline directly to their body with no observer state.
#[inline]
pub(crate) fn with_origin<T>(origin: &'static str, work: impl FnOnce() -> T) -> T {
    #[cfg(feature = "perf-witness")]
    {
        lash_core_ids::perf_witness::sql_shapes::with_origin(origin, || {
            crate::write_observer::scope(origin, work)
        })
    }
    #[cfg(not(feature = "perf-witness"))]
    {
        let _ = origin;
        work()
    }
}

/// The physical observer's boundary encloses COMMIT and aggregate accounting.
#[inline]
pub(crate) fn physical_scope<T>(origin: &'static str, work: impl FnOnce() -> T) -> T {
    #[cfg(feature = "perf-witness")]
    {
        crate::write_observer::scope(origin, work)
    }
    #[cfg(not(feature = "perf-witness"))]
    {
        let _ = origin;
        work()
    }
}

pub(crate) fn open_connection(
    path: impl AsRef<std::path::Path>,
    flags: rusqlite::OpenFlags,
) -> rusqlite::Result<rusqlite::Connection> {
    #[cfg(feature = "perf-witness")]
    {
        crate::write_observer::open(path, flags)
    }
    #[cfg(not(feature = "perf-witness"))]
    {
        rusqlite::Connection::open_with_flags(path, flags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SQL-PAYLOAD-ONCE: the physical window and write witness share one
    /// expansion for unknown bindings; known graph blob lengths need none
    /// (FIG-7506). Both retain their distinct spelling/value byte counters.
    #[cfg(feature = "perf-witness")]
    #[expect(
        clippy::disallowed_methods,
        reason = "isolates the process-global witness from parallel store laws"
    )]
    #[test]
    fn sql_payload_witness_formats_bound_values_once() {
        const CHILD: &str = "LASH_SQL_PAYLOAD_ONCE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "observed_sql::tests::sql_payload_witness_formats_bound_values_once",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            assert!(
                output.status.success(),
                "isolated payload law failed: {stdout}"
            );
            assert!(
                stdout.contains("test result: ok. 1 passed; 0 failed;"),
                "law did not execute: {stdout}"
            );
            return;
        }
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute("CREATE TABLE payload(text TEXT)", [])
            .unwrap();
        connection
            .execute("CREATE TABLE blob_payload(a BLOB,b BLOB)", [])
            .unwrap();
        enable(&connection);
        let value = "é'a".repeat(4096);
        for receipts in [false, true] {
            for observed in [false, true] {
                let collector = if receipts {
                    lash_core_ids::perf_witness::Collector::install_with_sql_receipts()
                } else {
                    lash_core_ids::perf_witness::Collector::install()
                }
                .unwrap();
                let window = Window::new("payload-once", "sqlite");
                let before = EXPANSIONS.with(std::cell::Cell::get);
                Window::on_worker(observed.then(|| window.clone()), || {
                    connection
                        .execute("INSERT INTO payload VALUES (?1)", [&value])
                        .unwrap();
                });
                let snapshot = collector.snapshot();
                assert_eq!(snapshot.sql_bytes_written, value.len() as u64);
                if observed {
                    assert_eq!(
                        window.snapshot().bound_bytes,
                        (value.len() + 4096 + 2) as u64
                    );
                }
                if receipts {
                    assert_eq!(snapshot.sql_receipts.len(), 1);
                    assert_eq!(
                        snapshot.sql_receipts[0].expanded_bytes,
                        "INSERT INTO payload VALUES ()".len() + value.len() + 4096 + 2
                    );
                }
                assert_eq!(
                    EXPANSIONS.with(std::cell::Cell::get) - before,
                    1,
                    "one write must share its immutable expansion: receipts={receipts}, window={observed}"
                );
                const BLOB_SQL: &str = "INSERT INTO blob_payload VALUES (?1,?2)";
                let blob_window = Window::new("known-blob-payload", "sqlite");
                let before = EXPANSIONS.with(std::cell::Cell::get);
                Window::on_worker(observed.then(|| blob_window.clone()), || {
                    with_blob_bindings(BLOB_SQL, [3, 5], || {
                        connection
                            .execute(
                                BLOB_SQL,
                                rusqlite::params![&[0_u8, 39, 255][..], &[1_u8, 2, 3, 4, 5][..]],
                            )
                            .unwrap();
                    });
                });
                let snapshot = collector.snapshot();
                assert_eq!(snapshot.sql_bytes_written, value.len() as u64 + 8);
                if observed {
                    assert_eq!(blob_window.snapshot().bound_bytes, 22);
                }
                if receipts {
                    assert_eq!(snapshot.sql_receipts.len(), 2);
                    assert_eq!(
                        snapshot.sql_receipts[1].expanded_bytes,
                        "INSERT INTO blob_payload VALUES (,)".len() + 22
                    );
                }
                assert_eq!(
                    EXPANSIONS.with(std::cell::Cell::get),
                    before,
                    "known blobs need no expansion: receipts={receipts}, window={observed}"
                );
            }
        }
    }

    /// SQL-COST-JOIN (FIG-6621): one join is one statement, with its
    /// complete row/byte cost; exact repeats exclude changed parameters.
    #[expect(
        clippy::disallowed_methods,
        reason = "isolates the exclusive process-global witness from parallel store laws"
    )]
    #[cfg(feature = "perf-witness")]
    #[test]
    fn turn_sql_work_joins_payload_and_exact_reads_without_table_fanout() {
        const CHILD: &str = "LASH_SQL_WORK_SQLITE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "observed_sql::tests::turn_sql_work_joins_payload_and_exact_reads_without_table_fanout", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            assert!(output.status.success(), "isolated law failed: {stdout}");
            assert!(
                stdout.contains("test result: ok. 1 passed; 0 failed;"),
                "law did not execute: {stdout}"
            );
            return;
        }

        use lash_core_ids::perf_witness::Collector;
        use lash_core_ids::perf_witness::sql_shapes;
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE a(id INTEGER); CREATE TABLE b(id INTEGER); INSERT INTO a VALUES(1); INSERT INTO b VALUES(1)").unwrap();
        connection.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE child(parent INTEGER REFERENCES parent(id) ON DELETE CASCADE); INSERT INTO parent VALUES(1); INSERT INTO child VALUES(1),(1)").unwrap();
        enable(&connection);
        let collector = Collector::install_with_sql_shapes().unwrap();
        sql_shapes::work::set_window(Some(1), "turn");
        sql_shapes::with_origin("join-law", || {
            for value in [1_i64, 1, 2] {
                let rows = connection
                    .prepare_cached("SELECT a.id FROM a JOIN b ON a.id=b.id WHERE a.id=?1")
                    .unwrap()
                    .query_map([value], |r| r.get::<_, i64>(0))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                assert_eq!(rows.len(), usize::from(value == 1));
            }
        });
        joined::flush(&connection);
        let receipt = serde_json::to_value(collector.snapshot().sql_shapes.unwrap()).unwrap();
        assert_eq!(receipt["work"]["totals"]["sqlite"]["statements"], 3);
        assert_eq!(receipt["work"]["totals"]["sqlite"]["rows_returned"], 2);
        assert_eq!(receipt["work"]["totals"]["sqlite"]["result_bytes"], 16);
        assert_eq!(receipt["work"]["totals"]["sqlite"]["bound_bytes"], 3);
        assert_eq!(
            receipt["work"]["repeated_reads"].as_array().unwrap().len(),
            1
        );
        assert_eq!(receipt["work"]["repeated_reads"][0]["executions"], 2);
        assert_eq!(receipt["work"]["rows"].as_array().unwrap().len(), 1);
        assert_eq!(
            receipt["work"]["rows"][0]["relations"],
            serde_json::json!(["a", "b"])
        );
        // PROFILE precedes the truncate optimization's final total_changes.
        connection.execute("DELETE FROM a", []).unwrap();
        joined::flush(&connection);
        let work = collector.snapshot().sql_shapes.unwrap().work;
        assert!(work.complete && work.conserves());
        assert_eq!(
            work.totals[&lash_core_ids::perf_witness::sql_shapes::work::Backend::Sqlite]
                .rows_affected,
            1
        );
        assert_eq!(
            work.totals[&lash_core_ids::perf_witness::sql_shapes::work::Backend::Sqlite].statements,
            4
        );
        // REAL's 15-digit expanded spelling cannot prove parameter equality.
        for value in [1.234567890123456_f64, 1.234567890123457] {
            connection
                .query_row("SELECT ?1", [value], |_| Ok(()))
                .unwrap();
        }
        // TEXT is retained under the explicitly named embedded-NUL qualifier.
        for _ in 0..2 {
            connection
                .query_row("SELECT ?1", ["é'a"], |_| Ok(()))
                .unwrap();
        }
        joined::flush(&connection);
        let work = collector.snapshot().sql_shapes.unwrap().work;
        assert_eq!(
            work.totals[&lash_core_ids::perf_witness::sql_shapes::work::Backend::Sqlite]
                .unqualified_read_identities,
            2
        );
        assert_eq!(work.read_bind_classes["real_unqualified"], 2);
        assert!(
            work.repeated_reads
                .iter()
                .any(|r| r.key.bind_class == "text_modulo_embedded_nul" && r.executions == 2)
        );
        assert!(
            !work
                .repeated_reads
                .iter()
                .any(|r| r.key.bind_class.contains("real"))
        );
        // FK subprograms emit STMT again on the same live handle. Their
        // starts are not new invocations; one PROFILE completes the delete.
        let before = work.statements();
        let rows = connection
            .prepare("DELETE FROM parent WHERE id=1 RETURNING id")
            .unwrap()
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows, vec![1]);
        joined::flush(&connection);
        let work = collector.snapshot().sql_shapes.unwrap().work;
        assert!(
            work.complete && work.conserves(),
            "FK trace subprogram invented active work"
        );
        assert_eq!(work.statements() - before, 1);
        assert_eq!(
            work.totals[&sql_shapes::work::Backend::Sqlite].rows_affected,
            4
        );
    }

    /// REPLAY-SQL-SHAPES: an extra physical SELECT is conserved in the
    /// turn counters and exactly one named store-origin row, on either executor.
    #[cfg(feature = "perf-witness")]
    #[expect(
        clippy::disallowed_methods,
        reason = "test-only process isolation keeps a global witness independent of parallel store laws"
    )]
    #[tokio::test]
    async fn turn_sql_shapes_conserve_an_injected_select_on_connection_workers() {
        const CHILD: &str = "LASH_FIG6400_SQL_SHAPES_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "observed_sql::tests::turn_sql_shapes_conserve_an_injected_select_on_connection_workers",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            assert!(output.status.success(), "isolated SQL law failed: {stdout}");
            assert!(
                stdout.contains("test result: ok. 1 passed; 0 failed;"),
                "isolated law did not execute: {stdout}"
            );
            return;
        }
        use lash_core_ids::perf_witness::Collector;
        async fn injected_store_method(store: &crate::SqliteStore, extra: bool) {
            store
                .conn
                .call(move |conn| {
                    conn.query_row("SELECT 41 AS baseline", [], |_| Ok(()))?;
                    if extra {
                        conn.query_row("SELECT 42 AS injected", [], |_| Ok(()))?;
                    }
                    Ok(())
                })
                .await
                .unwrap();
        }
        for inline in [true, false] {
            let mut options = crate::SqliteStoreSetOptions::memory();
            options.inline_calls = inline;
            let stores = crate::SqliteStoreSet::memory_with_options_and_clock(
                options,
                std::sync::Arc::new(lash_core_ids::clock::SystemClock),
            )
            .await
            .unwrap();
            let collector = Collector::install_with_sql_shapes().unwrap();
            let store = stores.session_store_factory();
            injected_store_method(&store, false).await;
            let before = collector.snapshot();
            injected_store_method(&store, true).await;
            let after = collector.snapshot();
            assert_eq!(after.sql_statements - before.sql_statements, 2);
            assert_eq!(
                after.sql_statements_by_verb["select"] - before.sql_statements_by_verb["select"],
                2
            );
            assert_eq!(after.sql_rows_read - before.sql_rows_read, 2);
            let shapes = after.sql_shapes.unwrap();
            assert_eq!(
                shapes.rows.iter().map(|r| r.counts.selects).sum::<u64>(),
                after.sql_statements_by_verb["select"]
            );
            assert_eq!(
                shapes
                    .rows
                    .iter()
                    .map(|r| r.counts.rows_returned)
                    .sum::<u64>(),
                after.sql_rows_read
            );
            let injected: Vec<_> = shapes
                .rows
                .iter()
                .filter(|r| r.shape == "select ? as injected")
                .collect();
            assert_eq!(injected.len(), 1);
            assert!(injected[0].store_origin.contains("injected_store_method"));
            assert_eq!(injected[0].counts.selects, 1);
            assert_eq!(injected[0].counts.rows_returned, 1);
            eprintln!(
                "REPLAY_SQL_SHAPES inline={inline} {}",
                serde_json::to_string(&shapes).unwrap()
            );
        }
    }

    /// SQL-PAYLOAD: counts writes separately from returned rows, keeps result
    /// payload distinct from SQLite's expanded bind spelling, and cached
    /// re-execution cannot inherit a previous statement's changes.
    #[test]
    fn sql_payload_counts_actual_rows_and_cached_execution_bytes() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute(
                "CREATE TABLE sample (text TEXT, bytes BLOB, number INTEGER)",
                [],
            )
            .unwrap();
        enable(&connection);
        for iteration in 0..2 {
            let window = Window::new(format!("payload-{iteration}"), "sqlite");
            Window::on_worker(Some(window.clone()), || {
                connection
                    .prepare_cached("INSERT INTO sample VALUES (?1, ?2, ?3)")
                    .unwrap()
                    .execute(rusqlite::params!["é'a", &[0_u8, 1, 2][..], 42_i64])
                    .unwrap();
                connection
                    .prepare_cached("-- a real statement, not a trigger\nSELECT text, bytes, number FROM sample LIMIT 1")
                    .unwrap()
                    .query_row([], |_| Ok(()))
                    .unwrap();
            });
            let receipt = window.snapshot();
            assert_eq!(receipt.statements, 2);
            assert_eq!(receipt.rows_affected, 1);
            assert_eq!(receipt.rows_returned, 1);
            assert_eq!(receipt.unmeasured_bound_statements, 0);
            assert_eq!(receipt.bound_bytes, 18); // 'é''a', x'000102', 42
            assert_eq!(receipt.result_bytes, 15); // 4 UTF-8 + 3 blob + 8 integer
            assert_eq!(
                receipt
                    .shapes
                    .values()
                    .map(|s| s.rows_affected)
                    .sum::<u64>(),
                1
            );
            eprintln!("SQL_PAYLOAD {}", serde_json::to_string(&receipt).unwrap());
        }
    }

    /// SQL-BYTES-WRITTEN: a write counts the bytes of the values it binds,
    /// not their SQL spelling: a text without its quoting, a blob without its
    /// hex, eight per number, none for NULL, and every use of a repeated
    /// parameter; placeholders inside comments and quoted text are not
    /// parameters. The replay gate's depth slopes bind this count because it
    /// does not move with B-tree page layout (FIG-6280).
    #[cfg(feature = "perf-witness")]
    #[test]
    fn bound_value_bytes_count_values_not_their_spelling() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute("CREATE TABLE sample (a, b, c, d, e, f)", [])
            .unwrap();
        let sql = "INSERT INTO sample VALUES (?1, :blob, ?3, -- ?9\n ?4, '?', ?1)";
        let mut statement = connection.prepare(sql).unwrap();
        statement.raw_bind_parameter(1, "it's é").unwrap();
        statement
            .raw_bind_parameter(2, &[0_u8, 1, 2, 3, 4][..])
            .unwrap();
        statement.raw_bind_parameter(3, -12.5_f64).unwrap();
        statement
            .raw_bind_parameter(4, rusqlite::types::Null)
            .unwrap();
        let expanded = statement.expanded_sql().unwrap();
        // 7 text bytes twice, 5 blob bytes, one number, NULL.
        assert_eq!(bound_value_bytes(sql, expanded.as_bytes()), 7 * 2 + 5 + 8);
    }

    /// GRAPH-INSTALL-WORK: count successful store insertions, including
    /// reinstalls after collection, rather than recorder-global first-seen ids.
    #[tokio::test]
    async fn graph_install_bytes_follow_deduplication_and_collection() {
        use lash_core_store::checkpoint_graph::*;
        use lash_durable::{ActorKey, CommitLabel, DurableStore, Epoch};
        let stores = crate::SqliteStoreSet::memory().await.unwrap();
        let identity = lash_core_execution::testing::sql_work::owner_commit(&stores).await;
        let store = stores.durable_store();
        let actor = ActorKey::session("sql-work-owner").unwrap();
        let mut graph = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(1 << 20)));
        let root = graph
            .encode_value(NodeKind::TEXT_LEAF, &"install witness".to_owned())
            .unwrap();
        let bytes = graph.nodes()[&root].bytes.len() as u64;
        for (sample, expected) in [bytes, 0, bytes].into_iter().enumerate() {
            if sample == 2 {
                store.audit_orphans().await.unwrap();
            }
            let mut tx = store.begin(&actor, Epoch(identity.epoch)).await.unwrap();
            tx.stage(graph.nodes().values().cloned());
            let (answer, receipt) = lash_core_ids::operational_metrics::sql::collect(
                format!("install-{sample}"),
                "sqlite",
                store.commit(tx, CommitLabel::new("sql-work.install")),
            )
            .await;
            answer.unwrap();
            assert_eq!(receipt.newly_installed_graph_bytes, expected);
            eprintln!("SQL_INSTALL {}", serde_json::to_string(&receipt).unwrap());
        }
    }

    /// SQL-WORK: all five windows export statements, normalized shapes,
    /// returned rows and SQL VM work, separately from logical graph writes.
    #[tokio::test]
    async fn sql_work_receipts_cover_two_history_sizes() {
        let stores = crate::SqliteStoreSet::memory()
            .await
            .expect("SQLite stores");
        let receipts = lash_core_execution::testing::sql_work::receipts(&stores, "sqlite").await;
        assert_eq!(receipts.len(), 10);
        for receipt in receipts {
            assert!(receipt.sqlite_work.expect("SQLite work").vm_steps > 0);
            eprintln!(
                "SQL_WORK {}",
                serde_json::to_string(&receipt).expect("receipt")
            );
        }
    }
    /// SQL-OWNER: production summaries carry the retained actor fence and
    /// caller operation identity, independent of logical transition metrics.
    #[tokio::test]
    async fn physical_summary_joins_the_owner_commit_and_caller_window() {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = captured.clone();
        let observer = lash_core_ids::operational_metrics::StoreObserver::default().with_sql_sink(
            move |receipt, outcome| {
                if receipt.operation == "sql-work.owner" {
                    sink.lock().expect("sink").push((receipt.clone(), outcome));
                }
            },
        );
        let mut options = crate::SqliteStoreSetOptions::memory();
        options.observer = observer;
        let stores = crate::SqliteStoreSet::memory_with_options_and_clock(
            options,
            std::sync::Arc::new(lash_core_ids::clock::SystemClock),
        )
        .await
        .expect("stores");
        let (identity, outer) = lash_core_ids::operational_metrics::sql::collect(
            "caller/session/turn/record",
            "sqlite",
            lash_core_execution::testing::sql_work::owner_commit(&stores),
        )
        .await;
        let receipts = captured.lock().expect("receipts");
        assert_eq!(receipts.len(), 1);
        let (receipt, outcome) = &receipts[0];
        assert_eq!(*outcome, "success");
        assert_eq!(receipt.owner.as_ref(), Some(&identity));
        assert_eq!(
            receipt.parent_operation.as_deref(),
            Some(outer.operation.as_str())
        );
        assert!(receipt.statements > 0);
        eprintln!(
            "SQL_WORK_OWNER {}",
            serde_json::to_string(receipt).expect("receipt")
        );
    }
}
