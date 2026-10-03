//! The Restate object sweep and its preflight (ADR 0115 §3.2, FIG-4041).
//!
//! Every Lash object family keeps a `_compat` record whose `format` is the
//! oldest family format any value in the object may carry. After finalize
//! moves the fleet to a release whose family format is newer, the objects the
//! release before it wrote still hold the older format; each family's
//! `upgrade` handler rewrites one object at the newest format and raises its
//! `_compat` in the same exclusive invocation.
//!
//! - The **preflight** ([`preflight_objects`]) lists every object whose
//!   `_compat` names a format below its family's newest, read from Restate's
//!   SQL introspection over state. Introspection measures; it never fences.
//! - The **sweep** ([`sweep_objects`]) calls `upgrade` on each object the
//!   preflight lists, then takes the preflight again. The object state is
//!   the sweep's only cursor: an object the sweep upgraded no longer appears,
//!   and an `upgrade` that raced a crash either committed (the object is
//!   current) or did not (it is listed again), so a sweep run after a crash
//!   finishes exactly what is left, and running it twice changes nothing.
//!   Before finalize every `upgrade` answers `not_finalized` and the sweep
//!   refuses, having rewritten nothing.

use lash_core_store::compat::{CompatRefusal, ComponentId};
use serde::{Deserialize, Serialize};

use crate::compat::{Call, ObjectCompat};
use crate::object_state::{ObjectFamily, ObjectUpgradeResponse};
use crate::{RestateAdminClient, RestateHttpError, RestateIngressClient, RestateNamespace};

/// One object family the sweep upgrades: its Restate service, the component
/// its `_compat` is admitted against, and the newest format this build
/// writes for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpgradableFamily {
    /// The family's service, by its unqualified name (`EffectGroupIndex`).
    pub service: &'static str,
    pub component: ComponentId,
    pub newest: u32,
}

impl UpgradableFamily {
    pub(crate) const fn of(service: &'static str, family: &ObjectFamily) -> Self {
        Self {
            service,
            component: family.component,
            newest: family.formats.surface.build_newest(),
        }
    }
}

pub use crate::services::UPGRADABLE_OBJECT_FAMILIES;

/// One object still at an older family format.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PendingObject {
    /// The family's service, by its unqualified name.
    pub service: String,
    pub key: String,
    /// The format its `_compat` names.
    pub format: u32,
}

/// What the preflight read for one family.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyPreflight {
    pub service: String,
    pub component: String,
    /// The newest format this build writes for the family.
    pub newest: u32,
    /// Every object of the family that keeps a `_compat` record.
    pub objects: usize,
    /// The objects whose `_compat` names an older format, by key.
    pub pending: Vec<PendingObject>,
}

/// The objects every family still holds at an older format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectPreflight {
    pub families: Vec<FamilyPreflight>,
}

impl ObjectPreflight {
    /// Every object still at an older format, family by family.
    pub fn pending(&self) -> impl Iterator<Item = &PendingObject> {
        self.families
            .iter()
            .flat_map(|family| family.pending.iter())
    }

    /// Whether every object is at its family's newest format.
    pub fn upgraded(&self) -> bool {
        self.pending().next().is_none()
    }
}

/// One object the sweep called `upgrade` on, and its answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweptObject {
    pub service: String,
    pub key: String,
    pub outcome: ObjectUpgradeResponse,
}

/// What one sweep did: every object it upgraded, and what the preflight
/// still lists after it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepReport {
    pub swept: Vec<SweptObject>,
    pub remaining: Vec<PendingObject>,
}

/// Why a preflight or a sweep stopped.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "refusal", rename_all = "snake_case")]
pub enum ObjectUpgradeError {
    /// Finalize has not moved the fleet to this family's newest format, so no
    /// object is rewritten while rollback is still promised.
    #[error(
        "{service} `{key}` is not upgraded: the fleet still writes format {writes}; \
         run `lashctl finalize` first"
    )]
    NotFinalized {
        service: String,
        key: String,
        writes: u32,
    },
    /// The object's `_compat` record refuses this build. The refusal is
    /// boxed so every `Result` carrying this error stays small.
    #[error("{service} `{key}` refuses this build's upgrade: {compat}")]
    Incompatible {
        service: String,
        key: String,
        compat: Box<CompatRefusal>,
    },
    /// Restate could not be read or called.
    #[error("{operation}: {detail}")]
    Engine { operation: String, detail: String },
}

