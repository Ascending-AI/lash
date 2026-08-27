use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use lash_sansio::{AttachmentCreateMeta, AttachmentId, AttachmentMeta, AttachmentRef};

use super::{
    AttachmentStore, AttachmentStoreError, AttachmentStorePersistence, StoredAttachment,
    StoredBlobRef, content_id,
};

/// Monotonic suffix so concurrent stagers within one process never collide on a
/// staging file name.
static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

/// An attachment id proven safe to use as one filesystem path component.
///
/// [`AttachmentId`] enforces that shape at construction — non-empty, bounded,
/// printable ASCII, no path separators, no relative-directory or
/// drive-qualified form — so a malformed id cannot exist to be joined here.
/// Requiring this private type in `path_for_id` keeps that dependency visible
/// at every filesystem call site: a raw string can only become a path
/// component by passing through `AttachmentId::parse` first.
#[derive(Clone, Copy)]
struct GuardedAttachmentId<'a>(&'a AttachmentId);

impl<'a> GuardedAttachmentId<'a> {
    fn new(id: &'a AttachmentId) -> Self {
        Self(id)
    }

    fn as_str(self) -> &'a str {
        self.0.as_str()
    }
}

pub struct FileAttachmentStore {
    root: PathBuf,
}

impl FileAttachmentStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn content_root(&self) -> PathBuf {
        self.root.join("blake3")
    }

    fn path_for_id(&self, id: GuardedAttachmentId<'_>) -> PathBuf {
        let id = id.as_str();
        let prefix = id.get(..2).unwrap_or(id);
        self.content_root().join(prefix).join(id)
    }
}

/// Write `bytes` to `final_path` crash-atomically and crash-durably: stage into
/// a per-write unique sibling file, flush it, `rename` into place, then fsync
/// the parent directory so the new directory entry itself survives a crash. A
/// `rename` within one directory is atomic on POSIX, so a reader (or a crash)
/// only ever sees the old contents or the complete new contents — never a
/// half-written file. The staging file is removed on any failure so a crashed
/// write leaves no litter behind, and its unique name means a stale staging
/// file from a prior crash never blocks a later write.
fn write_atomic(final_path: &Path, bytes: &[u8]) -> Result<(), AttachmentStoreError> {
    let counter = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut staging_name = final_path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    staging_name.push(format!(".staging.{}.{counter}.tmp", std::process::id()));
    let tmp_path = final_path
        .parent()
        .map(|parent| parent.join(&staging_name))
        .unwrap_or_else(|| PathBuf::from(&staging_name));

    let io_err = |path: &Path, source: std::io::Error| AttachmentStoreError::Io {
        path: path.to_path_buf(),
        source,
    };

    let write_result = (|| {
        let mut file = fs::File::create(&tmp_path).map_err(|source| io_err(&tmp_path, source))?;
        std::io::Write::write_all(&mut file, bytes).map_err(|source| io_err(&tmp_path, source))?;
        // Durability for the staged bytes before the rename.
        file.sync_all()
            .map_err(|source| io_err(&tmp_path, source))?;
        fs::rename(&tmp_path, final_path).map_err(|source| io_err(final_path, source))?;
        // Make the directory entry itself crash-durable: without this, a crash
        // after `rename` can lose the entry even though the bytes are on disk.
        if let Some(parent) = final_path.parent() {
            fsync_dir(parent);
        }
        Ok(())
    })();

    if write_result.is_err() {
        // Never leave a partial staging file behind.
        let _ = fs::remove_file(&tmp_path);
    }
    write_result
}

