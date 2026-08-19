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
//! side-effectful act preflight exists to precede: it may run creation DDL or an
//! explicit migration, it insists on a usable await-event signing secret, and it
//! emits schema-gate telemetry — each of which can be exactly what a broken
//! deployment fails at, and none of which a probe may perform. Nothing here
//! calls a `PostgresStorage` constructor. The only statements this module's own
//! code path reaches are the shared advisory-lock acquisition and the
//! `pg_catalog` reads inside `verify_schema_for`'s `REPEATABLE READ`
//! transaction: no DDL, no version stamp, no seed row, no write.
//!
//! **Credentials never leave.** A preflight report is operator-facing output
//! that lands in logs and tickets, so the location this handle reports is
//! derived from the connection string by keeping only `host[:port]/dbname` and
//! discarding everything before the credentials separator. A string that does
//! not parse yields a fixed placeholder rather than being echoed: a report that
//! cannot name the target is a smaller loss than one that leaks a password.

use async_trait::async_trait;
use lash_core::{
    StoreBackend, StoreError, StorePreflight, StoreSchemaDatabase, StoreSchemaStatus,
    StoreSchemaVerdict,
};
use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::PostgresStorage;

/// The operator-facing name of the single schema-carrying database a PostgreSQL
/// deployment holds.
///
/// SQLite versions four databases independently; PostgreSQL stamps one
/// component version covering the whole installation, so its status always has
/// exactly one row.
const COMPONENT_DATABASE_NAME: &str = "component schema";

/// What the reported location falls back to when the connection string cannot
/// be parsed. Deliberately content-free — see the module documentation.
const REDACTED_PLACEHOLDER: &str = "postgres";

/// A read-only handle over a PostgreSQL deployment, built from raw connection
/// configuration rather than from a wired store.
///
/// Hold one only for as long as the probe takes: it exists to answer a boot-time
/// question, not to become a second pool alongside the runtime's.
#[derive(Clone, Debug)]
pub struct PostgresStorePreflight {
    pool: PgPool,
    location: String,
    owns_pool: bool,
}

impl PostgresStorePreflight {
    /// Build a preflight over a database URL, without dialling it.
    ///
    /// Construction validates the URL and sizes a pool for one probe — a couple
    /// of connections and a short acquire timeout, so a probe against an
    /// unreachable or saturated server fails fast instead of stalling the boot
    /// it was supposed to protect. It deliberately does not connect: a handle
    /// that failed to *exist* because the server was down would push the
    /// diagnosis back into the boot path, whereas
    /// [`StorePreflight::schema_status`] has a documented place to report
    /// exactly that. Per-connection `lock_timeout` and `statement_timeout` are
    /// likewise not set — the reads are catalog reads under a shared lock, and a
    /// probe that timed out mid-read would report drift it did not observe.
    pub fn for_database_url(database_url: &str) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .min_connections(0)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect_lazy(database_url)
            .map_err(|err| StoreError::Backend(err.to_string()))?;
        Ok(Self {
            pool,
            location: redact_location(database_url),
            owns_pool: true,
        })
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
    /// Only closes a pool created by [`PostgresStorePreflight::for_database_url`]. A
    /// borrowed pool is left alone; see [`PostgresStorePreflight::from_pool`].
    pub async fn close(&self) {
        if self.owns_pool {
            self.pool.close().await;
        }
    }
}

