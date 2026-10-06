//! The component compatibility descriptor (ADR 0115 §1).
//!
//! Every versioned stored component (the PostgreSQL schema, the SQLite
//! database file) carries a durable stamp
//! `{version, min_reader}`. A build declares which stamps it opens and which
//! versions it produces in one [`CompatDescriptor`] per component, and every
//! open runs [`admit`] on the stamp before it takes traffic. A refusal is a
//! typed [`CompatRefusal`] whose message names the `lashctl` remedy.
//!
//! The stamps themselves are written and read by the backends; this module
//! owns only the vocabulary and the rule, so both stores answer the same way.

use serde::{Deserialize, Serialize};

pub use lash_sansio::VersionRange;

/// One versioned stored component: a PostgreSQL schema or a SQLite database
/// file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ComponentId(&'static str);

impl ComponentId {
    /// The PostgreSQL schema; its stamp is `lash_schema_versions` row
    /// `lash-postgres-store`.
    pub const POSTGRES: Self = Self("postgres");
    /// A SQLite deployment's one database file; its stamp is its
    /// `lash_compat` row.
    pub const SQLITE_CORE: Self = Self("sqlite-core");

    /// The component's stable name, as refusals and `lashctl version` print it.
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for ComponentId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

/// What this build declares about one component.
#[derive(Clone, Copy, Debug)]
pub struct CompatDescriptor {
    pub component: ComponentId,
    /// The stamps this build opens: `[oldest it still reads, newest it knows]`.
    pub reads: VersionRange,
    /// The versions its migrations or encoders can produce.
    pub writes: VersionRange,
}

/// The PostgreSQL schema's version: the one number `schema.sql` seeds into
/// `lash_schema_versions`, the migration ledger records, the shape artifact
/// names and the release stamp carries. Every reader takes it from the
/// PostgreSQL descriptor below; no backend restates it.
///
/// It is the 1.0 baseline: `schema.sql` provisions the whole catalog at
/// version 1, and the migrate catalog carries no step before it (FIG-5191).
/// A catalog stamped below it, or by a newer release whose reader floor is
/// above it, is refused typed by [`admit`]; it is never upgraded in place or
/// silently reset.
///
/// version_guard(
///     file(
///         path = "crates/lash-postgres-store/schema.sql",
///         cover(
///             "CREATE TABLE IF NOT EXISTS lash_schema_versions",
///             "CREATE TABLE IF NOT EXISTS lash_blobs", "CREATE TABLE IF NOT EXISTS lash_session_head",
///             "CREATE TABLE IF NOT EXISTS lash_graph_nodes",
///             "CREATE TABLE IF NOT EXISTS lash_session_meta",
///             "CREATE TABLE IF NOT EXISTS lash_runtime_turn_commits",
///             "CREATE TABLE IF NOT EXISTS lash_queued_work_batches",
///             "CREATE TABLE IF NOT EXISTS lash_pending_turn_inputs",
///             "CREATE TABLE IF NOT EXISTS lash_processes",
///             "CREATE TABLE IF NOT EXISTS lash_process_events",
///             "CREATE TABLE IF NOT EXISTS lash_process_wake_deliveries",
///             "CREATE TABLE IF NOT EXISTS lash_trigger_subscriptions",
///             "CREATE TABLE IF NOT EXISTS lash_trigger_occurrences",
///             "CREATE TABLE IF NOT EXISTS lash_trigger_deliveries",
///             "CREATE TABLE IF NOT EXISTS lash_lashlang_artifacts",
///         ),
///     ),
///     shapes(
///         path = "crates/lash-core-execution/src/runtime/effect/envelope.rs",
///         cover(
///             RuntimeEffectInvocation, RuntimeEffectEnvelope, RuntimeEffectCommand,
///             RuntimeEffectOutcome,
///         ),
///     ),
///     roots(path = "crates/lash-sansio/src/session_model/mod.rs", TurnOutcome, ErrorEnvelope),
///     catalog(path = "crates/lash-postgres-store/src/postgres/migrate.rs", EXPAND_MIGRATIONS),
/// )
/// version_surface = "migrate"
/// format_outside_manifest = "store schema version: declared in compat.rs for the component's descriptor and read from the deployment through StorePreflight::schema_status, not reported in the durable-format manifest"
/// version_unguarded = "store schema version: a catalog step moves it, and admission reads it through the component's compat descriptor at open (ADR 0115 §1.3), never through a record decoder"
pub const POSTGRES_SCHEMA_VERSION: u32 = 1;

/// The SQLite database's version: its `lash_compat` row and its entry in the
/// release stamp. A SQLite deployment is one database file (ADR 0132 §12),
/// so this one number versions every table the file holds: the durable core,
/// the process registry and the trigger store.
///
/// It is the 1.0 baseline: `schema.rs`, `trigger_schema.rs` and
/// `schema_fragments.rs` provision the whole database at version 1, and the
/// migration catalog carries no step before it (FIG-5191). A database stamped
/// below it, or by a newer release whose reader floor is above it, is refused
/// typed by [`admit`]; it is never upgraded in place or silently recreated. A database file in the retired three-file
/// layout is refused as [`CompatRefusal::RetiredSqliteLayout`].
/// version_guard(
///     shapes(
///         path = "crates/lash-core-execution/src/runtime/effect/envelope.rs",
///         cover(
///             RuntimeEffectInvocation, RuntimeEffectEnvelope, RuntimeEffectCommand,
///             RuntimeEffectOutcome,
///         ),
///     ),
///     roots(
///         path = "crates/lash-sqlite-store/src/lib.rs", StoredBlobEnvelope,
///         BlobArtifactDescriptor, BlobStorageHint, BlobCompression,
///     ),
///     roots(path = "crates/lash-sansio/src/session_model/mod.rs", TurnOutcome, ErrorEnvelope),
///     items(
///         path = "crates/lash-sqlite-store/src/schema.rs", SCHEMA, PROCESS_SCHEMA,
///         elide = "sql_idempotent_index",
///     ),
///     items(
///         path = "crates/lash-sqlite-store/src/trigger_schema.rs", TRIGGER_SCHEMA,
///         elide = "sql_idempotent_index",
///     ),
///     items(
///         path = "crates/lash-sqlite-store/src/schema_fragments.rs", SESSION_INGRESS_TABLE,
///         SESSION_RUNS_TABLES, elide = "sql_idempotent_index",
///     ),
///     catalog(path = "crates/lash-sqlite-store/src/migration.rs", CATALOG),
/// )
/// version_surface = "migrate"
/// format_outside_manifest = "store schema version: declared in compat.rs for the component's descriptor and read from the deployment through StorePreflight::schema_status, not reported in the durable-format manifest"
/// version_unguarded = "store schema version: a catalog step moves it, and admission reads it through the component's compat descriptor at open (ADR 0115 §1.3), never through a record decoder"
pub const SQLITE_CORE_SCHEMA_VERSION: u32 = 1;

/// What this build declares about a store component whose provisioning DDL
/// is at `version`: it reads and writes exactly that version.
#[cfg(not(feature = "synthetic-next"))]
const fn store(component: ComponentId, version: u32) -> CompatDescriptor {
    CompatDescriptor {
        component,
        reads: VersionRange::exactly(version),
        writes: VersionRange::exactly(version),
    }
}

/// Phase A's synthetic N+1 (ADR 0115 §6) expands every store component one
/// step past its provisioning DDL and keeps reading the version before it.
#[cfg(feature = "synthetic-next")]
const fn store(component: ComponentId, version: u32) -> CompatDescriptor {
    CompatDescriptor {
        component,
        reads: VersionRange::between(version, version + 1),
        writes: VersionRange::exactly(version + 1),
    }
}

/// Every component this build declares. `lashctl version --json` prints them.
///
/// The versions are the components' compatibility numbers, the `version` a
/// stamp records. A store component's is its schema-version constant above,
/// 1 at the 1.0 baseline.
pub const DESCRIPTORS: &[CompatDescriptor] = &[
    store(ComponentId::POSTGRES, POSTGRES_SCHEMA_VERSION),
    store(ComponentId::SQLITE_CORE, SQLITE_CORE_SCHEMA_VERSION),
];

/// The descriptor this build declares for `component`.
pub fn descriptor(component: ComponentId) -> Option<&'static CompatDescriptor> {
    DESCRIPTORS
        .iter()
        .find(|descriptor| descriptor.component == component)
}

