pub(super) fn process_owner_death_degraded(path: &'static str) {
    static WARN: std::sync::Once = std::sync::Once::new();
    WARN.call_once(|| {
        tracing::warn!(
            store = "memory",
            path,
            consequence = "process-owned uncommitted intents are never reclaimed",
            "in-memory attachment GC cannot prove process-owner death"
        )
    });
}