/// Where a preflight reads `_compat` records and a sweep calls `upgrade`.
#[async_trait::async_trait]
pub trait ObjectUpgradeTarget: Send + Sync {
    /// Every object of `service` that keeps a `_compat` record, with it.
    async fn compat_records(
        &self,
        service: &str,
    ) -> Result<Vec<(String, ObjectCompat)>, ObjectUpgradeError>;

    /// Call `service`'s `upgrade` handler on the object `key`.
    async fn upgrade(
        &self,
        service: &str,
        key: &str,
    ) -> Result<ObjectUpgradeResponse, ObjectUpgradeError>;
}

/// The objects every [`UPGRADABLE_OBJECT_FAMILIES`] family still holds below
/// its newest format.
pub async fn preflight_objects(
    target: &dyn ObjectUpgradeTarget,
) -> Result<ObjectPreflight, ObjectUpgradeError> {
    let mut families = Vec::with_capacity(UPGRADABLE_OBJECT_FAMILIES.len());
    for family in UPGRADABLE_OBJECT_FAMILIES {
        let records = target.compat_records(family.service).await?;
        let mut pending = records
            .iter()
            .filter(|(_, compat)| compat.format < family.newest)
            .map(|(key, compat)| PendingObject {
                service: family.service.to_owned(),
                key: key.clone(),
                format: compat.format,
            })
            .collect::<Vec<_>>();
        pending.sort();
        families.push(FamilyPreflight {
            service: family.service.to_owned(),
            component: family.component.as_str().to_owned(),
            newest: family.newest,
            objects: records.len(),
            pending,
        });
    }
    Ok(ObjectPreflight { families })
}

/// Upgrade every object the preflight lists, reporting each one to
/// `on_object` as its `upgrade` answers, then take the preflight again.
///
/// The sweep keeps no cursor of its own: the objects' `_compat` records are
/// the cursor, so after a crash the next sweep resumes from what is left.
/// The first `not_finalized` answer stops the sweep with
/// [`ObjectUpgradeError::NotFinalized`]; an object whose `_compat` refuses
/// this build stops it with [`ObjectUpgradeError::Incompatible`]. Neither
/// rewrote the object.
pub async fn sweep_objects(
    target: &dyn ObjectUpgradeTarget,
    mut on_object: impl FnMut(&SweptObject),
) -> Result<SweepReport, ObjectUpgradeError> {
    let preflight = preflight_objects(target).await?;
    let mut swept = Vec::new();
    for pending in preflight.pending() {
        let outcome = target.upgrade(&pending.service, &pending.key).await?;
        if let ObjectUpgradeResponse::NotFinalized { writes } = outcome {
            return Err(ObjectUpgradeError::NotFinalized {
                service: pending.service.clone(),
                key: pending.key.clone(),
                writes,
            });
        }
        let object = SweptObject {
            service: pending.service.clone(),
            key: pending.key.clone(),
            outcome,
        };
        on_object(&object);
        swept.push(object);
    }
    let remaining = preflight_objects(target)
        .await?
        .pending()
        .cloned()
        .collect();
    Ok(SweepReport { swept, remaining })
}

/// The live Restate server: `_compat` records through the admin API's SQL
/// introspection, `upgrade` through ingress, in one ADR 0111 namespace.
#[derive(Clone, Debug)]
pub struct RestateObjectUpgradeTarget {
    admin: RestateAdminClient,
    /// Where `upgrade` is called; a preflight reads only the admin API.
    ingress: Option<RestateIngressClient>,
    namespace: RestateNamespace,
}

impl RestateObjectUpgradeTarget {
    /// A target that reads `_compat` records and calls `upgrade`: a sweep's.
    pub fn new(
        admin: RestateAdminClient,
        ingress: RestateIngressClient,
        namespace: RestateNamespace,
    ) -> Self {
        Self {
            admin,
            ingress: Some(ingress),
            namespace,
        }
    }