/// A durable stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CompatStamp {
    pub version: u32,
    pub min_reader: u32,
}

/// What the store said about a component's stamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StampRead {
    /// No stamp. `populated` says whether the store holds any lash objects.
    Absent { populated: bool },
    /// The stamp as stored, not yet checked.
    Present(CompatStamp),
    /// A stamp is there but did not decode. Carries the backend's words.
    Unreadable(String),
}

/// How a store is admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompatAdmission {
    /// The store is empty. The component's installer provisions it and
    /// writes the stamp; nothing opens it before that. A stamp is never
    /// defaulted to the current version.
    Provision,
    /// The stamp is inside `reads`.
    Native,
    /// A newer release expanded the component, and its floor still admits
    /// this build. The shape check runs in tolerant mode (§1.4).
    Expanded { version: u32 },
}

/// Why a store, an epoch or a stored label is refused.
///
/// Each message names its remedy with a `lashctl` command. The JSON shape is
/// what `lashctl --json` reports and what a newer build reads from an older
/// one, so a variant's fields change only in place under the version freeze.
#[derive(
    Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "refusal", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompatRefusal {
    #[error(
        "{component} holds lash data but carries no compatibility stamp; a stamp is never \
         assumed, so the store is refused unchanged. Restore it from a backup or recreate it; \
         `lashctl preflight` reports what it found{}",
        release_suffix(.writing_release)
    )]
    Unstamped {
        component: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    #[error(
        "{component} compatibility stamp is malformed ({detail}); the store is refused \
         unchanged. Restore it from a backup; `lashctl preflight` reports the stamp{}",
        release_suffix(.writing_release)
    )]
    MalformedStamp {
        component: String,
        detail: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    #[error(
        "{component} is at version {found}, older than this build reads ({reads}): an older or \
         skipped release wrote it. Run `lashctl migrate` from the intermediate release first, \
         then from this build{}",
        release_suffix(.writing_release)
    )]
    TooOld {
        component: String,
        found: u32,
        reads: VersionRange,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    /// A SQLite database this build reads but has not migrated: its stamp is
    /// older than the version this build writes. The store's open migrates
    /// every database together, after a complete backup; a component opened
    /// on its own never migrates.
    #[error(
        "{component} is at version {found}, older than the version {target} this build writes: \
         it has not been migrated. Open the whole store with this build (`SqliteStoreSet::open`), \
         which backs up every database and then migrates them together{}",
        release_suffix(.writing_release)
    )]
    MigrationPending {
        component: String,
        found: u32,
        target: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    #[error(
        "{component} is at version {found} with reader floor {min_reader}, above the newest \
         this build reads ({reads}): a newer release contracted it. Run a build whose range \
         reaches {min_reader}; `lashctl version` prints a build's ranges{}",
        release_suffix(.writing_release)
    )]
    ReaderFloorAbove {
        component: String,
        found: u32,
        min_reader: u32,
        reads: VersionRange,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    #[error(
        "{component} carries additions this build cannot write beside: {}. Run the release \
         that expanded it; `lashctl preflight` lists them{}",
        .findings.join("; "),
        release_suffix(.writing_release)
    )]
    ShapeRefused {
        component: String,
        findings: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    /// `F` at open: below the writable range (a skipped release) or above it
    /// (a newer fleet).
    #[error(
        "store records fleet epoch {recorded}, outside this build's writable range {writable}: \
         below it a release was skipped, above it the fleet is newer. Run a build whose \
         writable range contains {recorded}; `lashctl version` prints a build's range{}",
        release_suffix(.writing_release)
    )]
    FleetOutsideWritable {
        recorded: u32,
        writable: VersionRange,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    /// `F` at open: the store records no fleet epoch. The installer seeds it
    /// and an open never does, so no build decides `F` by opening first.
    #[error(
        "{component} records no fleet epoch: `lashctl migrate` seeds it when it provisions or \
         advances the store, and an open never records one. The store is refused unchanged; \
         run `lashctl migrate`, then open again{}",
        release_suffix(.writing_release)
    )]
    FleetUnrecorded {
        component: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    /// A SQLite store's configured path is a directory in the retired layout
    /// of three database files (FIG-5195). A SQLite deployment is one
    /// database file (ADR 0132 §12), and formats reset at 1.0, so no release
    /// migrates the old layout: the open refuses it unchanged.
    #[error(
        "{location} is a directory in the retired SQLite layout of three database files ({}): \
         a SQLite deployment is one database file, and no release migrates the old layout. \
         The directory is left unchanged; configure the path of a database file and recreate \
         the store there",
        .files.join(", ")
    )]
    RetiredSqliteLayout {
        location: String,
        /// The retired layout's database files the directory holds.
        files: Vec<String>,
    },
    /// State or a call a pre-release build wrote (FIG-4819). The 1.0 cut
    /// restarted every counter, so its numbers do not mean what this
    /// build's do, and no release reads it: a store whose floor is above this
    /// build's range although an older release stamped it.
    #[error(
        "{component} holds pre-release state: a lash build from before the 1.0 release wrote \
         it, and 1.0 restarted every format counter, so its numbers do not mean what this \
         build's do. No release reads pre-release state and none migrates it; it is refused \
         unchanged. Recreate the store{}",
        release_suffix(.writing_release)
    )]
    PreRelease {
        component: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    /// A stored label this build has no name for (an obligation state or
    /// kind, an attachment owner kind, a referrer kind): a newer build wrote
    /// it. Classified apart from corruption so no pass treats it as absent.
    #[error(
        "stored {surface} label `{label}` is unknown to this build: a newer build wrote it. The \
         row is kept unchanged; run a build that knows it (`lashctl version` prints a build's \
         ranges)"
    )]
    UnknownVocabulary { surface: String, label: String },
    /// A plugin namespace is stamped with a format the fleet record does not
    /// permit its plugin to write (FIG-4746). Nothing was published.
    #[error(
        "plugin `{plugin}` {namespace:?} format {writer} is outside the writer range {permitted} \
         the fleet record permits: before finalize a build writes only what the fleet already \
         reads. Nothing was published; run a build that writes a permitted format, or finalize \
         the release that introduced format {writer} with `lashctl finalize`"
    )]
    PluginWriterOutsideRange {
        plugin: String,
        namespace: crate::plugin_state::FormatNamespace,
        writer: u32,
        permitted: VersionRange,
    },
    /// The fleet record carries no writer range for a plugin (FIG-4746): a
    /// plugin the record does not name publishes only its first format.
    #[error(
        "the fleet record carries no writer range for plugin `{plugin}`, so only its first \
         format may be published. Nothing was published; provision the plugin's range from its \
         registration before it writes"
    )]
    PluginWriterUnprovisioned { plugin: String },
    /// A recorded plugin writer range is not a range (FIG-4746). The store
    /// fails closed: no publication of that store is admitted.
    #[error(
        "the fleet record's writer range for plugin `{plugin}` is malformed ({detail}); the \
         store is refused unchanged. Restore it from a backup"
    )]
    PluginWriterRangeMalformed { plugin: String, detail: String },
    /// A plugin writes no format the fleet record permits it (FIG-4747): an
    /// admission chooses each plugin's writer inside its range, and this
    /// plugin has none there. Nothing was admitted.
    #[error(
        "plugin `{plugin}` writes formats {writable:?}, none of which is inside the writer range \
         {permitted} the fleet record permits, so it cannot run here. Nothing was admitted; run \
         a build whose plugin writes a permitted format, or finalize the release that \
         introduced its formats with `lashctl finalize`"
    )]
    PluginWriterUnwritable {
        plugin: String,
        writable: Vec<u32>,
        permitted: VersionRange,
    },
}