/// Keep `host[:port]/dbname` and discard everything that could carry a secret.
///
/// The authority is isolated *first*, at the first `/` after the scheme, and only
/// then split at its last `@`. Order is the whole correctness argument: a
/// password may contain `@`, `?` or `#`, and trimming the query string before
/// the credentials would end a `postgres://user:pa?ss@host/db` at the `?` and
/// publish `user:pa`. A host component can contain none of those characters, so
/// the last `@` inside the authority is always the credentials separator, and
/// the query string can only be trimmed from what is left after it.
///
/// Anything without a scheme separator — a libpq key/value DSN, say, whose
/// `password=` keyword this would have to understand to strip — is not parsed at
/// all but replaced wholesale.
fn redact_location(database_url: &str) -> String {
    let Some((_scheme, rest)) = database_url.split_once("://") else {
        return REDACTED_PLACEHOLDER.to_string();
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, Some(path)),
        None => (rest, None),
    };
    let host = match authority.rsplit_once('@') {
        Some((_credentials, host)) => host,
        None => authority,
    };
    if host.is_empty() {
        return REDACTED_PLACEHOLDER.to_string();
    }
    // Only now is it safe to drop the query string: everything that could have
    // been a credential is already gone.
    let database = path
        .map(|path| {
            path.split_once(['?', '#'])
                .map(|(head, _tail)| head)
                .unwrap_or(path)
        })
        .filter(|database| !database.is_empty());
    match database {
        Some(database) => format!("{host}/{database}"),
        None => host.to_string(),
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
        let report = PostgresStorage::verify_schema_for(&self.pool).await?;
        let expected = i64::from(PostgresStorage::schema_version());
        let verdict = match report.found_version {
            Some(found) if i64::from(found) != expected => StoreSchemaVerdict::Mismatch {
                found: i64::from(found),
            },
            // An absent stamp over existing lash tables is *not* an empty
            // deployment. The open path detects exactly this shape and refuses
            // it (`unstamped_schema` in `postgres/schema.rs`), so reporting it
            // as absent would hand back a pass for a store that is about to
            // refuse — the crash loop this surface exists to prevent. Version 0
            // stands for "provisioned, never stamped", which is the same
            // convention SQLite's side already reports for a populated database
            // whose `user_version` is 0.
            None if report.schema.is_some() => StoreSchemaVerdict::Mismatch { found: 0 },
            // Nothing provisioned: no stamp and no lash tables to carry one.
            None => StoreSchemaVerdict::Absent,
            // Structural drift at the expected version is not a version
            // refusal, and inventing one would misreport it. The report's own
            // per-object diff is the finding, so it is passed through verbatim
            // and left undecided — `SchemaCheck::Enforce` does refuse it at
            // open, which is why the verdict must not read as a pass either.
            Some(_) if !report.is_conformant() => StoreSchemaVerdict::Unreadable {
                reason: report.to_string(),
            },
            Some(_) => StoreSchemaVerdict::Matches,
        };
        Ok(StoreSchemaStatus {
            databases: vec![StoreSchemaDatabase {
                name: COMPONENT_DATABASE_NAME.to_string(),
                location: self.location.clone(),
                expected,
                verdict,
            }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reported_location_never_carries_the_credentials() {
        let redacted =
            redact_location("postgres://lash:hunter2@db.internal:5432/lash?sslmode=require");
        assert_eq!(redacted, "db.internal:5432/lash");
        assert!(!redacted.contains("hunter2"), "{redacted}");
        assert!(!redacted.contains("lash:"), "{redacted}");
    }

    #[test]
    fn a_password_containing_an_at_sign_is_still_stripped() {
        let redacted = redact_location("postgresql://admin:p@ss@w0rd@10.0.0.4/lash");
        assert_eq!(redacted, "10.0.0.4/lash");
        assert!(!redacted.contains("p@ss"), "{redacted}");
    }

    #[test]
    fn a_password_containing_a_query_or_fragment_marker_is_still_stripped() {
        // The defect this pins: trimming the query string before the credentials
        // ended the string at the `?` inside the password and published the
        // user name with the first half of it.
        let query_marker = redact_location("postgres://user:pa?ss@db.internal/lash");
        assert_eq!(query_marker, "db.internal/lash");
        assert!(!query_marker.contains("pa"), "{query_marker}");
        assert!(!query_marker.contains("user"), "{query_marker}");

        let fragment_marker =
            redact_location("postgres://user:pa#ss@db.internal/lash?sslmode=require");
        assert_eq!(fragment_marker, "db.internal/lash");
        assert!(!fragment_marker.contains("pa"), "{fragment_marker}");

        // A query string on the authority alone, with no path, is still not a
        // place credentials may survive.
        assert_eq!(
            redact_location("postgres://user:pa?ss@db.internal"),
            "db.internal"
        );
    }

    #[test]
    fn an_unparseable_connection_string_is_replaced_rather_than_echoed() {
        let redacted = redact_location("host=db.internal user=lash password=hunter2");
        assert_eq!(redacted, REDACTED_PLACEHOLDER);
        assert_eq!(redact_location("postgres://"), REDACTED_PLACEHOLDER);
    }

    #[test]
    fn a_url_without_credentials_keeps_its_target() {
        assert_eq!(
            redact_location("postgres://db.internal/lash"),
            "db.internal/lash"
        );
    }

    // A lazy pool performs no I/O but still installs its idle reaper, so this
    // needs a runtime — not a live server.
    #[tokio::test]
    async fn the_backend_location_is_the_redacted_one() {
        // Proves the redaction is what `backend()` publishes, without needing a
        // live server: the handle carries the string it was constructed with.
        let handle = PostgresStorePreflight {
            pool: PgPool::connect_lazy("postgres://lash:hunter2@db.internal/lash")
                .expect("a lazy pool performs no I/O"),
            location: redact_location("postgres://lash:hunter2@db.internal/lash"),
            owns_pool: true,
        };
        let StoreBackend::Postgres { location } = handle.backend() else {
            panic!("a Postgres handle reports a Postgres backend");
        };
        assert_eq!(location, "db.internal/lash");
        assert!(!location.contains("hunter2"), "{location}");
    }
}
