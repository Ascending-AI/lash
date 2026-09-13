use std::sync::Arc;

crate::attachment_adoption_tests!({
    (
        (),
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
    )
});

// No abandoned/condemnation recovery macros: in-memory attachments cannot cold-reopen.

crate::store_maintenance_tests!({
    ((), "in-memory", || {
        Arc::new(crate::InMemorySessionStoreFactory::new()) as Arc<dyn crate::SessionStoreFactory>
    })
});

// No failure-law invocation: the in-memory sweep has no injectable failure path.

crate::retention_tests!({
    (
        (),
        std::sync::Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
    )
});
