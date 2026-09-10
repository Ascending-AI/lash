//! Where a scope-retirement fence lives and how the effect journal reaches it
//! (FIG-2499, ADR 0049).
//!
//! The journal file carries its own `effect_scope_retirements` table: the
//! fence of every runtime-operation scope, and of a process scope retired
//! while no registry is bound, is inserted there in the same transaction
//! that deletes the scope's rows. A bound SQLite process registry carries the
//! same table in its own file, and that copy is the fence of every process
//! scope from then on: registration inserts the process row and deletes the
//! fence row in one single-file commit, and retirement's fence insert into
//! that file is its one commit point, the journal purge that follows being an
//! idempotent cleanup. Admission reads both tables.

use std::path::{Path, PathBuf};

use rusqlite::params;

use crate::conn::SqliteConnection;

/// Schema name under which a bound registry file is attached to the journal
/// connection.
pub(crate) const PROCESS_REGISTRY_SCHEMA: &str = "process_registry";

/// The journal's own database, as a schema name.
pub(crate) const JOURNAL_SCHEMA: &str = "main";

/// The fence tables a journal connection consults: its own, and the attached
/// registry's when a registry is bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FenceLocations {
    /// The schema the journal's own fence table is addressed through.
    journal: &'static str,
    /// The attached registry's schema name, `None` when no registry file is
    /// bound (or the registry is this very file, addressed as `main`).
    registry: Option<&'static str>,
}

impl FenceLocations {
    /// The journal alone, addressed as `main`.
    pub(crate) const JOURNAL_ONLY: Self = Self {
        journal: JOURNAL_SCHEMA,
        registry: None,
    };

    /// A journal attached under `journal` with no registry beside it.
    pub(crate) fn journal_only_at(journal: &'static str) -> Self {
        Self {
            journal,
            registry: None,
        }
    }

    /// A journal attached under `journal` beside a registry attached as
    /// [`PROCESS_REGISTRY_SCHEMA`]: the retention sweep's view.
    pub(crate) fn attached(journal: &'static str) -> Self {
        Self {
            journal,
            registry: Some(PROCESS_REGISTRY_SCHEMA),
        }
    }

    /// Every schema holding a fence table, the journal first.
    pub(crate) fn schemas(self) -> impl Iterator<Item = &'static str> {
        std::iter::once(self.journal).chain(self.registry)
    }

    /// The schema a fence for `scope` is written into: the registry's file
    /// for a process scope when a registry is bound, the journal otherwise.
    pub(crate) fn fence_schema_for(self, scope: &lash_core::ExecutionScope) -> &'static str {
        match (scope, self.registry) {
            (lash_core::ExecutionScope::Process { .. }, Some(registry)) => registry,
            _ => self.journal,
        }
    }

    /// Whether the fence for `scope` is written outside the journal file, so
    /// its insert commits on its own ahead of the journal purge.
    pub(crate) fn fence_is_in_registry_file(self, scope: &lash_core::ExecutionScope) -> bool {
        self.fence_schema_for(scope) != self.journal
    }

    /// Whether `scope_id` is fenced in any location.
    pub(crate) fn is_fenced(
        self,
        connection: &rusqlite::Connection,
        scope_id: &str,
    ) -> rusqlite::Result<bool> {
        for schema in self.schemas() {
            let fenced: bool = connection.query_row(
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM {schema}.effect_scope_retirements WHERE scope_id = ?1)"
                ),
                params![scope_id],
                |row| row.get(0),
            )?;
            if fenced {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Delete the fence of `scope_id` everywhere it may be recorded.
    pub(crate) fn lift(
        self,
        connection: &rusqlite::Connection,
        scope_id: &str,
    ) -> rusqlite::Result<()> {
        for schema in self.schemas() {
            connection.execute(
                &format!("DELETE FROM {schema}.effect_scope_retirements WHERE scope_id = ?1"),
                params![scope_id],
            )?;
        }
        Ok(())
    }
}

/// The bound process registry's file, attached to the journal connection on
/// first use so process-scope fences are read from and written to the file
/// whose transaction registers processes.
///
/// `ATTACH` cannot run inside a transaction, so the attach is a separate
/// serialized step ahead of the write that needs it; once attached it stays
/// for the connection's lifetime. A registry that turns out to be the
/// journal file itself is addressed through `main` instead.
#[derive(Default)]
pub(crate) struct RegistryAttachment {
    state: std::sync::Mutex<RegistryAttachmentState>,
    attach: tokio::sync::Mutex<()>,
}

