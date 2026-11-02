/// What the catalog's bytes outlive: a file, or the memory backend.
const PERSISTENCE: AttachmentStorePersistence = match SUBSTRATE {
    crate::backend_fixture::Substrate::File => AttachmentStorePersistence::Durable,
    crate::backend_fixture::Substrate::Memory => AttachmentStorePersistence::Ephemeral,
};

lash_conformance::attachment_store_reopenable_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    (
        retained.clone(),
        move || {
            let backend = retained.open_blocking();
            let reopened = sync_await({
                let backend = backend.clone();
                async move { backend.reopen().await }
            });
            retained.keep(&reopened);
            lash_conformance::ReopenableAttachmentStore {
                open: backend.attachment_store() as Arc<dyn AttachmentStore>,
                reopen: reopened.attachment_store() as Arc<dyn AttachmentStore>,
            }
        },
        PERSISTENCE,
    )
});
