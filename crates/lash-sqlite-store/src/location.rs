//! Where a SQLite backend's four databases live, and the one identity they
//! answer to (ADR 0102).
//!
//! A backend is a directory of four database files or a named in-memory
//! backend of four `memdb` databases. [`SqliteLocation`] owns both facts
//! every component needs from that choice: how to reach each database (a path
//! or a `file:/lash-<id>/<db>?vfs=memdb` URI) and the identity the turn-control
//! binding, the settlement notifier and every `ATTACH` are keyed on. No
//! component formats either on its own.
//!
//! A `memdb` database is shared by name across every connection in the
//! process and disappears with its last connection. A memory backend
//! therefore pins each database with one idle anchor connection
//! ([`MemoryAnchors`]), and every component opened on it holds the anchors, so
//! the data lives exactly as long as the backend or any handle taken from
//! it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::SqliteDatabase;

/// The typed location of one SQLite backend.
///
/// The only way to reach a SQLite database in memory: raw `:memory:` and
/// `file:` strings are refused by every path-taking constructor.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SqliteLocation {
    /// Four database files under a canonical directory.
    File { root: PathBuf },
    /// Four named `memdb` databases, alive while the backend is.
    Memory { id: uuid::Uuid },
}

impl SqliteLocation {
    /// A fresh in-memory location nothing else names.
    pub(crate) fn fresh_memory() -> Self {
        Self::Memory {
            id: uuid::Uuid::new_v4(),
        }
    }

    /// The identity every binding this backend writes is keyed on:
    /// `sqlite:<canonical effect-replay.db path>` or `sqlite-memory:<id>`.
    ///
    /// A file backend answers to its effect journal's canonical path
    /// because that is the identity a SQLite effect host has always bound
    /// turn control to (FIG-2971) and sessions have persisted: a file
    /// database written by a host opened on `<root>/effect-replay.db` opens
    /// through the backend with every binding unchanged.
    pub fn identity(&self) -> String {
        match self {
            Self::File { root } => format!(
                "sqlite:{}",
                root.join(SqliteDatabase::EffectReplay.file_name())
                    .display()
            ),
            Self::Memory { id } => format!("sqlite-memory:{id}"),
        }
    }

    /// The URI a raw SQLite connection opens `database` through.
    ///
    /// An inspection affordance: every lash component reaches the database
    /// through the backend, never through this string.
    pub fn database_uri(&self, database: SqliteDatabase) -> String {
        self.target(database).uri()
    }

    /// How a connection reaches `database` in this location.
    pub(crate) fn target(&self, database: SqliteDatabase) -> DatabaseTarget {
        match self {
            Self::File { root } => DatabaseTarget::File(root.join(database.file_name())),
            Self::Memory { id } => {
                DatabaseTarget::Memory(format!("/lash-{id}/{}", database.memory_name()))
            }
        }
    }
}

/// How one connection reaches one database: a file path, or the name of a
/// shared `memdb` database.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DatabaseTarget {
    File(PathBuf),
    /// The `memdb` name, `/lash-<id>/<db>`.
    Memory(String),
}

impl DatabaseTarget {
    /// The database file, when this target is one.
    pub(crate) fn file_path(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path),
            Self::Memory(_) => None,
        }
    }

    /// The name SQLite opens or `ATTACH`es: the path of a file, the URI of a
    /// `memdb` database. Every connection opens with `SQLITE_OPEN_URI`, so a
    /// `file:` name is read as a URI by both.
    pub(crate) fn open_name(&self) -> String {
        match self {
            Self::File(path) => path.to_string_lossy().into_owned(),
            Self::Memory(name) => format!("file:{name}?vfs=memdb"),
        }
    }

    /// The URI form of [`Self::open_name`], for a caller that adds its own
    /// query parameters.
    pub(crate) fn uri(&self) -> String {
        match self {
            Self::File(path) => format!("file:{}", escape_uri_path(&path.to_string_lossy())),
            Self::Memory(_) => self.open_name(),
        }
    }

    /// The URI a read-only connection opens.
    pub(crate) fn read_only_uri(&self) -> String {
        match self {
            Self::File(_) => format!("{}?mode=ro", self.uri()),
            Self::Memory(_) => format!("{}&mode=ro", self.uri()),
        }
    }

    /// The name this database is identified by among a backend's
    /// participants: the canonical path of a file, the URI of a `memdb`
    /// database.
    pub(crate) fn canonical_name(&self) -> String {
        match self {
            Self::File(path) => canonical_path(path).display().to_string(),
            Self::Memory(_) => self.open_name(),
        }
    }

    /// Whether the database has been created. A memory target is created by
    /// the backend that names it and lives as long as any handle does.
    pub(crate) fn exists(&self) -> bool {
        match self {
            Self::File(path) => path.exists(),
            Self::Memory(_) => true,
        }
    }
}

impl std::fmt::Display for DatabaseTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => path.display().fmt(formatter),
            Self::Memory(_) => formatter.write_str(&self.open_name()),
        }
    }
}

fn escape_uri_path(path: &str) -> String {
    path.replace('%', "%25")
        .replace('?', "%3F")
        .replace('#', "%23")
}

/// One idle connection per `memdb` database of a memory backend. A
/// `memdb` database disappears with its last connection; these keep all four
/// alive until the last handle holding them drops.
pub(crate) struct MemoryAnchors {
    _connections: Mutex<Vec<rusqlite::Connection>>,
}