#[derive(Default)]
enum RegistryAttachmentState {
    #[default]
    Unbound,
    Requested(PathBuf),
    Attached {
        path: PathBuf,
        locations: FenceLocations,
    },
}

impl RegistryAttachment {
    /// Record the registry file to attach; a later request naming a
    /// different file than the one already attached is refused with a
    /// warning, because one journal connection reaches one registry.
    pub(crate) fn request(&self, path: PathBuf) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &*state {
            RegistryAttachmentState::Attached { path: attached, .. } if *attached == path => {}
            RegistryAttachmentState::Attached { path: attached, .. } => {
                tracing::warn!(
                    attached = %attached.display(),
                    requested = %path.display(),
                    "effect journal already attached a different process registry; the later \
                     registry's process-scope fences stay in the journal file"
                );
            }
            _ => *state = RegistryAttachmentState::Requested(path),
        }
    }

    /// Attach the requested registry if not yet attached, repairing the
    /// journal against it once, and answer the fence locations to consult.
    pub(crate) async fn ensure_attached(
        &self,
        conn: &SqliteConnection,
    ) -> rusqlite::Result<FenceLocations> {
        let _serialized = self.attach.lock().await;
        let requested = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match &*state {
                RegistryAttachmentState::Unbound => return Ok(FenceLocations::JOURNAL_ONLY),
                RegistryAttachmentState::Attached { locations, .. } => return Ok(*locations),
                RegistryAttachmentState::Requested(path) => path.clone(),
            }
        };
        let path = requested.clone();
        let locations = conn
            .call(move |connection| {
                let main_file: Option<String> = connection
                    .query_row(
                        "SELECT file FROM pragma_database_list WHERE name = 'main'",
                        [],
                        |row| row.get(0),
                    )
                    .ok()
                    .flatten();
                if main_file
                    .as_deref()
                    .is_some_and(|main| !main.is_empty() && Path::new(main) == path)
                {
                    return Ok(FenceLocations::JOURNAL_ONLY);
                }
                connection.execute(
                    &format!("ATTACH DATABASE ?1 AS {PROCESS_REGISTRY_SCHEMA}"),
                    params![path.to_string_lossy().into_owned()],
                )?;
                Ok(FenceLocations::attached(JOURNAL_SCHEMA))
            })
            .await?;
        if locations.registry.is_some() {
            // Bind-time repair (ADR 0049): rows a lost journal purge left under
            // a scope the registry file fences are garbage, and a fence this
            // file wrote for a process scope while no registry was bound is
            // stale once the registry says the process is registered.
            conn.write(move |tx| {
                crate::effect_replay::purge_rows_under_fenced_scopes(
                    tx,
                    JOURNAL_SCHEMA,
                    locations,
                )?;
                lift_journal_fences_of_registered_processes(tx, locations)?;
                Ok(())
            })
            .await?;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *state = RegistryAttachmentState::Attached {
            path: requested,
            locations,
        };
        Ok(locations)
    }
}

/// Delete every journal-file process-scope fence whose process the attached
/// registry has registered: registration is the owner coming back, and a
/// fence written into this file before the registry was bound cannot have
/// been cleared by the registration transaction.
fn lift_journal_fences_of_registered_processes(
    tx: &rusqlite::Transaction<'_>,
    locations: FenceLocations,
) -> rusqlite::Result<usize> {
    let Some(registry) = locations.registry else {
        return Ok(0);
    };
    let fenced: Vec<String> = {
        let mut statement = tx.prepare(&format!(
            "SELECT scope_id FROM {JOURNAL_SCHEMA}.effect_scope_retirements"
        ))?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut lifted = 0;
    for scope_id in fenced {
        let Some(lash_core::ExecutionScope::Process { process_id }) =
            lash_core::ExecutionScope::from_journal_key(&scope_id)
        else {
            continue;
        };
        let registered: bool = tx.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {registry}.processes WHERE process_id = ?1)"),
            params![process_id.as_str()],
            |row| row.get(0),
        )?;
        if registered {
            lifted += tx.execute(
                &format!(
                    "DELETE FROM {JOURNAL_SCHEMA}.effect_scope_retirements WHERE scope_id = ?1"
                ),
                params![scope_id],
            )?;
        }
    }
    Ok(lifted)
}