    /// A target that only reads `_compat` records: a preflight's. Its
    /// `upgrade` is refused.
    pub fn read_only(admin: RestateAdminClient, namespace: RestateNamespace) -> Self {
        Self {
            admin,
            ingress: None,
            namespace,
        }
    }
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn engine_error(operation: String, error: &RestateHttpError) -> ObjectUpgradeError {
    ObjectUpgradeError::Engine {
        operation,
        detail: error.to_string(),
    }
}

#[async_trait::async_trait]
impl ObjectUpgradeTarget for RestateObjectUpgradeTarget {
    async fn compat_records(
        &self,
        service: &str,
    ) -> Result<Vec<(String, ObjectCompat)>, ObjectUpgradeError> {
        #[derive(Deserialize)]
        struct Row {
            service_key: String,
            value_utf8: Option<String>,
        }
        let query = format!(
            "SELECT service_key, value_utf8 FROM state WHERE service_name = {} AND key = {}",
            sql_literal(&self.namespace.service_name(service)),
            sql_literal(crate::compat::COMPAT_KEY),
        );
        let rows: Vec<Row> =
            self.admin.query_json(&query).await.map_err(|error| {
                engine_error(format!("read {service} `_compat` records"), &error)
            })?;
        rows.into_iter()
            .map(|row| {
                let compat = row
                    .value_utf8
                    .as_deref()
                    .and_then(|text| serde_json::from_str::<ObjectCompat>(text).ok())
                    .ok_or_else(|| ObjectUpgradeError::Incompatible {
                        service: service.to_owned(),
                        key: row.service_key.clone(),
                        compat: Box::new(CompatRefusal::MalformedStamp {
                            component: service.to_owned(),
                            detail: format!(
                                "`{}` does not decode: {:?}",
                                crate::compat::COMPAT_KEY,
                                row.value_utf8
                            ),
                            writing_release: None,
                        }),
                    })?;
                Ok((row.service_key, compat))
            })
            .collect()
    }