impl CompatRefusal {
    /// Attach the release that last wrote the store when its stamp is readable.
    /// The refusal's reason and JSON tag remain unchanged.
    pub fn with_writing_release(mut self, release: Option<String>) -> Self {
        match &mut self {
            Self::Unstamped {
                writing_release, ..
            }
            | Self::MalformedStamp {
                writing_release, ..
            }
            | Self::TooOld {
                writing_release, ..
            }
            | Self::MigrationPending {
                writing_release, ..
            }
            | Self::ReaderFloorAbove {
                writing_release, ..
            }
            | Self::ShapeRefused {
                writing_release, ..
            }
            | Self::FleetOutsideWritable {
                writing_release, ..
            }
            | Self::FleetUnrecorded {
                writing_release, ..
            }
            | Self::PreRelease {
                writing_release, ..
            } => *writing_release = release,
            Self::RetiredSqliteLayout { .. }
            | Self::UnknownVocabulary { .. }
            | Self::PluginWriterOutsideRange { .. }
            | Self::PluginWriterUnprovisioned { .. }
            | Self::PluginWriterRangeMalformed { .. }
            | Self::PluginWriterUnwritable { .. } => {}
        }
        self
    }

    /// This refusal as the store's release evidence corrects it.
    ///
    /// Within one release line counters only grow, so a floor above this
    /// build's range means a newer release contracted the store. A floor
    /// above the range on a store that a release older than `build_release`
    /// stamped can only predate the 1.0 counter restart: the refusal is
    /// [`Self::PreRelease`], not "newer". The store is refused either way;
    /// the stamp is evidence for the reason, never an admission input (ADR
    /// 0115 §1.2). A store with no readable stamp, or one this build cannot
    /// order against its own release, keeps the floor refusal.
    pub fn read_against_release(self, writing_release: Option<&str>, build_release: &str) -> Self {
        let older = writing_release.is_some_and(|writing| {
            crate::store::compare_releases(writing, build_release) == Some(std::cmp::Ordering::Less)
        });
        match self {
            Self::ReaderFloorAbove {
                component,
                writing_release: attached,
                ..
            } if older => Self::PreRelease {
                component,
                writing_release: attached,
            },
            other => other,
        }
    }
}

