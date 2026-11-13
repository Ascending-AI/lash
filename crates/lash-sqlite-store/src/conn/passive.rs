//! Existing-store reads without journal creation. A normal read-only SQLite
//! opener can create WAL/SHM files. Hold SQLite's own database-file lock while
//! selecting a mode: existing WAL uses read-only shared memory; no WAL uses an
//! immutable view under an exclusive lock, so it cannot race a new writer.
#![expect(
    unsafe_code,
    reason = "SQLite exposes its database-file locks only through the VFS callbacks"
)]

use super::DatabaseTarget;
use rusqlite::{Connection, OpenFlags, ffi};

pub(super) fn open(target: &DatabaseTarget) -> rusqlite::Result<(Connection, Connection)> {
    let path = target.file_path().ok_or(rusqlite::Error::InvalidQuery)?;
    if !path.try_exists().map_err(io_error)? {
        return Err(io_error(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("existing SQLite store does not exist: {}", path.display()),
        )));
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    // No CREATE, pragmas, transactions, schema admission or journal recovery.
    // This handle supplies locks only. It never executes SQL.
    let guard = Connection::open_with_flags(target.uri(), flags)?;
    lock(&guard, ffi::SQLITE_LOCK_SHARED)?;
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let wal_present = std::path::Path::new(&wal).try_exists().map_err(io_error)?;
    let mut journal = path.as_os_str().to_owned();
    journal.push("-journal");
    if std::path::Path::new(&journal)
        .try_exists()
        .map_err(io_error)?
    {
        return Err(rusqlite::Error::SqliteFailure(
            ffi::Error::new(ffi::SQLITE_READONLY_RECOVERY),
            Some("passive store read requires journal recovery by a writer".to_owned()),
        ));
    }
    let mut uri = target.read_only_uri();
    if wal_present {
        // The shared database lock prevents WAL deletion. readonly_shm
        // refuses an absent or unusable index instead of creating one.
        uri.push_str("&readonly_shm=1");
    } else {
        // A WAL checkpoint need not take an exclusive database lock. A
        // shared lock alone is therefore insufficient for an immutable view.
        // Refuse a concurrent opener rather than bypassing SQLite's locks.
        lock(&guard, ffi::SQLITE_LOCK_EXCLUSIVE)?;
        if std::path::Path::new(&journal)
            .try_exists()
            .map_err(io_error)?
        {
            return Err(rusqlite::Error::SqliteFailure(
                ffi::Error::new(ffi::SQLITE_READONLY_RECOVERY),
                Some("passive store read requires journal recovery by a writer".to_owned()),
            ));
        }
        // A writer could have created a WAL between the first existence
        // check and obtaining the exclusive lock, then closed without being
        // able to remove it while our shared lock was held. Recheck now.
        if std::path::Path::new(&wal).try_exists().map_err(io_error)? {
            uri.push_str("&readonly_shm=1");
        } else {
            uri.push_str("&immutable=1");
        }
    }
    let connection = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    Ok((guard, connection))
}

fn lock(connection: &Connection, level: i32) -> rusqlite::Result<()> {
    // SAFETY: FILE_POINTER borrows the VFS file owned by this open connection.
    // It stays alive through the callback, on this one worker thread. xClose
    // releases these locks when the guard is dropped after the read handle.
    let result = unsafe {
        let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
        let result = ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            ffi::SQLITE_FCNTL_FILE_POINTER,
            (&raw mut file).cast(),
        );
        if result != ffi::SQLITE_OK {
            result
        } else if file.is_null() || (*file).pMethods.is_null() {
            ffi::SQLITE_IOERR_LOCK
        } else {
            (*(*file).pMethods)
                .xLock
                .map_or(ffi::SQLITE_IOERR_LOCK, |lock| lock(file, level))
        }
    };
    if result == ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(rusqlite::Error::SqliteFailure(
            ffi::Error::new(result),
            Some("cannot lock existing store for a passive read".to_owned()),
        ))
    }
}

fn io_error(error: std::io::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An immutable passive read must hold SQLite's lock, rather than ignore
    /// changes made by a concurrent writer, and it must see committed WAL data.
    #[test]
    fn passive_reads_lock_closed_stores_and_read_live_wal_commits() {
        let root = tempfile::tempdir().expect("fixture directory");
        let path = root.path().join("store.db");
        let target = DatabaseTarget::File(path.clone());
        let writer = Connection::open(&path).expect("writer");
        writer
            .execute_batch("CREATE TABLE data(value); INSERT INTO data VALUES(1);")
            .expect("fixture");
        drop(writer);
        {
            let (guard, reader) = open(&target).expect("closed passive open");
            assert_eq!(
                reader
                    .query_row("SELECT value FROM data", [], |r| r.get::<_, i64>(0))
                    .expect("read"),
                1
            );
            let writer = Connection::open(&path).expect("second writer");
            let error = writer
                .execute("UPDATE data SET value=2", [])
                .expect_err("read guards immutable view");
            assert_eq!(
                error.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy)
            );
            drop(writer);
            drop(reader);
            drop(guard);
        }
        let writer = Connection::open(&path).expect("live writer");
        writer
            .execute_batch("PRAGMA journal_mode=WAL; UPDATE data SET value=3;")
            .expect("WAL commit");
        let (guard, reader) = open(&target).expect("live passive open");
        assert_eq!(
            reader
                .query_row("SELECT value FROM data", [], |r| r.get::<_, i64>(0))
                .expect("WAL read"),
            3
        );
        assert!(
            reader.execute("UPDATE data SET value=4", []).is_err(),
            "passive connection cannot write"
        );
        drop(reader);
        drop(guard);
        drop(writer);
    }
}
