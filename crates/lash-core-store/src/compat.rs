//! The component compatibility descriptor (ADR 0115 §1).
//!
//! Every versioned stored component (the PostgreSQL schema, each SQLite
//! database, each Restate object family) carries a durable stamp
//! `{version, min_reader}`. A build declares which stamps it opens and which
//! versions it produces in one [`CompatDescriptor`] per component, and every
//! open runs [`admit`] on the stamp before it takes traffic. A refusal is a
//! typed [`CompatRefusal`] whose message names the `lashctl` remedy.
//!
//! The stamps themselves are written and read by the backends; this module
//! owns only the vocabulary and the rule, so both stores and the Restate
//! objects answer the same way.

use serde::{Deserialize, Serialize};

pub use lash_sansio::VersionRange;

/// One versioned stored component: a PostgreSQL schema, one SQLite
/// database, or one Restate object family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ComponentId(&'static str);

impl ComponentId {
    /// The PostgreSQL schema; its stamp is `lash_schema_versions` row
    /// `lash-postgres-store`.
    pub const POSTGRES: Self = Self("postgres");
    /// The SQLite durable-core database; its stamp is its `lash_compat` row.
    pub const SQLITE_CORE: Self = Self("sqlite-core");
    /// The SQLite process-registry database; its stamp is its `lash_compat` row.
    pub const SQLITE_REGISTRY: Self = Self("sqlite-registry");
    /// The SQLite trigger database; its stamp is its `lash_compat` row.
    pub const SQLITE_TRIGGERS: Self = Self("sqlite-triggers");
    /// Every `EffectGroupIndex` object; its stamp is the object's `_compat`.
    pub const RESTATE_EFFECT_GROUP_STATE: Self = Self("restate-effect-group-state");
    /// Every `EffectGroupPayload` object; its stamp is the object's `_compat`.
    pub const RESTATE_EFFECT_GROUP_PAYLOAD: Self = Self("restate-effect-group-payload");
    /// Every `LashDurableWaitIndex` object; its stamp is the object's `_compat`.
    pub const RESTATE_DURABLE_WAIT_REGISTRY: Self = Self("restate-durable-wait-registry");

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

/// Every component this build declares. `lashctl version --json` prints them.
///
/// The versions are the components' compatibility numbers, the `version` a
/// stamp records. 1.0 is the clean-slate release, so every component starts
/// at `[1,1]`.
pub const DESCRIPTORS: &[CompatDescriptor] = &[
    CompatDescriptor {
        component: ComponentId::POSTGRES,
        reads: VersionRange::exactly(1),
        writes: VersionRange::exactly(1),
    },
    CompatDescriptor {
        component: ComponentId::SQLITE_CORE,
        reads: VersionRange::exactly(1),
        writes: VersionRange::exactly(1),
    },
    CompatDescriptor {
        component: ComponentId::SQLITE_REGISTRY,
        reads: VersionRange::exactly(1),
        writes: VersionRange::exactly(1),
    },
    CompatDescriptor {
        component: ComponentId::SQLITE_TRIGGERS,
        reads: VersionRange::exactly(1),
        writes: VersionRange::exactly(1),
    },
    CompatDescriptor {
        component: ComponentId::RESTATE_EFFECT_GROUP_STATE,
        reads: VersionRange::exactly(1),
        writes: VersionRange::exactly(1),
    },
    CompatDescriptor {
        component: ComponentId::RESTATE_EFFECT_GROUP_PAYLOAD,
        reads: VersionRange::exactly(1),
        writes: VersionRange::exactly(1),
    },
    CompatDescriptor {
        component: ComponentId::RESTATE_DURABLE_WAIT_REGISTRY,
        reads: VersionRange::exactly(1),
        writes: VersionRange::exactly(1),
    },
];

/// The descriptor this build declares for `component`.
pub fn descriptor(component: ComponentId) -> Option<&'static CompatDescriptor> {
    DESCRIPTORS
        .iter()
        .find(|descriptor| descriptor.component == component)
}