impl MemoryAnchors {
    /// Create the four databases of `location` and pin them.
    pub(crate) fn pin(location: &SqliteLocation) -> rusqlite::Result<Arc<Self>> {
        let connections = SqliteDatabase::ALL
            .into_iter()
            .map(|database| rusqlite::Connection::open(location.target(database).open_name()))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Arc::new(Self {
            _connections: Mutex::new(connections),
        }))
    }
}

impl std::fmt::Debug for MemoryAnchors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemoryAnchors")
            .finish_non_exhaustive()
    }
}

/// One database of one backend, as a component opens it: where it is, the
/// identity of the backend it belongs to, and — for a memory backend —
/// the anchors that keep it alive while this handle does.
#[derive(Clone, Debug)]
pub(crate) struct DatabaseLocation {
    target: DatabaseTarget,
    identity: Arc<str>,
    /// Held, never read: dropping the last handle releases the databases.
    _anchors: Option<Arc<MemoryAnchors>>,
}

impl DatabaseLocation {
    /// `database` in `location`, answering to the backend's `identity`
    /// and pinned by `anchors` when in memory.
    pub(crate) fn in_backend(
        location: &SqliteLocation,
        identity: &Arc<str>,
        database: SqliteDatabase,
        anchors: Option<&Arc<MemoryAnchors>>,
    ) -> Self {
        Self {
            target: location.target(database),
            identity: Arc::clone(identity),
            _anchors: anchors.cloned(),
        }
    }

    /// A database file a host opened on its own, outside any backend: the
    /// file is its own location, and its identity is `sqlite:<canonical
    /// path>`, stable across relative spellings and symlinks. A backend's
    /// journal file answers to the same string (see
    /// [`SqliteLocation::identity`]).
    pub(crate) fn standalone_file(path: &Path) -> Self {
        let identity = format!("sqlite:{}", canonical_path(path).display());
        Self {
            target: DatabaseTarget::File(path.to_path_buf()),
            identity: Arc::from(identity),
            _anchors: None,
        }
    }

    pub(crate) fn target(&self) -> &DatabaseTarget {
        &self.target
    }

    pub(crate) fn identity(&self) -> &Arc<str> {
        &self.identity
    }
}

/// Refuse a path-taking constructor a spelling that is not a database file:
/// the empty path, `:memory:`, or a `file:` URI. A private `:memory:`
/// database cannot be reached by a second connection and a URI would bypass
/// the location's identity, so the typed [`SqliteLocation::Memory`] is the one
/// way into memory (FIG-2971).
pub(crate) fn validate_file_database_path(
    path: &Path,
    component: &'static str,
) -> tokio_rusqlite::Result<()> {
    let rendered = path.to_string_lossy();
    if path.as_os_str().is_empty() || rendered == ":memory:" || rendered.starts_with("file:") {
        return Err(tokio_rusqlite::Error::Error(
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                Some(format!(
                    "{component} requires a file-backed database path, got `{rendered}`; \
                     use SqliteBackend::memory() for an in-memory backend"
                )),
            ),
        ));
    }
    Ok(())
}

/// The canonical form of `path`, resolving the longest existing prefix so a
/// file not yet created still has a stable identity.
#[expect(
    clippy::disallowed_methods,
    reason = "a location's identity is the canonical form of the host-supplied path (FIG-2971)"
)]
pub(crate) fn canonical_path(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return absolute;
        };
        missing.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            return absolute;
        };
        existing = parent;
    }
    let mut canonical = std::fs::canonicalize(existing).unwrap_or_else(|_| existing.to_path_buf());
    for component in missing.into_iter().rev() {
        canonical.push(component);
    }
    canonical
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_count(uri: &str) -> i64 {
        rusqlite::Connection::open(uri)
            .expect("open memdb connection")
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'anchored'",
                [],
                |row| row.get(0),
            )
            .expect("read catalog")
    }

    #[test]
    fn memory_data_survives_every_non_anchor_connection_and_dies_with_the_anchors() {
        let location = SqliteLocation::fresh_memory();
        let uri = location.database_uri(SqliteDatabase::DurableCore);
        let anchors = MemoryAnchors::pin(&location).expect("pin memory backend");
        {
            let writer = rusqlite::Connection::open(&uri).expect("open writer");
            writer
                .execute_batch(
                    "CREATE TABLE anchored (value INTEGER); INSERT INTO anchored VALUES (7);",
                )
                .expect("write through a non-anchor connection");
        }
        assert_eq!(
            table_count(&uri),
            1,
            "the anchor keeps the database once every other connection has closed"
        );
        drop(anchors);
        assert_eq!(
            table_count(&uri),
            0,
            "dropping the anchors releases the database"
        );
    }

    #[test]
    fn each_database_of_a_memory_location_is_its_own_named_memdb() {
        let location = SqliteLocation::fresh_memory();
        let SqliteLocation::Memory { id } = &location else {
            unreachable!("fresh_memory is a memory location");
        };
        assert_eq!(location.identity(), format!("sqlite-memory:{id}"));
        assert_eq!(
            location.database_uri(SqliteDatabase::DurableCore),
            format!("file:/lash-{id}/core?vfs=memdb")
        );
        let uris = SqliteDatabase::ALL.map(|database| location.database_uri(database));
        for (index, uri) in uris.iter().enumerate() {
            assert!(!uris[index + 1..].contains(uri), "{uri} is named twice");
        }
    }
}
