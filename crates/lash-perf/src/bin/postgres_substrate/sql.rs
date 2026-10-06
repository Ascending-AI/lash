//! Spike-only PostgreSQL statements. No product schema or engine is installed.

pub const DDL: &str = r#"
CREATE TABLE lash_nodes (
 node_id TEXT PRIMARY KEY, incarnation BIGINT NOT NULL, formats TEXT NOT NULL,
 draining BOOLEAN NOT NULL DEFAULT FALSE, started_at_ms BIGINT NOT NULL,
 heartbeat_expires_at_ms BIGINT NOT NULL
);
CREATE TABLE lash_actors (
 actor_key TEXT COLLATE "C" PRIMARY KEY,
 kind TEXT NOT NULL CHECK (kind IN ('session','process')),
 state TEXT NOT NULL CHECK (state IN ('idle','ready','owned','waiting','parked','terminal')),
 epoch BIGINT NOT NULL DEFAULT 0, owner_node TEXT, owner_incarnation BIGINT,
 ready_at_ms BIGINT, next_due_ms BIGINT, has_mail BOOLEAN NOT NULL DEFAULT FALSE,
 formats TEXT NOT NULL, progress_seq BIGINT NOT NULL DEFAULT 0,
 claim_progress_seq BIGINT NOT NULL DEFAULT 0, failed_activations INT NOT NULL DEFAULT 0,
 park_json TEXT,
 CONSTRAINT ck_actor_owned CHECK ((state = 'owned') = (owner_node IS NOT NULL)),
 CONSTRAINT ck_actor_ready CHECK ((state = 'ready') = (ready_at_ms IS NOT NULL)),
 CONSTRAINT ck_actor_parked CHECK ((state = 'parked') = (park_json IS NOT NULL))
) WITH (fillfactor = 80);
CREATE INDEX lash_actors_ready ON lash_actors (ready_at_ms) WHERE state = 'ready';
CREATE INDEX lash_actors_due ON lash_actors (next_due_ms)
 WHERE state IN ('waiting','parked') AND next_due_ms IS NOT NULL;
CREATE INDEX lash_actors_owner ON lash_actors (owner_node) WHERE state = 'owned';
CREATE FUNCTION lash_now() RETURNS BIGINT LANGUAGE SQL VOLATILE AS
 $$ SELECT (extract(epoch FROM clock_timestamp()) * 1000)::BIGINT $$;
-- A stand-in for the domain write committed atomically with progress_seq.
CREATE TABLE phase_state (actor_key TEXT PRIMARY KEY, revision BIGINT NOT NULL, body BYTEA NOT NULL);
CREATE TABLE wake_events (seq BIGINT PRIMARY KEY);
"#;

pub const SEED: &str = r#"
INSERT INTO lash_actors (actor_key, kind, state, ready_at_ms, formats)
SELECT 's/' || lpad(i::TEXT, 6, '0'), 'session', 'ready', 0, 'spike'
FROM generate_series(0, $1 - 1) i
"#;

pub const CLAIM: &str = r#"
WITH c AS (
 SELECT actor_key FROM lash_actors
 WHERE ((state = 'ready' AND ready_at_ms <= lash_now())
     OR (state = 'waiting' AND next_due_ms <= lash_now()))
   AND formats = ANY($1)
 ORDER BY coalesce(ready_at_ms, next_due_ms)
 LIMIT $2 FOR UPDATE SKIP LOCKED
)
UPDATE lash_actors a SET state = 'owned', epoch = a.epoch + 1,
 owner_node = $3, owner_incarnation = 1, ready_at_ms = NULL, has_mail = FALSE,
 failed_activations = CASE WHEN a.progress_seq = a.claim_progress_seq
                          THEN a.failed_activations + 1 ELSE 0 END,
 claim_progress_seq = a.progress_seq
FROM c WHERE a.actor_key = c.actor_key RETURNING a.actor_key, a.epoch
"#;

pub const RELEASE: &str = r#"
UPDATE lash_actors SET state = 'ready', ready_at_ms = 0,
 owner_node = NULL, owner_incarnation = NULL WHERE actor_key = ANY($1)
"#;

pub const FENCE: &str = "SELECT epoch FROM lash_actors WHERE actor_key = $1 FOR NO KEY UPDATE";
pub const WRITE: &str = r#"
WITH domain_write AS (
 UPDATE phase_state SET revision = revision + 1, body = $2 WHERE actor_key = $1 RETURNING revision
)
UPDATE lash_actors SET progress_seq = progress_seq + 1
WHERE actor_key = $1 AND epoch = $3 AND EXISTS (SELECT 1 FROM domain_write)
"#;

pub const HEARTBEAT: &str = r#"
UPDATE lash_nodes SET heartbeat_expires_at_ms = lash_now() + 15000
WHERE node_id = $1 AND incarnation = 1
"#;
pub const REAP: &str = r#"
WITH dead AS (
 DELETE FROM lash_nodes WHERE heartbeat_expires_at_ms < lash_now() RETURNING node_id, incarnation
)
UPDATE lash_actors a SET state = 'ready', ready_at_ms = lash_now(), epoch = a.epoch + 1,
 owner_node = NULL, owner_incarnation = NULL
FROM dead WHERE a.owner_node = dead.node_id AND a.owner_incarnation = dead.incarnation
RETURNING a.actor_key
"#;

pub const LOCK_SAMPLES: &str = r#"
SELECT count(*) FILTER (WHERE wait_event_type = 'Lock')::BIGINT,
 count(*) FILTER (WHERE state = 'active')::BIGINT
FROM pg_stat_activity WHERE application_name = $1 AND pid <> pg_backend_pid()
"#;
pub const NOTIFY: &str = "SELECT pg_notify($1, $2)";
pub const WAKE: &str = "INSERT INTO wake_events VALUES ($1)";
pub const WAKE_NOTIFY: &str = r#"
WITH accepted AS (INSERT INTO wake_events VALUES ($1) RETURNING seq)
SELECT pg_notify($2, seq::TEXT) FROM accepted
"#;
pub const POLL: &str = "SELECT seq FROM wake_events WHERE seq > $1 ORDER BY seq";
