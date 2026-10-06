use super::{PROCESS_SCHEMA, SCHEMA, TRIGGER_SCHEMA};
use crate::durable::DURABLE_TABLES;
use crate::schema_fragments::{SESSION_INGRESS_TABLE, SESSION_RUNS_TABLES};

#[derive(Clone, Copy)]
struct SqliteDatabaseDefinition {
    name: &'static str,
    schema: &'static str,
    /// Shared table sets this database also carries; see
    /// [`SqliteDatabase::fragments`].
    fragments: &'static [&'static str],
}

/// One of the three independently versioned SQLite databases a lash backend
/// can hold.
///
/// The variant is the single table for each database's schema SQL, version,
/// and operator-facing name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SqliteDatabase {
    /// Sessions, graph nodes, checkpoints, leases, queued work, and the
    /// durability engine's nodes, actors and mailboxes.
    DurableCore,
    /// The process registry.
    ProcessRegistry,
    /// The trigger store.
    Triggers,
}

impl SqliteDatabase {
    /// Every database a backend holds.
    pub(crate) const ALL: [Self; 3] = [Self::DurableCore, Self::ProcessRegistry, Self::Triggers];

    /// The file this database is kept in under a file backend's root.
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::DurableCore => crate::DURABLE_CORE_DB_FILE,
            Self::ProcessRegistry => "process-registry.db",
            Self::Triggers => "triggers.db",
        }
    }

    /// The last segment of this database's `memdb` name in a memory
    /// backend.
    pub(crate) const fn memory_name(self) -> &'static str {
        match self {
            Self::DurableCore => "core",
            Self::ProcessRegistry => "registry",
            Self::Triggers => "triggers",
        }
    }

    const fn definition(self) -> SqliteDatabaseDefinition {
        match self {
            Self::DurableCore => SqliteDatabaseDefinition {
                name: "durable core",
                schema: SCHEMA,
                fragments: &[SESSION_INGRESS_TABLE, SESSION_RUNS_TABLES, DURABLE_TABLES],
            },
            Self::ProcessRegistry => SqliteDatabaseDefinition {
                name: "process registry",
                schema: PROCESS_SCHEMA,
                fragments: &[],
            },
            Self::Triggers => SqliteDatabaseDefinition {
                name: "trigger store",
                schema: TRIGGER_SCHEMA,
                fragments: &[],
            },
        }
    }

    pub(crate) fn schema(self) -> &'static str {
        self.definition().schema
    }

    pub(crate) const fn component(self) -> lash_core_execution::compat::ComponentId {
        use lash_core_execution::compat::ComponentId;
        match self {
            Self::DurableCore => ComponentId::SQLITE_CORE,
            Self::ProcessRegistry => ComponentId::SQLITE_REGISTRY,
            Self::Triggers => ComponentId::SQLITE_TRIGGERS,
        }
    }

    /// This build's compatibility version for this database.
    pub fn expected_version(self) -> i64 {
        lash_core_execution::compat::descriptor(self.component())
            .map_or(0, |descriptor| i64::from(descriptor.writes.max()))
    }

    /// The operator-facing name used in reports and refusal messages.
    pub fn name(self) -> &'static str {
        self.definition().name
    }

    /// Shared DDL fragments applied after `schema` inside the same
    /// initialization transaction; see [`crate::schema_fragments`].
    pub(crate) fn fragments(self) -> &'static [&'static str] {
        self.definition().fragments
    }

    /// The shared fragments provisioning applies after `schema`, in order.
    #[cfg(feature = "testing")]
    pub(crate) fn fragment_statements(self) -> impl Iterator<Item = &'static str> {
        self.definition().fragments.iter().copied()
    }

    /// Everything provisioning applies, in order: the schema body, the shared
    /// fragments, then every step of the migration catalog up to this build's
    /// version ([`crate::migration::provisioning_steps`]), so a database this
    /// build creates has the shape a migrated one has. Fixtures that shadow
    /// one table apply this to complete the catalog — every statement is
    /// idempotent, so the shadowed declaration stands while every other table
    /// is created.
    pub(crate) fn provisioning_statements(self) -> impl Iterator<Item = &'static str> {
        std::iter::once(self.schema())
            .chain(self.fragments().iter().copied())
            .chain(crate::migration::provisioning_steps(self))
    }
}
