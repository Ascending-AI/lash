//! Test-only fixtures for running Postgres suites in parallel.
//!
//! The workspace shares one configured Postgres database
//! (`LASH_POSTGRES_DATABASE_URL`). Suites that truncate or enumerate every
//! `lash_*` table therefore observe rows written by any concurrently running
//! suite, which under a process-per-test runner shows up as rotating,
//! irreproducible failures. Suites that hold the shared advisory lock take
//! turns; suites that do not need their own database instead of a turn.
//!
//! [`IsolatedDatabase`] gives a suite a uniquely named, freshly created
//! database derived from the configured URL, and drops it on teardown. The
//! caller opens [`PostgresStorage`](crate::PostgresStorage) against
//! [`IsolatedDatabase::url`], which provisions the schema the same way it does
//! for the shared database.

use sqlx::{Connection, PgConnection};

/// A throwaway Postgres database, created for one test and dropped with it.
///
/// Construction connects to the maintenance database named in the base URL,
/// issues `CREATE DATABASE`, and hands back a URL pointing at the new
/// database. `Drop` issues `DROP DATABASE ... WITH (FORCE)`, so a test that
/// leaves pooled connections open — or panics — still cleans up.
#[derive(Debug)]
pub struct IsolatedDatabase {
    maintenance_url: String,
    database_name: String,
    url: String,
}

impl IsolatedDatabase {
    /// Creates a uniquely named database beside the one `base_url` points at.
    ///
    /// # Panics
    ///
    /// Panics when the base URL cannot be parsed or the database cannot be
    /// created; both are test-configuration faults with no useful recovery.
    pub async fn create(base_url: &str) -> Self {
        let database_name = format!("lash_test_{}", uuid::Uuid::new_v4().simple());
        let url = replace_database_name(base_url, &database_name);
        let mut connection = PgConnection::connect(base_url)
            .await
            .expect("connect Postgres maintenance database for test isolation");
        // Identifiers are generated here, never caller-supplied, so the quoted
        // interpolation cannot carry an injection; `CREATE DATABASE` also
        // refuses to run as a bound-parameter statement.
        sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
            .execute(&mut connection)
            .await
            .unwrap_or_else(|error| {
                panic!("create isolated test database {database_name}: {error}")
            });
        Self {
            maintenance_url: base_url.to_string(),
            database_name,
            url,
        }
    }

    /// The connection URL for the isolated database.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The generated database name, for diagnostics.
    pub fn database_name(&self) -> &str {
        &self.database_name
    }
}

impl Drop for IsolatedDatabase {
    fn drop(&mut self) {
        let maintenance_url = self.maintenance_url.clone();
        let database_name = self.database_name.clone();
        // Teardown is synchronous so the database is gone before the test
        // process exits, and runs on its own thread + runtime so it works from
        // inside any async context, including a current-thread runtime where
        // blocking on the ambient runtime would panic.
        let dropped = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime.block_on(async move {
                let mut connection = PgConnection::connect(&maintenance_url)
                    .await
                    .map_err(|error| error.to_string())?;
                sqlx::query(&format!(
                    "DROP DATABASE IF EXISTS \"{database_name}\" WITH (FORCE)"
                ))
                .execute(&mut connection)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
            })
        })
        .join();
        match dropped {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!(
                    "warning: could not drop isolated test database {}: {error}",
                    self.database_name
                );
            }
            Err(_) => {
                eprintln!(
                    "warning: teardown thread panicked dropping isolated test database {}",
                    self.database_name
                );
            }
        }
    }
}

/// Rewrites the database component of a Postgres connection URL.
///
/// Kept string-level rather than URL-parsing so query parameters, credentials,
/// and non-standard hosts survive verbatim.
fn replace_database_name(base_url: &str, database_name: &str) -> String {
    let (before_query, query) = match base_url.find('?') {
        Some(index) => (&base_url[..index], &base_url[index..]),
        None => (base_url, ""),
    };
    let scheme_end = before_query
        .find("://")
        .map(|index| index + 3)
        .unwrap_or_else(|| panic!("Postgres URL {base_url} has no scheme"));
    let authority_end = before_query[scheme_end..]
        .find('/')
        .map(|index| scheme_end + index)
        .unwrap_or(before_query.len());
    format!("{}/{database_name}{query}", &before_query[..authority_end])
}

#[cfg(test)]
mod tests {
    use super::replace_database_name;

    #[test]
    fn replaces_database_component_and_keeps_credentials_and_query() {
        assert_eq!(
            replace_database_name("postgres://lash:lash@localhost:5432/lash", "lash_test_1"),
            "postgres://lash:lash@localhost:5432/lash_test_1"
        );
        assert_eq!(
            replace_database_name(
                "postgres://lash:lash@localhost:5432/lash?sslmode=disable",
                "lash_test_1"
            ),
            "postgres://lash:lash@localhost:5432/lash_test_1?sslmode=disable"
        );
        assert_eq!(
            replace_database_name("postgres://localhost", "lash_test_1"),
            "postgres://localhost/lash_test_1"
        );
    }
}