    async fn upgrade(
        &self,
        service: &str,
        key: &str,
    ) -> Result<ObjectUpgradeResponse, ObjectUpgradeError> {
        let Some(ingress) = &self.ingress else {
            return Err(ObjectUpgradeError::Engine {
                operation: format!("upgrade {service} `{key}`"),
                detail: "a read-only target calls no handler".to_owned(),
            });
        };
        let answer = ingress
            .call_object_json::<_, crate::compat::Reply<ObjectUpgradeResponse>>(
                &self.namespace.service_name(service),
                key,
                "upgrade",
                &Call::new(()),
            )
            .await;
        match answer {
            Ok(reply) => Ok(reply.body),
            Err(error) => {
                if let RestateHttpError::Status { body, .. } = &error
                    && let Some(crate::wire::RestateCompatError::Incompatible { refusal }) =
                        crate::wire::restate_compat_error_in(body)
                {
                    return Err(ObjectUpgradeError::Incompatible {
                        service: service.to_owned(),
                        key: key.to_owned(),
                        compat: Box::new(refusal),
                    });
                }
                Err(engine_error(format!("upgrade {service} `{key}`"), &error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use lash_core::FleetFormat;

    use super::*;
    use crate::object_state::{ObjectUpgradePlan, StampedValue, plan_object_upgrade};

    /// One family's objects as a server holds them: each key's `_compat` and
    /// raw stamped values.
    type Objects = BTreeMap<String, (Option<ObjectCompat>, BTreeMap<String, Vec<u8>>)>;

    /// A server stand-in that runs the production upgrade plan on in-memory
    /// objects, and can crash: after `crash_after` more upgrades commit, the
    /// next one commits and then fails before it answers, as a sweep killed
    /// while its call is in flight sees it.
    struct Server {
        fleet: FleetFormat,
        objects: Mutex<BTreeMap<&'static str, Objects>>,
        crash_after: Mutex<Option<usize>>,
        upgrades: Mutex<Vec<(String, String)>>,
    }

    fn family_of(service: &str) -> &'static ObjectFamily {
        match service {
            "EffectGroupIndex" | "EffectGroupDrainIndex" => {
                &crate::effect_group::EFFECT_GROUP_STATE_FAMILY
            }
            "EffectGroupPayload" => &crate::effect_group::EFFECT_GROUP_PAYLOAD_FAMILY,
            "LashDurableWaitIndex" => &crate::durable_wait::DURABLE_WAIT_REGISTRY_FAMILY,
            other => panic!("no family serves {other}"),
        }
    }

    fn stamped(format: u32, body: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&StampedValue { format, body }).expect("encode a stamped value")
    }

    impl Server {
        /// `per_family` objects in every family, each at `format`.
        fn with_objects(fleet: FleetFormat, per_family: usize, format: u32) -> Self {
            let objects = UPGRADABLE_OBJECT_FAMILIES
                .iter()
                .map(|family| {
                    let objects = (0..per_family)
                        .map(|index| {
                            let values = BTreeMap::from([(
                                "value".to_owned(),
                                stamped(format, serde_json::json!({"index": index})),
                            )]);
                            (
                                format!("{}-{index}", family.service),
                                (Some(ObjectCompat::fresh(format)), values),
                            )
                        })
                        .collect();
                    (family.service, objects)
                })
                .collect();
            Self {
                fleet,
                objects: Mutex::new(objects),
                crash_after: Mutex::new(None),
                upgrades: Mutex::new(Vec::new()),
            }
        }

        #[cfg(feature = "synthetic-next")]
        fn crash_after(&self, upgrades: usize) {
            *self.crash_after.lock().expect("crash") = Some(upgrades);
        }

        #[cfg(feature = "synthetic-next")]
        fn formats(&self) -> BTreeMap<(String, String), (u32, Vec<u32>)> {
            let objects = self.objects.lock().expect("objects");
            objects
                .iter()
                .flat_map(|(service, objects)| {
                    objects.iter().map(|(key, (compat, values))| {
                        let stamps = values
                            .values()
                            .map(|bytes| {
                                let raw: serde_json::Value =
                                    serde_json::from_slice(bytes).expect("stamped");
                                u32::try_from(raw["format"].as_u64().expect("format")).expect("u32")
                            })
                            .collect();
                        (
                            ((*service).to_owned(), key.clone()),
                            (compat.expect("stamped").format, stamps),
                        )
                    })
                })
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl ObjectUpgradeTarget for Server {
        async fn compat_records(
            &self,
            service: &str,
        ) -> Result<Vec<(String, ObjectCompat)>, ObjectUpgradeError> {
            let objects = self.objects.lock().expect("objects");
            Ok(objects
                .get(service)
                .into_iter()
                .flat_map(|objects| objects.iter())
                .filter_map(|(key, (compat, _))| compat.map(|compat| (key.clone(), compat)))
                .collect())
        }

        async fn upgrade(
            &self,
            service: &str,
            key: &str,
        ) -> Result<ObjectUpgradeResponse, ObjectUpgradeError> {
            let family = family_of(service);
            let mut objects = self.objects.lock().expect("objects");
            let object = objects
                .get_mut(service)
                .and_then(|objects| objects.get_mut(key))
                .expect("the object exists");
            let plan = plan_object_upgrade(
                family,
                self.fleet,
                object.0,
                object
                    .1
                    .iter()
                    .map(|(key, bytes)| (key.clone(), bytes.clone()))
                    .collect(),
            )
            .map_err(|error| ObjectUpgradeError::Engine {
                operation: format!("upgrade {service} `{key}`"),
                detail: error.to_string(),
            })?;
            let response = match plan {
                ObjectUpgradePlan::Unchanged(response) => response,
                ObjectUpgradePlan::Rewrite {
                    values,
                    compat,
                    response,
                } => {
                    let writes = family.formats.writer(self.fleet);
                    for (key, body) in values {
                        let bytes = serde_json::to_vec(&StampedValue {
                            format: writes.format(),
                            body,
                        })
                        .expect("encode");
                        object.1.insert(key, bytes);
                    }
                    object.0 = Some(compat);
                    self.upgrades
                        .lock()
                        .expect("upgrades")
                        .push((service.to_owned(), key.to_owned()));
                    response
                }
            };
            let mut crash = self.crash_after.lock().expect("crash");
            if let Some(left) = crash.as_mut() {
                if *left == 0 {
                    *crash = None;
                    return Err(ObjectUpgradeError::Engine {
                        operation: format!("upgrade {service} `{key}`"),
                        detail: "the sweep crashed with the call in flight".to_owned(),
                    });
                }
                *left -= 1;
            }
            Ok(response)
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    #[cfg(feature = "synthetic-next")]
    fn pending_keys(preflight: &ObjectPreflight) -> Vec<(String, String)> {
        preflight
            .pending()
            .map(|pending| (pending.service.clone(), pending.key.clone()))
            .collect()
    }

    /// The format every family's objects are written at before its successor
    /// upgrades them: N's, one below this build's newest.
    #[cfg(feature = "synthetic-next")]
    fn predecessor() -> u32 {
        UPGRADABLE_OBJECT_FAMILIES[0].newest - 1
    }

    /// Law: `preflight_lists_unupgraded_objects`. The preflight lists every
    /// object whose `_compat` names an older format, and none at the newest.
    #[cfg(feature = "synthetic-next")]
    #[test]
    fn preflight_lists_unupgraded_objects() {
        let server = Server::with_objects(FleetFormat::from_version(2), 3, predecessor());
        {
            let mut objects = server.objects.lock().expect("objects");
            let groups = objects.get_mut("EffectGroupIndex").expect("groups");
            groups.insert(
                "current".to_owned(),
                (
                    Some(ObjectCompat::fresh(predecessor() + 1)),
                    BTreeMap::new(),
                ),
            );
        }
        let preflight = block_on(preflight_objects(&server)).expect("preflight");
        assert!(!preflight.upgraded());
        assert_eq!(preflight.families.len(), 4);
        for family in &preflight.families {
            let expected_objects = if family.service == "EffectGroupIndex" {
                4
            } else {
                3
            };
            assert_eq!(family.objects, expected_objects, "{}", family.service);
            assert_eq!(family.pending.len(), 3, "{}", family.service);
            assert!(
                family
                    .pending
                    .iter()
                    .all(|pending| pending.format == predecessor() && pending.key != "current"),
                "{family:?}"
            );
        }
        // Nothing the preflight reads is written.
        assert!(server.upgrades.lock().expect("upgrades").is_empty());
    }

    /// Law: `object_sweep_resumes_after_crash_and_completes`. A sweep killed
    /// with an upgrade in flight leaves the objects it upgraded current and
    /// the rest listed; the next sweep upgrades exactly what the preflight
    /// lists, each object once, and the preflight ends empty.
    #[cfg(feature = "synthetic-next")]
    #[test]
    fn object_sweep_resumes_after_crash_and_completes() {
        let server = Server::with_objects(FleetFormat::from_version(2), 4, predecessor());
        let every = pending_keys(&block_on(preflight_objects(&server)).expect("preflight"));
        assert_eq!(every.len(), 16);

        server.crash_after(4);
        let mut seen = Vec::new();
        let crashed = block_on(sweep_objects(&server, |object| {
            seen.push((object.service.clone(), object.key.clone()));
        }));
        assert!(
            matches!(crashed, Err(ObjectUpgradeError::Engine { .. })),
            "{crashed:?}"
        );
        // Four answered, and the fifth committed before the crash.
        assert_eq!(seen.len(), 4);
        let left = pending_keys(&block_on(preflight_objects(&server)).expect("preflight"));
        assert_eq!(left.len(), every.len() - 5);
        assert!(seen.iter().all(|object| !left.contains(object)));

        let resumed = block_on(sweep_objects(&server, |_| {})).expect("the resumed sweep");
        assert!(resumed.remaining.is_empty(), "{resumed:?}");
        let resumed_keys = resumed
            .swept
            .iter()
            .map(|object| (object.service.clone(), object.key.clone()))
            .collect::<Vec<_>>();
        assert_eq!(resumed_keys, left);
        assert!(resumed.swept.iter().all(|object| object.outcome
            == ObjectUpgradeResponse::Upgraded {
                from: predecessor(),
                format: predecessor() + 1,
            }));

        // Each object was upgraded exactly once, and every value and record is
        // at the newest format.
        let mut upgraded = server.upgrades.lock().expect("upgrades").clone();
        upgraded.sort();
        let mut expected = every.clone();
        expected.sort();
        assert_eq!(upgraded, expected);
        for ((service, key), (compat, stamps)) in server.formats() {
            let newest = family_of(&service).formats.newest();
            assert_eq!(compat, newest, "{service} {key}");
            assert!(
                stamps.iter().all(|stamp| *stamp == newest),
                "{service} {key}"
            );
        }
        assert!(
            block_on(preflight_objects(&server))
                .expect("preflight")
                .upgraded()
        );

        // A sweep of a swept fleet does nothing.
        let again = block_on(sweep_objects(&server, |_| {})).expect("a second sweep");
        assert!(again.swept.is_empty() && again.remaining.is_empty());
    }

    /// Before finalize the sweep refuses at its first object and rewrites
    /// nothing.
    #[cfg(feature = "synthetic-next")]
    #[test]
    fn the_sweep_refuses_before_finalize() {
        let server = Server::with_objects(FleetFormat::from_version(1), 2, predecessor());
        let before = server.formats();
        let refused = block_on(sweep_objects(&server, |_| {}));
        assert!(
            matches!(
                refused,
                Err(ObjectUpgradeError::NotFinalized { writes, .. }) if writes == predecessor()
            ),
            "{refused:?}"
        );
        assert_eq!(server.formats(), before);
    }

    /// In a build whose families are all at their only format there is
    /// nothing to list and nothing to sweep.
    #[test]
    fn a_fleet_at_the_newest_format_has_nothing_to_sweep() {
        let newest = UPGRADABLE_OBJECT_FAMILIES[0].newest;
        let server = Server::with_objects(FleetFormat::current(), 2, newest);
        let preflight = block_on(preflight_objects(&server)).expect("preflight");
        assert!(preflight.upgraded(), "{preflight:?}");
        let report = block_on(sweep_objects(&server, |_| {})).expect("sweep");
        assert!(report.swept.is_empty() && report.remaining.is_empty());
    }
}
