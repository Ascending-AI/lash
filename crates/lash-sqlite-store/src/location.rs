//! Where a SQLite store's one database lives, and the one identity it
//! answers to (ADR 0102, ADR 0132 §12).
//!
//! A SQLite deployment is one database file or one named `memdb` database.
//! [`SqliteLocation`] owns both facts every component needs from that
//! choice: how to reach the database (a path or a
//! `file:/lash-<id>/db?vfs=memdb` URI) and the storage binding identity. No
//! component formats either on its own.
//!
//! A `memdb` database is shared by name across every connection in the
//! process and disappears with its last connection. A memory store therefore
//! pins its database with one idle anchor connection ([`MemoryAnchors`]), and
//! every component opened on it holds the anchor, so the data lives exactly
//! as long as the store set or any handle taken from it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The typed location of one SQLite store.
///
/// The only way to reach a SQLite database in memory: raw `:memory:` and
/// `file:` strings are refused by every path-taking constructor.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SqliteLocation {
    /// One database file, at its canonical path.
    File { path: PathBuf },
    /// One named `memdb` database, alive while its anchor is held.
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
    /// `sqlite:<canonical database path>` or `sqlite-memory:<id>`.
    pub fn identity(&self) -> String {
        match self {
            Self::File { path } => format!("sqlite:{}", path.display()),
            Self::Memory { id } => format!("sqlite-memory:{id}"),
        }
    }

    /// The URI a raw SQLite connection opens the database through.
    ///
    /// An inspection affordance: every lash component reaches the database
    /// through the backend, never through this string.
    pub fn database_uri(&self) -> String {
        self.target().uri()
    }

    /// How a connection reaches the database in this location.
    pub(crate) fn target(&self) -> DatabaseTarget {
        match self {
            Self::File { path } => DatabaseTarget::File(path.clone()),
            Self::Memory { id } => DatabaseTarget::Memory(format!("/lash-{id}/db")),
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

/// The idle connection that pins a memory store's `memdb` database, which
/// disappears with its last connection, until the last handle holding the
/// anchor drops.
pub(crate) struct MemoryAnchors {
    _connection: Mutex<rusqlite::Connection>,
}

impl MemoryAnchors {
    /// Create the database of `location` and pin it.
    pub(crate) fn pin(location: &SqliteLocation) -> rusqlite::Result<Arc<Self>> {
        let connection = rusqlite::Connection::open(location.target().open_name())?;
        Ok(Arc::new(Self {
            _connection: Mutex::new(connection),
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

/// A backend's database, as a component opens it: where it is and, for a
/// memory backend, the anchor that keeps it alive while this handle does.
#[derive(Clone, Debug)]
pub(crate) struct DatabaseLocation {
    target: DatabaseTarget,
    /// Held, never read: dropping the last handle releases the database.
    _anchors: Option<Arc<MemoryAnchors>>,
}

impl DatabaseLocation {
    /// The database of `location`, pinned by `anchors` when in memory.
    pub(crate) fn in_backend(
        location: &SqliteLocation,
        anchors: Option<&Arc<MemoryAnchors>>,
    ) -> Self {
        Self {
            target: location.target(),
            _anchors: anchors.cloned(),
        }
    }

    /// A database file a host opened on its own, outside any backend: the
    /// file is its own location.
    pub(crate) fn standalone_file(path: &Path) -> Self {
        Self {
            target: DatabaseTarget::File(path.to_path_buf()),
            _anchors: None,
        }
    }

    pub(crate) fn target(&self) -> &DatabaseTarget {
        &self.target
    }
}

/// The database files of the retired three-file layout (FIG-5195), which a
/// store directory held before a SQLite deployment became one file.
const RETIRED_LAYOUT_FILES: [&str; 3] = ["durable-core.db", "process-registry.db", "triggers.db"];

/// Validate `path` as a store's database file, create the directory it sits
/// in, and answer its canonical location.
///
/// A directory is never a database file. One that holds the retired
/// three-file layout is refused as [`CompatRefusal::RetiredSqliteLayout`]
/// (typed in the error's source chain) and left unchanged.
///
/// [`CompatRefusal::RetiredSqliteLayout`]: lash_core_execution::compat::CompatRefusal::RetiredSqliteLayout
#[expect(
    clippy::disallowed_methods,
    reason = "a file backend creates the directory of the host-supplied database path (FIG-2971)"
)]
pub(crate) fn file_location(
    path: &Path,
    owner: &'static str,
) -> tokio_rusqlite::Result<SqliteLocation> {
    validate_file_database_path(path, owner)?;
    if path.is_dir() {
        let files: Vec<String> = RETIRED_LAYOUT_FILES
            .into_iter()
            .filter(|file| path.join(file).exists())
            .map(str::to_owned)
            .collect();
        if !files.is_empty() {
            return Err(tokio_rusqlite::Error::Error(
                crate::sqlite_conversion_error(lash_core_execution::StoreError::Incompatible {
                    refusal: lash_core_execution::compat::CompatRefusal::RetiredSqliteLayout {
                        location: path.display().to_string(),
                        files,
                    },
                }),
            ));
        }
        return Err(cannot_open(format!(
            "{owner} requires the path of a database file, and {} is a directory",
            path.display()
        )));
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            cannot_open(format!(
                "{owner} could not create the directory of {}: {error}",
                path.display()
            ))
        })?;
    }
    Ok(SqliteLocation::File {
        path: canonical_path(path),
    })
}

fn cannot_open(message: String) -> tokio_rusqlite::Error {
    tokio_rusqlite::Error::Error(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
        Some(message),
    ))
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
        return Err(cannot_open(format!(
            "{component} requires a file-backed database path, got `{rendered}`; \
             use SqliteStoreSet::memory() for a SQLite in-memory store set"
        )));
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