/// Best-effort parent-directory fsync. Not every filesystem supports fsync on a
/// directory handle; a failure to open or sync is tolerated rather than failing
/// an otherwise-successful write.
fn fsync_dir(dir: &Path) {
    if let Ok(handle) = fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

#[async_trait::async_trait]
impl AttachmentStore for FileAttachmentStore {
    fn persistence(&self) -> AttachmentStorePersistence {
        AttachmentStorePersistence::Durable
    }

    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        let meta = AttachmentMeta::new(
            content_id(&bytes),
            meta.media_type,
            bytes.len() as u64,
            meta.type_metadata,
            meta.label,
        );
        let path = self.path_for_id(GuardedAttachmentId::new(&meta.id));
        put_at_path(path, bytes, meta)
    }

    async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        get_at_path(self.path_for_id(GuardedAttachmentId::new(id)), id)
    }

    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        delete_at_path(self.path_for_id(GuardedAttachmentId::new(id)))
    }

    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        head_at_path(self.path_for_id(GuardedAttachmentId::new(id)), id)
    }

    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        let content_root = self.content_root();
        let mut blobs = Vec::new();
        let prefix_dirs = match fs::read_dir(&content_root) {
            Ok(entries) => entries,
            // No content written yet: an empty store lists nothing.
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(blobs),
            Err(source) => {
                return Err(AttachmentStoreError::Io {
                    path: content_root,
                    source,
                });
            }
        };
        for prefix_dir in prefix_dirs {
            let prefix_dir = prefix_dir.map_err(|source| AttachmentStoreError::Io {
                path: content_root.clone(),
                source,
            })?;
            if !prefix_dir
                .file_type()
                .map(|ty| ty.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let dir_path = prefix_dir.path();
            for entry in fs::read_dir(&dir_path).map_err(|source| AttachmentStoreError::Io {
                path: dir_path.clone(),
                source,
            })? {
                let entry = entry.map_err(|source| AttachmentStoreError::Io {
                    path: dir_path.clone(),
                    source,
                })?;
                let file_name = entry.file_name();
                let entry_path = entry.path();
                // A file this store never wrote cannot name an attachment, so
                // it is not listed. It is also unreachable through `get` and
                // invisible to reclamation, so say so rather than dropping it
                // in silence: an operator has to know an untracked file is
                // sitting in the content root.
                let Some(name) = file_name.to_str() else {
                    tracing::warn!(
                        path = %entry_path.display(),
                        reason = "file name is not valid UTF-8",
                        "attachment store is skipping a content-root entry that cannot name an \
                         attachment: it is unreachable and invisible to reclamation"
                    );
                    continue;
                };
                // Skip any in-flight staging files.
                if name.contains(".staging.") {
                    continue;
                }
                let id = match AttachmentId::parse(name) {
                    Ok(id) => id,
                    Err(err) => {
                        tracing::warn!(
                            path = %entry_path.display(),
                            reason = %err,
                            "attachment store is skipping a content-root entry that cannot name an \
                             attachment: it is unreachable and invisible to reclamation"
                        );
                        continue;
                    }
                };
                let last_modified_epoch_ms = entry
                    .metadata()
                    .ok()
                    .and_then(|meta| meta.modified().ok())
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|dur| dur.as_millis() as u64);
                blobs.push(StoredBlobRef {
                    id,
                    last_modified_epoch_ms,
                });
            }
        }
        Ok(blobs)
    }
}

fn put_at_path(
    path: PathBuf,
    bytes: Vec<u8>,
    meta: AttachmentMeta,
) -> Result<AttachmentRef, AttachmentStoreError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| AttachmentStoreError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    if path.exists() {
        // Dedup hit: refresh the blob's modification time so a GC sweep that
        // snapshotted the roots before this put (a fresh intent for the same
        // content id) cannot reclaim the now-freshly-referenced blob.
        refresh_mtime(&path, &bytes)?;
    } else {
        write_atomic(&path, &bytes)?;
    }
    Ok(meta.as_ref())
}

