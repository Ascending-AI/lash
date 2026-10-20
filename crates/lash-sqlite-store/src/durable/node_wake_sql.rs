//! SQLite statements for capturing durable work for node wake delivery.

// Connection-local buffers capture producers without a second dispatch list:
// an actor created ready, one whose mail grew, one an owner release, a reap
// or a hand-back readied (FIG-7044), and a process whose event log grew. Each
// capture sets the writer's capture mark (`CaptureMark::register`).
pub(crate) const NODE_WAKE_CAPTURE: &str = "
CREATE TEMP TABLE lash_woken (actor TEXT PRIMARY KEY);
CREATE TEMP TABLE lash_appended (actor TEXT PRIMARY KEY);
CREATE TEMP TRIGGER lash_capture_created AFTER INSERT ON main.actors
    WHEN NEW.state = 'ready' BEGIN
    INSERT OR IGNORE INTO lash_woken VALUES (NEW.actor_key);
    SELECT lash_wake_captured();
END;
CREATE TEMP TRIGGER lash_capture_woken AFTER UPDATE OF mail_seq ON main.actors
    WHEN NEW.mail_seq > OLD.mail_seq BEGIN
    INSERT OR IGNORE INTO lash_woken VALUES (NEW.actor_key);
    SELECT lash_wake_captured();
END;
CREATE TEMP TRIGGER lash_capture_readied AFTER UPDATE OF state ON main.actors
    WHEN NEW.state = 'ready' AND OLD.state <> 'ready' BEGIN
    INSERT OR IGNORE INTO lash_woken VALUES (NEW.actor_key);
    SELECT lash_wake_captured();
END;
CREATE TEMP TRIGGER lash_capture_appended AFTER INSERT ON main.process_events BEGIN
    INSERT OR IGNORE INTO lash_appended VALUES ('p/' || NEW.process_id);
    SELECT lash_wake_captured();
END;
";

pub(crate) const NODE_WAKE_ROWS: &str = "
SELECT a.actor_key, a.state, a.owner_node, a.owner_boot
FROM temp.lash_woken w JOIN main.actors a ON a.actor_key = w.actor
UNION ALL SELECT actor, NULL, NULL, NULL FROM temp.lash_appended";
pub(crate) const NODE_WAKE_PEERS: &str =
    "SELECT node_id FROM nodes WHERE draining = 0 AND heartbeat_expires_at_ms > ?1";
#[cfg(test)]
pub(crate) const NODE_WAKE_ROLLBACK_MAIL: &str =
    "UPDATE actors SET mail_seq = mail_seq + 1 WHERE actor_key = ?1";