/// A durable stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "refusal", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompatRefusal {
    #[error(
        "{component} holds lash data but carries no compatibility stamp; a stamp is never \
         assumed, so the store is refused unchanged. Restore it from a backup or recreate it; \
         `lashctl preflight` reports what it found"
    )]
    Unstamped { component: String },
    #[error(
        "{component} compatibility stamp is malformed ({detail}); the store is refused \
         unchanged. Restore it from a backup; `lashctl preflight` reports the stamp"
    )]
    MalformedStamp { component: String, detail: String },
    #[error(
        "{component} is at version {found}, older than this build reads ({reads}): an older or \
         skipped release wrote it. Run `lashctl migrate` from the intermediate release first, \
         then from this build"
    )]
    TooOld {
        component: String,
        found: u32,
        reads: VersionRange,
    },
    #[error(
        "{component} is at version {found} with reader floor {min_reader}, above the newest \
         this build reads ({reads}): a newer release contracted it. Run a build whose range \
         reaches {min_reader}; `lashctl version` prints a build's ranges"
    )]
    ReaderFloorAbove {
        component: String,
        found: u32,
        min_reader: u32,
        reads: VersionRange,
    },
    /// A Restate object's `_compat` writer floor is above the newest family
    /// format this build writes (ADR 0115 §3.2): a newer release upgraded
    /// the object, and this build may still read it but never mutate it.
    #[error(
        "{component} is at format {found} with writer floor {min_writer}, above the newest \
         this build writes ({writes}): a newer release upgraded it. Run a build whose range \
         reaches {min_writer}; `lashctl version` prints a build's ranges"
    )]
    WriterFloorAbove {
        component: String,
        found: u32,
        min_writer: u32,
        writes: VersionRange,
    },
    #[error(
        "{component} carries additions this build cannot write beside: {}. Run the release \
         that expanded it; `lashctl preflight` lists them",
        .findings.join("; ")
    )]
    ShapeRefused {
        component: String,
        findings: Vec<String>,
    },
    /// `F` at open: below the writable range (a skipped release) or above it
    /// (a newer fleet).
    #[error(
        "store records fleet epoch {recorded}, outside this build's writable range {writable}: \
         below it a release was skipped, above it the fleet is newer. Run a build whose \
         writable range contains {recorded}; `lashctl version` prints a build's range"
    )]
    FleetOutsideWritable {
        recorded: u32,
        writable: VersionRange,
    },
    /// The SQLite databases of one store disagree on their stamps or `F`.
    #[error(
        "the store's databases disagree on their stamps ({}): a migration or finalize stopped \
         part way. Run `lashctl migrate` from the build that advanced them to complete the set",
        describe_databases(.databases)
    )]
    PartiallyAdvanced {
        databases: Vec<(String, CompatStamp, u32)>,
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
}

fn describe_databases(databases: &[(String, CompatStamp, u32)]) -> String {
    databases
        .iter()
        .map(|(database, stamp, fleet)| {
            format!(
                "{database} at version {} floor {} epoch {fleet}",
                stamp.version, stamp.min_reader
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
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
            });
        }
        StampRead::Unreadable(detail) => {
            return Err(CompatRefusal::MalformedStamp {
                component: component(),
                detail,
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
        });
    }
    let reads = descriptor.reads;
    if stamp.version < reads.min() {
        return Err(CompatRefusal::TooOld {
            component: component(),
            found: stamp.version,
            reads,
        });
    }
    if stamp.min_reader > reads.max() {
        return Err(CompatRefusal::ReaderFloorAbove {
            component: component(),
            found: stamp.version,
            min_reader: stamp.min_reader,
            reads,
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
        CompatAdmission, CompatDescriptor, CompatRefusal, CompatStamp, ComponentId, DESCRIPTORS,
        StampRead, VersionRange, admit,
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
                component: component.clone()
            })
        );

        // 2. Unreadable or malformed, checked before any range.
        assert_eq!(
            admit(&descriptor, StampRead::Unreadable("not an integer".into())),
            Err(CompatRefusal::MalformedStamp {
                component: component.clone(),
                detail: "not an integer".into()
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
                reads
            })
        );

        // 4. Floor passed: a newer release contracted past this build.
        assert_eq!(
            admit(&descriptor, stamp(5, 4)),
            Err(CompatRefusal::ReaderFloorAbove {
                component: component.clone(),
                found: 5,
                min_reader: 4,
                reads
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

    #[test]
    fn every_component_starts_at_one() {
        let names: Vec<&str> = DESCRIPTORS.iter().map(|d| d.component.as_str()).collect();
        assert_eq!(
            names,
            [
                "postgres",
                "sqlite-core",
                "sqlite-registry",
                "sqlite-triggers",
                "restate-effect-group-state",
                "restate-effect-group-payload",
                "restate-durable-wait-registry",
            ]
        );
        for descriptor in DESCRIPTORS {
            assert_eq!(descriptor.reads, VersionRange::exactly(1));
            assert_eq!(descriptor.writes, VersionRange::exactly(1));
        }
    }

    #[test]
    fn refusal_json_is_tagged() {
        let refusal = CompatRefusal::FleetOutsideWritable {
            recorded: 3,
            writable: VersionRange::new(1, 2).expect("range"),
        };
        let json = serde_json::to_string(&refusal).expect("encode");
        assert_eq!(
            json,
            r#"{"refusal":"fleet_outside_writable","recorded":3,"writable":{"min":1,"max":2}}"#
        );
        assert_eq!(
            serde_json::from_str::<CompatRefusal>(&json).expect("decode"),
            refusal
        );
    }
}
