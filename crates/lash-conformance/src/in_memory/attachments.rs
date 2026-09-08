use crate::*;
use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};
#[tokio::test]
async fn file_attachment_store_satisfies_conformance() {
    use crate::conformance::ReopenableAttachmentStore;

    // Each `make()` call needs its own root that outlives the returned
    // store. Keep the tempdirs alive for the duration of the suite.
    let dirs: Arc<Mutex<Vec<tempfile::TempDir>>> = Arc::new(Mutex::new(Vec::new()));
    crate::conformance::attachment_store_reopenable(
        || {
            let dir = tempfile::tempdir().expect("tempdir");
            let open = Arc::new(FileAttachmentStore::new(dir.path())) as Arc<dyn AttachmentStore>;
            let reopen = Arc::new(FileAttachmentStore::new(dir.path())) as Arc<dyn AttachmentStore>;
            dirs.lock_recover().push(dir);
            ReopenableAttachmentStore { open, reopen }
        },
        AttachmentStorePersistence::Durable,
    )
    .await;
}
