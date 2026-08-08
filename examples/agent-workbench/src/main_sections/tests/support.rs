pub(super) fn in_memory_trigger_store() -> Arc<dyn lash::triggers::TriggerStore> {
    Arc::new(lash::triggers::InMemoryTriggerStore::new())
}
