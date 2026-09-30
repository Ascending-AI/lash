//! The durable accounting of one parent-admitted code execution.
pub const TABLE: &str = "worker_recovery";
crate::statements! {
    pub struct WorkerRecoveryStatements @ "worker_recovery" {
        select = "SELECT revision, attempts, cpu_nanos, replacement, unknown_cpu_attempts, in_flight FROM worker_recovery WHERE scope_id = ?1";
        reserve = "INSERT INTO worker_recovery (scope_id, revision, attempts, cpu_nanos, replacement, unknown_cpu_attempts, in_flight)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0) ON CONFLICT (scope_id) DO UPDATE
            SET revision = excluded.revision, attempts = excluded.attempts,
                cpu_nanos = excluded.cpu_nanos, replacement = excluded.replacement, unknown_cpu_attempts = excluded.unknown_cpu_attempts, in_flight = 0";
        running = "UPDATE worker_recovery SET in_flight = 1 WHERE scope_id = ?1 AND revision = ?2";
        settle = "UPDATE worker_recovery SET attempts = ?3, cpu_nanos = ?4, replacement = ?5, unknown_cpu_attempts = ?6, in_flight = 0
            WHERE scope_id = ?1 AND revision = ?2
                AND attempts <= ?3 AND cpu_nanos <= ?4 AND unknown_cpu_attempts <= ?6";
    }
}
