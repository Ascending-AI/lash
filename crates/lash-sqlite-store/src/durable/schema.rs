//! Baseline tables for actor ownership, mail and execution resolutions.
pub(crate) const DURABLE_TABLES: &str = "
CREATE TABLE IF NOT EXISTS nodes (
    node_id TEXT PRIMARY KEY,
    boot_id TEXT NOT NULL,
    formats_json TEXT NOT NULL,
    registered_at_ms INTEGER NOT NULL,
    heartbeat_expires_at_ms INTEGER NOT NULL,
    draining INTEGER NOT NULL DEFAULT 0 CONSTRAINT ck_nodes_draining CHECK (draining IN (0, 1)),
    -- What the node offers running code, as JSON: gone with the row.
    capabilities_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS actors (
    actor_key TEXT PRIMARY KEY,
    kind TEXT NOT NULL CONSTRAINT ck_actors_kind CHECK (kind IN ('session', 'process')),
    state TEXT NOT NULL CONSTRAINT ck_actors_state
        CHECK (state IN ('idle', 'ready', 'owned', 'waiting', 'parked', 'terminal')),
    epoch INTEGER NOT NULL,
    owner_node TEXT,
    owner_boot TEXT,
    ready_at_ms INTEGER,
    next_due_ms INTEGER,
    formats TEXT NOT NULL,
    requires TEXT,
    base_requires TEXT,
    state_revision INTEGER NOT NULL,
    mail_seq INTEGER NOT NULL,
    acked_seq INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    park_json TEXT,
    failed_activations INTEGER NOT NULL DEFAULT 0,
    claimed_revision INTEGER,
    CONSTRAINT ck_actors_parked CHECK (state <> 'parked' OR park_json IS NOT NULL),
    CONSTRAINT ck_actors_owned CHECK ((state = 'owned') = (owner_node IS NOT NULL)),
    CONSTRAINT ck_actors_owner_boot CHECK ((owner_node IS NULL) = (owner_boot IS NULL)),
    CONSTRAINT ck_actors_ready CHECK ((state = 'ready') = (ready_at_ms IS NOT NULL)),
    CONSTRAINT ck_actors_due CHECK (next_due_ms IS NULL OR state = 'waiting'),
    CONSTRAINT ck_actors_mail CHECK (acked_seq <= mail_seq),
    CONSTRAINT ck_actors_key_kind CHECK (
        (kind = 'session' AND substr(actor_key, 1, 2) = 's/') OR
        (kind = 'process' AND substr(actor_key, 1, 2) = 'p/'))
);
CREATE INDEX IF NOT EXISTS ix_actors_ready ON actors (ready_at_ms) WHERE state = 'ready';
CREATE INDEX IF NOT EXISTS ix_actors_due ON actors (next_due_ms)
    WHERE state = 'waiting' AND next_due_ms IS NOT NULL;
CREATE INDEX IF NOT EXISTS ix_actors_owner ON actors (owner_node, owner_boot)
    WHERE state = 'owned';

-- JSON sidecars belong to Package content, outside its structural identity.
CREATE TABLE IF NOT EXISTS package_labels (
    namespace TEXT NOT NULL CHECK (namespace = 'kernel_document'),
    package_ref TEXT NOT NULL CHECK (package_ref LIKE 'package:v1:%'),
    label_digest BLOB NOT NULL CHECK (length(label_digest) = 32),
    labels_json TEXT NOT NULL,
    PRIMARY KEY (namespace, package_ref, label_digest),
    FOREIGN KEY (namespace, package_ref) REFERENCES artifact_refs(namespace, artifact_ref) ON DELETE CASCADE
);

-- Immutable execution membership and terminal deferred-use outcomes.
CREATE TABLE IF NOT EXISTS package_executions (
    execution TEXT PRIMARY KEY,
    actor_key TEXT NOT NULL REFERENCES actors(actor_key) ON DELETE CASCADE,
    registration_json TEXT NOT NULL,
    ended INTEGER NOT NULL DEFAULT 0 CHECK (ended IN (0, 1))
);
CREATE INDEX IF NOT EXISTS idx_package_executions_actor ON package_executions(actor_key);
-- Actor termination and claim debt visit only live membership, never ended history.
CREATE INDEX IF NOT EXISTS idx_package_executions_open_actor ON package_executions(actor_key)
    WHERE ended = 0;
CREATE TABLE IF NOT EXISTS execution_resolutions (
    execution TEXT NOT NULL REFERENCES package_executions(execution) ON DELETE CASCADE,
    link TEXT NOT NULL,
    slot INTEGER NOT NULL CHECK (slot >= 0 AND slot <= 4294967295),
    record_json TEXT NOT NULL,
    PRIMARY KEY (execution, link, slot)
);

CREATE TABLE IF NOT EXISTS actor_mail (
    actor_key TEXT NOT NULL REFERENCES actors (actor_key),
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    body TEXT NOT NULL,
    appended_at_ms INTEGER NOT NULL,
    PRIMARY KEY (actor_key, seq)
);
";
