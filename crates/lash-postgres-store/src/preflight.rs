//! Read a PostgreSQL deployment's recorded schema version without opening it.
//!
//! [`PostgresStorage::verify_schema_for`](crate::PostgresStorage::verify_schema_for)
//! has been able to describe a database too broken to open since ADR 0052, but
//! it takes a `&PgPool` and returns a crate-shaped [`SchemaReport`]. The host
//! asking "will this boot?" has neither: it has a connection string, and it
//! wants the one backend-independent answer the facade's preflight report is
//! built from. This module is the adapter between those two, and nothing more.
//!
//! **Construction is not an open.** Opening a `PostgresStorage` is the
//! side-effectful act preflight exists to precede: it writes the release stamp
//! and it emits schema-gate telemetry — each of which can be exactly what a broken
//! deployment fails at, and none of which a probe may perform. Nothing here
//! calls a `PostgresStorage` constructor. The only statements this module's own
//! code path reaches are the shared advisory-lock acquisition and the shape,
//! compatibility, release and fleet reads in one `REPEATABLE READ READ ONLY`
//! transaction: no DDL, no version stamp, no seed row, no write. The durable
//! walk in [`walk`] adds only plain `SELECT`s over lash's own tables, two of
//! them inside an explicitly `READ ONLY` transaction — see that module for why
//! the promise is made engine-enforced rather than merely intended.
//!
//! **Credentials never leave.** A preflight report is operator-facing output
//! that lands in logs and tickets, so the location this handle reports is
//! built from the parsed endpoint's host, port and database alone: the user,
//! password and options are never read. A handle over a host's own pool
//! reports a fixed placeholder.

use async_trait::async_trait;
use lash_core_execution::{
    DurableScan, DurableScanPage, StoreBackend, StoreError, StorePreflight, StoreSchemaDatabase,
    StoreSchemaStatus, StoreSchemaVerdict,
};
use sqlx::postgres::{PgConnectOptions, PgPool};

use crate::schema::SchemaObservation;
use lash_core_execution::compat::{self, CompatAdmission, ComponentId, StampRead};

pub(crate) mod walk;

/// The operator-facing name of the single schema-carrying database a PostgreSQL
/// deployment holds.
///
/// SQLite versions four databases independently; PostgreSQL stamps one
/// component version covering the whole installation, so its status always has
/// exactly one row.
const COMPONENT_DATABASE_NAME: &str = "component schema";

/// The location a handle over a host's own pool reports. Deliberately
/// content-free — see the module documentation.
const REDACTED_PLACEHOLDER: &str = "postgres";

/// A read-only handle over a PostgreSQL deployment, built from raw connection
/// configuration rather than from a wired store.
#[derive(Clone, Debug)]
pub struct PostgresStorePreflight {
    pool: PgPool,
    location: String,
    owns_pool: bool,
}

impl PostgresStorePreflight {
    /// A probe of the deployment `endpoints` reach, on a pool sized by
    /// `config.maintenance.preflight_pool` (a couple of connections and a
    /// short acquire timeout by default, so a probe against an unreachable
    /// or saturated server fails fast instead of stalling the boot it was
    /// supposed to protect) and named `<prefix>/preflight`. It deliberately
    /// does not connect: a handle that failed to *exist* because the server
    /// was down would push the diagnosis back into the boot path, whereas
    /// [`StorePreflight::schema_status`] has a documented place to report
    /// exactly that. No `lock_timeout` or `statement_timeout` is installed —
    /// the reads are catalog reads under a shared lock, and a probe that
    /// timed out mid-read would report drift it did not observe.
    pub fn connect_lazy(
        endpoints: &crate::PostgresEndpoints,
        config: &crate::PostgresHostConfig,
    ) -> Self {
        let factory =
            crate::PostgresConnectionFactory::new(endpoints.clone(), config.connection.clone());
        let pool = factory.pool(
            crate::ConnectionRole::Preflight,
            &config.maintenance.preflight_pool,
            None,
        );
        Self {
            pool,
            location: redact_options(endpoints.primary()),
            owns_pool: true,
        }
    }

    /// Read server-wide capacity, including slots unavailable to normal clients.
    pub async fn connection_capacity(
        &self,
    ) -> Result<crate::PostgresConnectionCapacity, StoreError> {
        crate::host::connection_capacity(&self.pool).await
    }

