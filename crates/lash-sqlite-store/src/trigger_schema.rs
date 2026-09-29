//! The trigger store's database: subscriptions, occurrences, the deliveries
//! an occurrence reserved (each a `TriggerDelivery` obligation, ADR 0109) and
//! mutation receipts. Its version is `crate::schema::TRIGGER_SCHEMA_VERSION`.

pub(crate) const TRIGGER_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS lash_compat (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    component TEXT NOT NULL,
    version INTEGER NOT NULL,
    min_reader INTEGER NOT NULL,
    fleet_format INTEGER NOT NULL,
    CHECK (version >= 1 AND min_reader >= 1 AND min_reader <= version AND fleet_format >= 1)
);

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
    reclaimable_at_ms INTEGER,
    record_json      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_trigger_occurrences_source
    ON trigger_occurrences(source_type, source_key, occurred_at_ms);

CREATE INDEX IF NOT EXISTS idx_trigger_occurrences_reclaimable
    ON trigger_occurrences(reclaimable_at_ms, occurrence_id)
    WHERE reclaimable_at_ms IS NOT NULL;

CREATE TABLE IF NOT EXISTS trigger_deliveries (
    occurrence_id    TEXT NOT NULL,
    subscription_id  TEXT NOT NULL,
    process_id       TEXT,
    subscription_incarnation TEXT NOT NULL,
    subscription_revision INTEGER NOT NULL,
    subscription_snapshot_json TEXT NOT NULL,
    created_at_ms    INTEGER NOT NULL,
    obligation_id         TEXT,
    obligation_state      TEXT,
    obligation_attempts   INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms  INTEGER,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms INTEGER,
    CONSTRAINT ck_trigger_deliveries_obligation CHECK ((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)),
    PRIMARY KEY (occurrence_id, subscription_id),
    FOREIGN KEY (occurrence_id) REFERENCES trigger_occurrences(occurrence_id) ON DELETE CASCADE
);

-- A reserved delivery owes its start (ADR 0109, ADR 0021): its obligation id,
-- the relay's due read and the stalled listing.
CREATE UNIQUE INDEX IF NOT EXISTS idx_trigger_deliveries_obligation_id
    ON trigger_deliveries(obligation_id);
CREATE INDEX IF NOT EXISTS idx_trigger_deliveries_obligation_due
    ON trigger_deliveries(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX IF NOT EXISTS idx_trigger_deliveries_obligation_stalled
    ON trigger_deliveries(obligation_id)
    WHERE obligation_state = 'stalled';

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