fn release_suffix(release: &Option<String>) -> String {
    release.as_deref().map_or_else(String::new, |release| {
        format!(". Writing release: {release}")
    })
}

/// The admission rule of §1.3, answered in order: absent, malformed, too old,
/// floor passed, admitted.
pub fn admit(
    descriptor: &CompatDescriptor,
    stamp: StampRead,
) -> Result<CompatAdmission, CompatRefusal> {
    let component = || descriptor.component.as_str().to_owned();
    let stamp = match stamp {
        StampRead::Absent { populated: false } => return Ok(CompatAdmission::Provision),
        StampRead::Absent { populated: true } => {
            return Err(CompatRefusal::Unstamped {
                component: component(),
                writing_release: None,
            });
        }
        StampRead::Unreadable(detail) => {
            return Err(CompatRefusal::MalformedStamp {
                component: component(),
                detail,
                writing_release: None,
            });
        }
        StampRead::Present(stamp) => stamp,
    };
    if stamp.min_reader == 0 || stamp.min_reader > stamp.version {
        return Err(CompatRefusal::MalformedStamp {
            component: component(),
            detail: format!(
                "reader floor {} outside [1, version {}]",
                stamp.min_reader, stamp.version
            ),
            writing_release: None,
        });
    }
    let reads = descriptor.reads;
    if stamp.version < reads.min() {
        return Err(CompatRefusal::TooOld {
            component: component(),
            found: stamp.version,
            reads,
            writing_release: None,
        });
    }
    if stamp.min_reader > reads.max() {
        return Err(CompatRefusal::ReaderFloorAbove {
            component: component(),
            found: stamp.version,
            min_reader: stamp.min_reader,
            reads,
            writing_release: None,
        });
    }
    if stamp.version <= reads.max() {
        Ok(CompatAdmission::Native)
    } else {
        Ok(CompatAdmission::Expanded {
            version: stamp.version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CompatAdmission, CompatDescriptor, CompatRefusal, CompatStamp, ComponentId, StampRead,
        VersionRange, admit,
    };

    fn stamp(version: u32, min_reader: u32) -> StampRead {
        StampRead::Present(CompatStamp {
            version,
            min_reader,
        })
    }

    #[test]
    fn admit_truth_table() {
        // A build that reads [2,3] of a component.
        let descriptor = CompatDescriptor {
            component: ComponentId::POSTGRES,
            reads: VersionRange::new(2, 3).expect("range"),
            writes: VersionRange::new(2, 3).expect("range"),
        };
        let reads = descriptor.reads;
        let component = "postgres".to_owned();

        // 1. Absent: an empty store is provisioned, a populated one refused.
        assert_eq!(
            admit(&descriptor, StampRead::Absent { populated: false }),
            Ok(CompatAdmission::Provision)
        );
        assert_eq!(
            admit(&descriptor, StampRead::Absent { populated: true }),
            Err(CompatRefusal::Unstamped {
                component: component.clone(),
                writing_release: None,
            })
        );

        // 2. Unreadable or malformed, checked before any range.
        assert_eq!(
            admit(&descriptor, StampRead::Unreadable("not an integer".into())),
            Err(CompatRefusal::MalformedStamp {
                component: component.clone(),
                detail: "not an integer".into(),
                writing_release: None,
            })
        );
        for (version, min_reader) in [(3, 0), (2, 3), (9, 0), (1, 5), (0, 0)] {
            assert!(
                matches!(
                    admit(&descriptor, stamp(version, min_reader)),
                    Err(CompatRefusal::MalformedStamp { .. })
                ),
                "{{version: {version}, min_reader: {min_reader}}} is malformed"
            );
        }

        // 3. Too old: an older or skipped release wrote it.
        assert_eq!(
            admit(&descriptor, stamp(1, 1)),
            Err(CompatRefusal::TooOld {
                component: component.clone(),
                found: 1,
                reads,
                writing_release: None,
            })
        );

        // 4. Floor passed: a newer release contracted past this build.
        assert_eq!(
            admit(&descriptor, stamp(5, 4)),
            Err(CompatRefusal::ReaderFloorAbove {
                component: component.clone(),
                found: 5,
                min_reader: 4,
                reads,
                writing_release: None,
            })
        );

        // 5. Admitted: native inside `reads`, expanded above it under the floor.
        assert_eq!(admit(&descriptor, stamp(2, 1)), Ok(CompatAdmission::Native));
        assert_eq!(admit(&descriptor, stamp(3, 3)), Ok(CompatAdmission::Native));
        assert_eq!(
            admit(&descriptor, stamp(4, 3)),
            Ok(CompatAdmission::Expanded { version: 4 })
        );
        assert_eq!(
            admit(&descriptor, stamp(7, 2)),
            Ok(CompatAdmission::Expanded { version: 7 })
        );
    }

    /// FIG-4819: a floor above this build's range on a store an older
    /// release stamped predates the counter restart. The same floor under
    /// this release, a newer one, or no readable stamp stays "newer".
    #[test]
    fn a_floor_refusal_of_an_older_releases_store_reads_as_pre_release() {
        let floor = || CompatRefusal::ReaderFloorAbove {
            component: "sqlite-core".into(),
            found: 99,
            min_reader: 99,
            reads: VersionRange::exactly(1),
            writing_release: None,
        };
        for older in ["0.0.0-dev", "0.9.3", "0.0.0-alpha"] {
            assert_eq!(
                floor().read_against_release(Some(older), "1.0.0"),
                CompatRefusal::PreRelease {
                    component: "sqlite-core".into(),
                    writing_release: None,
                },
                "{older}"
            );
        }
        for (writing, build) in [
            (Some("1.1.0"), "1.0.0"),
            (Some("1.0.0"), "1.0.0"),
            (Some("0.0.0-dev"), "0.0.0-dev"),
            (Some("not-a-version"), "1.0.0"),
            (None, "1.0.0"),
        ] {
            assert_eq!(
                floor().read_against_release(writing, build),
                floor(),
                "{writing:?} under {build}"
            );
        }
        // Only the floor refusal claims "newer"; no other reason changes.
        let too_old = CompatRefusal::TooOld {
            component: "sqlite-core".into(),
            found: 1,
            reads: VersionRange::exactly(2),
            writing_release: None,
        };
        assert_eq!(
            too_old.clone().read_against_release(Some("0.9.0"), "1.0.0"),
            too_old
        );
        let refusal = CompatRefusal::PreRelease {
            component: "postgres".into(),
            writing_release: None,
        };
        assert_eq!(
            serde_json::to_string(&refusal).expect("encode"),
            r#"{"refusal":"pre_release","component":"postgres"}"#
        );
    }
}
