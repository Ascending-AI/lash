//! SQLite statements for capturing durable work for node wake delivery.

// Connection-local callbacks capture producers without a second dispatch list:
// an actor created ready, one whose mail grew, one an owner release, a reap
// or a hand-back readied (FIG-7044), a process whose event log grew, and a
// session whose owner committed, which passes the owner fence and so bumps
// its state revision (FIG-7055). The owner discards that session hint when
// ActorTx::changes_follow_facts proves its snapshot unchanged (FIG-7111).
// Capture carries the values with their key.
// Later updates refresh captured facts; deletion drops a captured actor.
pub(crate) const NODE_WAKE_CAPTURE: &str = "
CREATE TEMP TRIGGER lash_capture_created AFTER INSERT ON main.actors
    WHEN NEW.state = 'ready' BEGIN
    SELECT lash_capture_woken(NEW.actor_key, NEW.state, NEW.owner_node, NEW.owner_boot, 1);
END;
CREATE TEMP TRIGGER lash_capture_woken AFTER UPDATE ON main.actors BEGIN
    SELECT lash_capture_woken(NEW.actor_key, NEW.state, NEW.owner_node, NEW.owner_boot,
        NEW.mail_seq > OLD.mail_seq OR (NEW.state = 'ready' AND OLD.state <> 'ready'));
END;
CREATE TEMP TRIGGER lash_remove_woken AFTER DELETE ON main.actors BEGIN
    SELECT lash_capture_woken(OLD.actor_key, NULL, NULL, NULL, 0);
END;
CREATE TEMP TRIGGER lash_capture_appended AFTER INSERT ON main.process_events BEGIN
    SELECT lash_capture_appended('p/' || NEW.process_id);
END;
CREATE TEMP TRIGGER lash_capture_session_commit AFTER UPDATE OF state_revision ON main.actors
    WHEN NEW.kind = 'session' BEGIN
    SELECT lash_capture_appended(NEW.actor_key);
END;
";

pub(crate) const NODE_WAKE_PEERS: &str =
    "SELECT node_id FROM nodes WHERE draining = 0 AND heartbeat_expires_at_ms > ?1";
#[cfg(test)]
pub(crate) const NODE_WAKE_ROLLBACK_MAIL: &str =
    "UPDATE actors SET mail_seq = mail_seq + 1 WHERE actor_key = ?1";

// The final-facts and member-isolation laws exercise multiple writes inside
// one member; ordinary store calls would each flush their own capture.
#[cfg(test)]
pub(crate) const NODE_WAKE_FINAL_OWNER: &str =
    "UPDATE actors SET state = 'owned', ready_at_ms = NULL, owner_node = 'final-owner',
     owner_boot = 'final-boot' WHERE actor_key = ?1";
#[cfg(test)]
pub(crate) const NODE_WAKE_OWNED_SETUP: &str =
    "UPDATE actors SET state = 'owned', ready_at_ms = NULL, owner_node = 'owner',
     owner_boot = 'boot'";
#[cfg(test)]
pub(crate) const NODE_WAKE_SESSION_REVISION: &str =
    "UPDATE actors SET state_revision = state_revision + 1";