/// Refresh a blob's modification time on a dedup-hit `put`. Prefers a cheap
/// `set_modified`; if the platform refuses it (or the file raced with a delete),
/// falls back to a full crash-atomic rewrite, which necessarily bumps the mtime.
fn refresh_mtime(path: &Path, bytes: &[u8]) -> Result<(), AttachmentStoreError> {
    match fs::OpenOptions::new().write(true).open(path) {
        Ok(file) => {
            if file.set_modified(std::time::SystemTime::now()).is_ok() {
                return Ok(());
            }
            // set_modified refused (platform quirk): rewrite below.
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            // Raced with a concurrent delete: rewrite below.
        }
        Err(source) => {
            return Err(AttachmentStoreError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    }
    write_atomic(path, bytes)
}

fn head_at_path(
    path: PathBuf,
    id: &AttachmentId,
) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
    match fs::metadata(&path) {
        Ok(metadata) => {
            let last_modified_epoch_ms = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|dur| dur.as_millis() as u64);
            Ok(Some(StoredBlobRef {
                id: id.clone(),
                last_modified_epoch_ms,
            }))
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(AttachmentStoreError::Io { path, source }),
    }
}

fn get_at_path(path: PathBuf, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
    let bytes = fs::read(&path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            AttachmentStoreError::NotFound(id.clone())
        } else {
            AttachmentStoreError::Io {
                path: path.clone(),
                source,
            }
        }
    })?;
    Ok(StoredAttachment { bytes })
}

