"""Identify live service test inputs without changing pure test cache keys."""
SERVICE_KEYS = frozenset({
    'DATABASE_URL', 'SQLX_DATABASE_URL',
    'PGHOST', 'PGHOSTADDR', 'PGPORT', 'PGDATABASE', 'PGUSER', 'PGPASSWORD',
    'PGSERVICE', 'PGSERVICEFILE', 'PGPASSFILE',
    'LASH_REQUIRE_S3',
})
SERVICE_PREFIXES = ('LASH_POSTGRES_', 'LASH_S3_')
SERVICE_LABELS = frozenset({'cargo-service-gate', 'lash.service=local'})


def needs_local_uncached(keys, labels=()):
    return bool(SERVICE_LABELS.intersection(labels)) or any(
        key in SERVICE_KEYS or key.startswith(SERVICE_PREFIXES) for key in keys
    )
