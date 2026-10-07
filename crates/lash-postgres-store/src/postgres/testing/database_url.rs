/// The PostgreSQL URL required by an explicitly selected service test.
///
/// Service tests should be ignored in runs without services, then selected
/// with `--include-ignored` inside a PostgreSQL gate. An absent URL cannot
/// turn an executed test into a successful no-op.
///
/// # Panics
///
/// Panics when `LASH_POSTGRES_DATABASE_URL` is missing, non-Unicode or blank.
#[allow(
    clippy::disallowed_methods,
    reason = "test fixtures read the service URL injected by the gate"
)]
pub fn required_database_url() -> String {
    std::env::var("LASH_POSTGRES_DATABASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| {
            panic!(
                "PostgreSQL test requires a non-empty LASH_POSTGRES_DATABASE_URL; \
                 run it inside kiln gate lash <fork> -- scripts/ci/with-service.sh pg -- ..."
            )
        })
}