fn delete_at_path(path: PathBuf) -> Result<(), AttachmentStoreError> {
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(AttachmentStoreError::Io { path, source }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttachmentTypeMetadata, MediaType};
    use lash_sansio::sync::MutexExt;
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    fn meta() -> AttachmentCreateMeta {
        AttachmentCreateMeta::new(
            MediaType::parse("image/png").unwrap(),
            Some(AttachmentTypeMetadata::image(Some(1), Some(1))),
            Some("pixel".to_string()),
        )
    }

    fn guarded(id: &AttachmentId) -> GuardedAttachmentId<'_> {
        GuardedAttachmentId::new(id)
    }

    /// `path_for_id` joins the id straight into the store root, so it depends
    /// on `AttachmentId` refusing every shape that is not a single path
    /// component. If that rule ever loosens, this fails here rather than
    /// silently handing the filesystem an escape.
    #[test]
    fn attachment_id_rule_keeps_every_shape_out_of_the_store_namespace() {
        let overlong = "a".repeat(129);
        let cases = [
            ("parent with separator", "../"),
            ("parent component", ".."),
            ("current component", "."),
            ("absolute", "/abs"),
            ("forward separator", "a/b"),
            ("back separator", "a\\b"),
            ("nul", "a\0b"),
            ("empty", ""),
            ("unicode", "snowman-☃"),
            ("control", "a\nb"),
            ("windows prefix", "C:escape"),
            ("overlong", overlong.as_str()),
        ];

        for (shape, raw) in cases {
            assert!(
                AttachmentId::parse(raw).is_err(),
                "{shape} id must be unconstructible: {raw:?}"
            );
        }
    }

    #[tokio::test]
    async fn file_store_round_trips_bytes_and_metadata() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = FileAttachmentStore::new(temp.path());
        let reference = store.put(vec![1, 2, 3], meta()).await.expect("put");
        let stored = store.get(&reference.id).await.expect("get");

        assert_eq!(stored.bytes, vec![1, 2, 3]);
        assert_eq!(reference.byte_len, 3);
    }

    // `put` must write crash-atomically (stage into a unique sibling, then
    // rename). After a successful put there must be no leftover staging files
    // in the content directory — proof the temp file was renamed into place
    // rather than written in situ.
    #[tokio::test]
    async fn file_store_writes_atomically_without_temp_litter() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = FileAttachmentStore::new(temp.path());
        let reference = store.put(vec![9, 8, 7, 6], meta()).await.expect("put");

        let final_path = store.path_for_id(guarded(&reference.id));
        assert!(final_path.exists(), "content file must be in place");

        let mut staging_files = Vec::new();
        let dir = final_path.parent().expect("content dir");
        for entry in fs::read_dir(dir).expect("read content dir") {
            let path = entry.expect("dir entry").path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.contains(".staging."))
                .unwrap_or(false)
            {
                staging_files.push(path);
            }
        }
        assert!(
            staging_files.is_empty(),
            "atomic write must not leave staging files behind: {staging_files:?}"
        );

        // The bytes round-trip in full (no truncation from a partial write).
        let stored = store.get(&reference.id).await.expect("get");
        assert_eq!(stored.bytes, vec![9, 8, 7, 6]);
    }

    /// A file in the content root whose name cannot be an attachment id is
    /// unreachable through `get` and invisible to reclamation. Listing it is
    /// wrong, but skipping it silently makes it a permanent invisible orphan,
    /// so the skip is announced.
    #[tokio::test]
    async fn file_store_announces_a_content_root_entry_it_cannot_name() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = FileAttachmentStore::new(temp.path());
        let reference = store.put(vec![4, 5, 6], meta()).await.expect("put");
        let content_root = temp.path().join("blake3");
        let stray_dir = content_root.join("zz");
        fs::create_dir_all(&stray_dir).expect("mkdir stray prefix");
        // A name no `put` could have produced: over the id length bound.
        let stray = stray_dir.join("z".repeat(200));
        fs::write(&stray, b"not an attachment").expect("seed stray file");

        let (blobs, capture) = crate::runtime::tests::trace_capture::capturing(|| async {
            store.list().await.expect("list")
        })
        .await;

        assert_eq!(
            blobs.iter().map(|blob| &blob.id).collect::<Vec<_>>(),
            vec![&reference.id],
            "a name that cannot be an attachment id must not be listed"
        );
        let announced = capture
            .events
            .lock_recover()
            .iter()
            .filter(|event| event.level == "WARN" && event.field("path").contains(&"z".repeat(200)))
            .count();
        assert_eq!(announced, 1, "the skipped entry must be announced once");
    }

    // A stale staging file left by a crashed prior write must not block a
    // subsequent successful put — unique staging names sidestep it entirely,
    // and it is not mistaken for content.
    #[tokio::test]
    async fn file_store_ignores_stale_staging_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = FileAttachmentStore::new(temp.path());
        let content_id = content_id(&[1, 1, 1]);
        let id = AttachmentId::parse(content_id.to_string())
            .expect("content id is a valid attachment id");
        let final_path = store.path_for_id(guarded(&id));
        let parent = final_path.parent().expect("parent");
        fs::create_dir_all(parent).expect("mkdir");
        // Seed a stale staging file with a plausible prior-crash name.
        let mut stale = final_path.file_name().expect("name").to_os_string();
        stale.push(".staging.999.0.tmp");
        fs::write(parent.join(&stale), b"stale partial write").expect("seed stale staging");

        let reference = store
            .put(vec![1, 1, 1], meta())
            .await
            .expect("put over stale staging");
        let stored = store.get(&reference.id).await.expect("get");
        assert_eq!(stored.bytes, vec![1, 1, 1]);

        // The stale staging file is not enumerated as a blob.
        let listed: Vec<AttachmentId> = store
            .list()
            .await
            .expect("list")
            .into_iter()
            .map(|blob| blob.id)
            .collect();
        assert_eq!(listed, vec![reference.id]);
    }

    // Fix C: a dedup-hit `put` (identical bytes already stored) must refresh the
    // blob's modification time, so a GC sweep that snapshotted the roots before
    // this put cannot reclaim the now-freshly-referenced blob.
    #[tokio::test]
    async fn file_store_put_refreshes_mtime_on_dedup_hit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = FileAttachmentStore::new(temp.path());
        let reference = store.put(vec![2, 4, 6], meta()).await.expect("put");
        let path = store.path_for_id(guarded(&reference.id));

        // Age the blob far into the past to make the refresh observable.
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open blob")
            .set_modified(old)
            .expect("set old mtime");
        let aged = store
            .head(&reference.id)
            .await
            .expect("head")
            .expect("blob present");

        // Identical bytes: a dedup hit that must still refresh the mtime.
        store.put(vec![2, 4, 6], meta()).await.expect("dedup put");
        let refreshed = store
            .head(&reference.id)
            .await
            .expect("head")
            .expect("blob present");

        assert!(
            refreshed.last_modified_epoch_ms > aged.last_modified_epoch_ms,
            "dedup-hit put must refresh mtime: {:?} !> {:?}",
            refreshed.last_modified_epoch_ms,
            aged.last_modified_epoch_ms
        );
    }

    #[tokio::test]
    async fn file_store_is_flat_content_addressed_across_writers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = FileAttachmentStore::new(temp.path());
        // Identical bytes written twice resolve to ONE physical blob (flat,
        // content-addressed — no per-session namespace).
        let first = store.put(vec![7, 7, 7], meta()).await.expect("put first");
        let second = store.put(vec![7, 7, 7], meta()).await.expect("put second");
        assert_eq!(first.id, second.id);
        assert_eq!(
            store.path_for_id(guarded(&first.id)),
            store.path_for_id(guarded(&second.id))
        );

        let listed: BTreeSet<AttachmentId> = store
            .list()
            .await
            .expect("list")
            .into_iter()
            .map(|blob| blob.id)
            .collect();
        assert_eq!(listed.len(), 1);
        assert!(listed.contains(&first.id));
    }

    // Runs the backend-agnostic `AttachmentStore` conformance suite against
    // the file-backed implementation. The same suite runs against the
    // in-memory store, so both backends are held to one contract.
    #[tokio::test]
    async fn file_attachment_store_satisfies_conformance() {
        use crate::testing::conformance::ReopenableAttachmentStore;

        // Each `make()` call needs its own root that outlives the returned
        // store. Keep the tempdirs alive for the duration of the suite.
        let dirs: Arc<Mutex<Vec<tempfile::TempDir>>> = Arc::new(Mutex::new(Vec::new()));
        crate::testing::conformance::attachment_store_reopenable(
            || {
                let dir = tempfile::tempdir().expect("tempdir");
                let open =
                    Arc::new(FileAttachmentStore::new(dir.path())) as Arc<dyn AttachmentStore>;
                let reopen =
                    Arc::new(FileAttachmentStore::new(dir.path())) as Arc<dyn AttachmentStore>;
                dirs.lock_recover().push(dir);
                ReopenableAttachmentStore { open, reopen }
            },
            AttachmentStorePersistence::Durable,
        )
        .await;
    }

    /// The escape canary: a traversal id never reaches the store because it
    /// never becomes an `AttachmentId` in the first place. The canary file
    /// outside the store root is here to make the failure mode concrete —
    /// under the previous infallible constructor this id was well formed and
    /// only the store-side guard stood between it and the parent directory.
    #[tokio::test]
    async fn file_store_traversal_id_cannot_escape_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("store");
        let canary = temp.path().join("canary");
        fs::create_dir_all(&root).expect("create store root");
        fs::write(&canary, b"outside").expect("write canary");
        let store = FileAttachmentStore::new(&root);

        let rejected =
            AttachmentId::parse("../canary").expect_err("traversal id must not be constructible");
        assert!(
            rejected.to_string().contains("invalid attachment id"),
            "unexpected error: {rejected}"
        );

        // The only ids the store can be asked about are well-formed ones, and
        // those resolve inside the root.
        let inside = AttachmentId::parse("canary").expect("plain id");
        assert!(matches!(
            store.get(&inside).await,
            Err(AttachmentStoreError::NotFound(_))
        ));
        assert_eq!(fs::read(&canary).expect("canary survives"), b"outside");
    }
}
