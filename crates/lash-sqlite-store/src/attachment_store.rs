//! [`SqliteAttachmentStore`]: the attachment port over the SQLite session
//! catalog.
//!
//! A SQLite backend keeps its attachment bytes in the same durable-core
//! database as the attachment manifest and the GC condemnation fence
//! (`attachments.rs`), in the `attachment_blobs` table. The store is the flat,
//! content-addressed [`AttachmentStore`] the port asks for and nothing more:
//! the session boundary, the write-ahead intents and reclamation stay one layer
//! up, in `SessionAttachmentStore` and the manifest, exactly as they do over a
//! file or S3 backend.
//!
//! The store reaches the catalog through the connection of the [`Store`] it is
//! built from, so it lives wherever that store lives: a file catalog or an
//! in-memory one. Its declared persistence follows: a file catalog is
//! [`AttachmentStorePersistence::Durable`], an in-memory one is
//! [`AttachmentStorePersistence::Ephemeral`].

use std::sync::{Arc, LazyLock};

use lash_core_execution::attachments::content_id;
use lash_core_execution::{
    AttachmentCreateMeta, AttachmentId, AttachmentRef, AttachmentStore, AttachmentStoreError,
    AttachmentStoreFailureClass, AttachmentStorePersistence, StoredAttachment, StoredBlobRef,
};
use lash_store_sql::attachment::blob;
use lash_store_sql::{Dialect, SchemaTables, TableLayout};
use rusqlite::{OptionalExtension, params};

use crate::Store;
use crate::conn::SqliteConnection;
use crate::location::DatabaseTarget;
use crate::schema_layout::Schema;

lash_store_sql::statements! {
    /// `attachment_blobs` statements. SQLite alone issues them: the table
    /// exists on SQLite only.
    pub(crate) struct AttachmentBlobSqliteStatements @ "attachment_blob" {
        /// Store the bytes `?2` under their content id `?1`, stamped `?3`.
        ///
        /// A repeated put of bytes already held keeps them — the id is their
        /// content address — and restamps the row, never backwards: the GC
        /// reads the stamp as "last put at", and a clock that stepped back must
        /// not make a freshly re-referenced blob look old.
        upsert = "INSERT INTO attachment_blobs (attachment_id, content, stored_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (attachment_id)
             DO UPDATE SET stored_at_ms = MAX(stored_at_ms, excluded.stored_at_ms)";

        /// The bytes held under `?1`.
        select_content = "SELECT content FROM attachment_blobs WHERE attachment_id = ?1";

        /// One blob's identity and freshness, without its bytes.
        select_ref = "SELECT attachment_id, stored_at_ms FROM attachment_blobs
             WHERE attachment_id = ?1";

        /// Every blob's identity and freshness, for the GC's listing.
        select_all_refs = "SELECT attachment_id, stored_at_ms FROM attachment_blobs";

        /// Drop the bytes held under `?1`. Deleting an absent id is a no-op.
        delete_by_id = "DELETE FROM attachment_blobs WHERE attachment_id = ?1";
    }
}

/// The table lives in the session catalog's own file and is never reached
/// through an `ATTACH`ed name.
const CATALOG: TableLayout =
    TableLayout::new(&[SchemaTables::new(Schema::Main.qualifier(), &[blob::TABLE])]);

static ATTACHMENT_BLOB_SQL: LazyLock<AttachmentBlobSqliteStatements> =
    LazyLock::new(|| AttachmentBlobSqliteStatements::render(Dialect::sqlite(CATALOG)));

/// The attachment-blob statements, rendered once at first use.
pub(crate) fn attachment_blob_sql() -> &'static AttachmentBlobSqliteStatements {
    &ATTACHMENT_BLOB_SQL
}

/// The [`AttachmentStore`] over a SQLite durable-core catalog.
///
/// Build it from the [`Store`] open on the catalog with
/// [`SqliteAttachmentStore::for_store`]. Blobs are keyed by the attachment
/// content id, so identical bytes put from any session are one row, and each
/// row carries the freshness stamp mark-and-sweep GC ages it by, read from the
/// store's clock.
pub struct SqliteAttachmentStore {
    conn: SqliteConnection,
    clock: Arc<dyn lash_core_execution::Clock>,
    persistence: AttachmentStorePersistence,
}

impl SqliteAttachmentStore {
    /// The attachment store over `store`'s catalog: the same database, reached
    /// through the same connection, stamped by the same clock.
    ///
    /// Its persistence is the catalog's location: bytes in a file catalog are
    /// durable; bytes in a memory backend's catalog live as long as the
    /// backend does.
    pub fn for_store(store: &Store) -> Self {
        Self {
            conn: store.conn.clone(),
            clock: Arc::clone(&store.clock),
            persistence: match store.location.target() {
                DatabaseTarget::File(_) => AttachmentStorePersistence::Durable,
                DatabaseTarget::Memory(_) => AttachmentStorePersistence::Ephemeral,
            },
        }
    }
}