    /// Probe over a pool the caller already owns.
    ///
    /// The caller keeps ownership: [`PostgresStorePreflight::close`] is a no-op
    /// for a handle built this way, because closing a pool the host still
    /// intends to use would make the probe the destructive act it exists not to
    /// be. The reported location is the placeholder, since a live pool carries
    /// no connection string this handle may read.
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            pool,
            location: REDACTED_PLACEHOLDER.to_string(),
            owns_pool: false,
        }
    }

    /// Release the connections this handle opened.
    ///
    /// Only closes a pool created by [`PostgresStorePreflight::connect_lazy`]. A
    /// borrowed pool is left alone; see [`PostgresStorePreflight::from_pool`].
    pub async fn close(&self) {
        if self.owns_pool {
            self.pool.close().await;
        }
    }
}

/// `host:port/dbname` of `options`: never the user, password or options, so
/// a report built from it carries no secret.
pub(crate) fn redact_options(options: &PgConnectOptions) -> String {
    format!(
        "{}:{}/{}",
        options.get_host(),
        options.get_port(),
        options.get_database().unwrap_or("")
    )
}

fn project_schema_status(
    observation: SchemaObservation,
    descriptor: &compat::CompatDescriptor,
    location: String,
) -> StoreSchemaStatus {
    let SchemaObservation {
        report,
        stamp,
        admission_findings,
        release,
        fleet_format,
    } = observation;
    let min_reader = match &stamp {
        StampRead::Present(stamp) => Some(i64::from(stamp.min_reader)),
        _ => None,
    };
    #[cfg(feature = "synthetic-next")]
    let synthetic_expanded = matches!(
        &stamp,
        StampRead::Present(stamp) if stamp.version == descriptor.writes.max()
    );
    let verdict = match compat::admit(descriptor, stamp) {
        Err(refusal) => StoreSchemaVerdict::Refused {
            refusal: refusal
                .read_against_release(release.release(), crate::release_stamp::BUILD_RELEASE),
        },
        Ok(CompatAdmission::Provision) => StoreSchemaVerdict::Absent,
        #[cfg(feature = "synthetic-next")]
        Ok(CompatAdmission::Native) if synthetic_expanded => {
            let findings = admission_findings;
            if findings.is_empty() {
                StoreSchemaVerdict::Matches
            } else {
                StoreSchemaVerdict::Refused {
                    refusal: compat::CompatRefusal::ShapeRefused {
                        component: descriptor.component.as_str().to_owned(),
                        findings,
                        writing_release: None,
                    },
                }
            }
        }
        Ok(CompatAdmission::Native) if report.is_conformant() => StoreSchemaVerdict::Matches,
        Ok(CompatAdmission::Native) => StoreSchemaVerdict::Unreadable {
            reason: report.to_string(),
        },
        Ok(CompatAdmission::Expanded { version }) => {
            let findings = admission_findings;
            if findings.is_empty() {
                StoreSchemaVerdict::Expanded {
                    found: i64::from(version),
                }
            } else {
                StoreSchemaVerdict::Refused {
                    refusal: compat::CompatRefusal::ShapeRefused {
                        component: descriptor.component.as_str().to_owned(),
                        findings,
                        writing_release: None,
                    },
                }
            }
        }
    };
    StoreSchemaStatus {
        databases: vec![StoreSchemaDatabase {
            name: COMPONENT_DATABASE_NAME.to_string(),
            location,
            expected: i64::from(descriptor.reads.max()),
            min_reader,
            verdict,
        }],
        release,
        fleet_format,
    }
}

#[async_trait]
impl StorePreflight for PostgresStorePreflight {
    fn backend(&self) -> StoreBackend {
        StoreBackend::Postgres {
            location: self.location.clone(),
        }
    }

    async fn schema_status(&self) -> Result<StoreSchemaStatus, StoreError> {
        let descriptor = compat::descriptor(ComponentId::POSTGRES).ok_or_else(|| {
            StoreError::Backend("missing PostgreSQL compatibility descriptor".into())
        })?;
        let observation = crate::schema::observe_schema(&self.pool, descriptor).await?;
        Ok(project_schema_status(
            observation,
            descriptor,
            self.location.clone(),
        ))
    }

    /// Walk one page of one durable surface over this handle's pool.
    ///
    /// The pool is the one this handle was built from and nothing else is
    /// constructed to serve the walk — building a `PostgresStorage` to read its
    /// tables would perform the open the whole surface exists to precede. The
    /// enumeration itself, and every argument for why each statement is shaped
    /// the way it is, lives in the `walk` submodule.
    async fn scan_durable(&self, scan: &DurableScan) -> Result<DurableScanPage, StoreError> {
        walk::scan_durable(&self.pool, scan).await
    }
}
