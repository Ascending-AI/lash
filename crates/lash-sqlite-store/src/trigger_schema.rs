//! The trigger store's tables: subscriptions, occurrences and the tombstones
//! of reclaimed ones, the deliveries an occurrence recorded (each started or
//! refused in the same transaction) and mutation
//! receipts. They are
//! provisioned in the deployment's one database, versioned by
//! `lash_core_store::compat::SQLITE_CORE_SCHEMA_VERSION`.

pub(crate) const TRIGGER_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS trigger_subscription_change_clock (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    current_seq INTEGER NOT NULL CHECK (current_seq >= 0),
    pruned_through INTEGER NOT NULL CHECK (pruned_through >= 0 AND pruned_through <= current_seq)
);
INSERT INTO trigger_subscription_change_clock (singleton, current_seq, pruned_through)
    VALUES (TRUE, 0, 0) ON CONFLICT (singleton) DO NOTHING;
CREATE TABLE IF NOT EXISTS trigger_subscription_changes (
    subscription_id TEXT PRIMARY KEY,
    change_seq INTEGER NOT NULL UNIQUE CHECK (change_seq > 0),
    deleted_at_ms INTEGER CONSTRAINT ck_trigger_subscription_changes_reclaimable CHECK ((deleted_at_ms IS NULL OR json_extract(record_json, '$.lifecycle.lifecycle') = 'tombstoned') IS TRUE),
    record_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_trigger_subscription_changes_deleted
    ON trigger_subscription_changes(deleted_at_ms, change_seq)
    WHERE deleted_at_ms IS NOT NULL;

CREATE TABLE IF NOT EXISTS trigger_subscriptions (
    subscription_id      TEXT PRIMARY KEY,
    owner_scope          TEXT NOT NULL,
    subscription_key     TEXT NOT NULL,
    incarnation          TEXT NOT NULL,
    revision             INTEGER NOT NULL,
    definition_fingerprint      TEXT NOT NULL,
    source_type          TEXT NOT NULL,
    source_key           TEXT NOT NULL,
    lifecycle            TEXT NOT NULL,
    deleted_at_ms        INTEGER,
    created_at_ms        INTEGER NOT NULL,
    updated_at_ms        INTEGER NOT NULL,
    record_json          TEXT NOT NULL,
    CONSTRAINT ck_trigger_subscriptions_lifecycle CHECK (lifecycle IN ('enabled', 'disabled', 'tombstoned')),
    CONSTRAINT ck_trigger_subscriptions_lifecycle_deleted_at CHECK ((lifecycle IN ('enabled', 'disabled') AND deleted_at_ms IS NULL) OR (lifecycle = 'tombstoned' AND deleted_at_ms IS NOT NULL)),
    UNIQUE(owner_scope, subscription_key)
);

CREATE INDEX IF NOT EXISTS idx_trigger_subscriptions_registrant
    ON trigger_subscriptions(owner_scope, subscription_key);

CREATE INDEX IF NOT EXISTS idx_trigger_subscriptions_source
    ON trigger_subscriptions(source_type, source_key, lifecycle);

CREATE TABLE IF NOT EXISTS trigger_occurrences (
    occurrence_id    TEXT PRIMARY KEY,
    idempotency_key  TEXT NOT NULL UNIQUE,
    source_type      TEXT NOT NULL,
    source_key       TEXT NOT NULL,
    occurred_at_ms   INTEGER NOT NULL,
    outcome_kind TEXT NOT NULL,
    reclaimable_at_ms INTEGER,
    record_json      TEXT NOT NULL,
    CONSTRAINT ck_trigger_occurrences_outcome_kind CHECK (outcome_kind IN ('fired', 'dropped')),
    CONSTRAINT ck_trigger_occurrences_reclaimable CHECK (outcome_kind = 'fired' OR reclaimable_at_ms IS NULL),
    UNIQUE (occurrence_id, outcome_kind)
);

CREATE INDEX IF NOT EXISTS idx_trigger_occurrences_source
    ON trigger_occurrences(source_type, source_key, occurred_at_ms);

CREATE INDEX IF NOT EXISTS idx_trigger_occurrences_reclaimable
    ON trigger_occurrences(reclaimable_at_ms, occurrence_id)
    WHERE reclaimable_at_ms IS NOT NULL;

-- An occurrence retention reclaimed (FIG-4513): written with the delete, so
-- an ingest that presents the identity again writes nothing back. The
-- host explicitly forgets it once its source will not redeliver (FIG-4610).
CREATE TABLE IF NOT EXISTS trigger_occurrence_tombstones (
    occurrence_id    TEXT PRIMARY KEY,
    reclaimed_at_ms  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_trigger_occurrence_tombstones_reclaimed
    ON trigger_occurrence_tombstones(reclaimed_at_ms);

CREATE TABLE IF NOT EXISTS trigger_deliveries (
    occurrence_id    TEXT NOT NULL,
    occurrence_outcome_kind TEXT NOT NULL DEFAULT 'fired' CHECK (occurrence_outcome_kind = 'fired'),
    subscription_id  TEXT NOT NULL,
    process_id TEXT,
    status TEXT NOT NULL,
    refusal_json TEXT,
    subscription_incarnation TEXT NOT NULL,
    subscription_revision INTEGER NOT NULL,
    subscription_snapshot_json TEXT NOT NULL,
    created_at_ms    INTEGER NOT NULL,
    CONSTRAINT ck_trigger_deliveries_disposition CHECK (
        (status = 'started' AND process_id IS NOT NULL AND refusal_json IS NULL)
        OR (status = 'refused' AND process_id IS NULL AND refusal_json IS NOT NULL)
    ),
    PRIMARY KEY (occurrence_id, subscription_id),
    FOREIGN KEY (occurrence_id, occurrence_outcome_kind) REFERENCES trigger_occurrences(occurrence_id, outcome_kind) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS trigger_mutation_receipts (
    operation_id    TEXT PRIMARY KEY,
    owner_kind      TEXT NOT NULL,
    owner_id        TEXT NOT NULL,
    request_fingerprint    TEXT NOT NULL,
    result_json     TEXT NOT NULL,
    created_at_ms   INTEGER NOT NULL,
    CONSTRAINT ck_trigger_receipts_owner_kind CHECK (owner_kind IN ('session', 'host', 'platform'))
);

CREATE INDEX IF NOT EXISTS idx_trigger_deliveries_process
    ON trigger_deliveries(process_id);

CREATE INDEX IF NOT EXISTS idx_trigger_deliveries_subscription
    ON trigger_deliveries(subscription_id);
";