/// Classify a SQLite failure for the port's retry and operator verdicts.
///
/// Busy, locked and I/O conditions are transient; a refusal the database will
/// repeat for the same request (a read-only or corrupt file, an oversized
/// blob, a constraint) is terminal; a permission refusal is the operator's to
/// correct. A failure with no SQLite code is a decode or a closed connection
/// on this side, which a retry reproduces.
fn failure_class(error: &rusqlite::Error) -> AttachmentStoreFailureClass {
    use rusqlite::ErrorCode;
    match error.sqlite_error_code() {
        Some(ErrorCode::PermissionDenied | ErrorCode::AuthorizationForStatementDenied) => {
            AttachmentStoreFailureClass::Credentials
        }
        Some(
            ErrorCode::ReadOnly
            | ErrorCode::TooBig
            | ErrorCode::ConstraintViolation
            | ErrorCode::DatabaseCorrupt
            | ErrorCode::NotADatabase
            | ErrorCode::TypeMismatch
            | ErrorCode::ApiMisuse,
        ) => AttachmentStoreFailureClass::Terminal,
        Some(_) => AttachmentStoreFailureClass::Transient,
        None => AttachmentStoreFailureClass::Terminal,
    }
}

fn backend_error(operation: &'static str, error: rusqlite::Error) -> AttachmentStoreError {
    AttachmentStoreError::Backend {
        operation,
        class: failure_class(&error),
        source: Box::new(error),
    }
}

/// Rebuild one stored row's reference, refusing a row the table's own
/// contract forbids: an id that is not an attachment id, or a negative stamp.
fn stored_blob_ref(
    attachment_id: String,
    stored_at_ms: i64,
) -> Result<StoredBlobRef, AttachmentStoreError> {
    let id = AttachmentId::parse(&attachment_id).map_err(|error| {
        AttachmentStoreError::Contract(format!(
            "stored attachment blob id {attachment_id:?} is not an attachment id: {error}"
        ))
    })?;
    let stored_at_ms = u64::try_from(stored_at_ms).map_err(|_| {
        AttachmentStoreError::Contract(format!(
            "stored attachment blob {id} carries a negative stamp {stored_at_ms}"
        ))
    })?;
    Ok(StoredBlobRef {
        id,
        last_modified_epoch_ms: Some(stored_at_ms),
    })
}

#[async_trait::async_trait]
impl AttachmentStore for SqliteAttachmentStore {
    fn persistence(&self) -> AttachmentStorePersistence {
        self.persistence
    }

    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        let reference = AttachmentRef::new(
            content_id(&bytes),
            meta.media_type,
            bytes.len() as u64,
            meta.type_metadata,
            meta.label,
        );
        let attachment_id = reference.id.as_str().to_string();
        let stored_at_ms = crate::clamp_epoch_ms(self.clock.timestamp_ms());
        self.conn
            .call(move |connection| {
                connection.execute(
                    attachment_blob_sql().upsert.sql(),
                    params![attachment_id, bytes, stored_at_ms],
                )
            })
            .await
            .map_err(|error| backend_error("put", error))?;
        Ok(reference)
    }

    async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        let attachment_id = id.as_str().to_string();
        self.conn
            .call(move |connection| {
                connection
                    .query_row(
                        attachment_blob_sql().select_content.sql(),
                        params![attachment_id],
                        |row| row.get::<_, Vec<u8>>(0),
                    )
                    .optional()
            })
            .await
            .map_err(|error| backend_error("get", error))?
            .map(|bytes| StoredAttachment { bytes })
            .ok_or_else(|| AttachmentStoreError::NotFound(id.clone()))
    }

    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        let attachment_id = id.as_str().to_string();
        self.conn
            .call(move |connection| {
                connection.execute(
                    attachment_blob_sql().delete_by_id.sql(),
                    params![attachment_id],
                )
            })
            .await
            .map_err(|error| backend_error("delete", error))?;
        Ok(())
    }

    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        let rows = self
            .conn
            .call(|connection| {
                let mut statement =
                    connection.prepare(attachment_blob_sql().select_all_refs.sql())?;
                statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(|error| backend_error("list", error))?;
        rows.into_iter()
            .map(|(attachment_id, stored_at_ms)| stored_blob_ref(attachment_id, stored_at_ms))
            .collect()
    }

    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        let attachment_id = id.as_str().to_string();
        let row = self
            .conn
            .call(move |connection| {
                connection
                    .query_row(
                        attachment_blob_sql().select_ref.sql(),
                        params![attachment_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .optional()
            })
            .await
            .map_err(|error| backend_error("head", error))?;
        row.map(|(attachment_id, stored_at_ms)| stored_blob_ref(attachment_id, stored_at_ms))
            .transpose()
    }
}
