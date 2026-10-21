//! Enumerate the durable payloads a SQLite deployment is holding, without
//! opening the store and without decoding what is found.
//!
//! The schema read next door answers "which databases would refuse an open?".
//! It cannot answer the question an operator asks immediately afterwards —
//! *what is parked behind that refusal, and whose is it?* — because a version
//! stamp names a format, not the identities carrying it. A refusal an operator
//! cannot turn into a drain list is a refusal they cannot act on, so this module
//! walks the places durable payloads actually live and reports each one with
//! enough identity to appear on that list.
//!
//! **What a walk is allowed to touch.** Exactly the same read-only connection
//! discipline the module next door documents and for the same reason: opened
//! `SQLITE_OPEN_READ_ONLY` with no `SQLITE_OPEN_CREATE`, `PRAGMA query_only` set
//! before the first read, and never a read-write fallback — a read-write
//! connection checkpoints a hot WAL and deletes it on close, which rewrites the
//! main database file's bytes. A probe that writes the thing it was asked about
//! has answered a different question.
//!
//! **Why it never decodes.** Every payload here is returned as bytes or text and never parsed
//! into the type it represents.
//! The one place this module looks *inside* bytes is the checkpoint manifest, and even there
//! it reads the one node reference its body names rather than calling the crate's validating
//! load, precisely so a manifest written by another build is walked past rather than raised
//! as an error.
//!
//! The framing a walk *does* unwrap is storage bookkeeping rather than durable
//! payload format: the [`StoredBlobEnvelope`](crate::StoredBlobEnvelope) wrapper
//! and its optional zlib frame. The SQLite schema version guards that
//! storage format. A caller handed the wrapped bytes would see SQLite framing
//! instead of the payload. An item whose envelope
//! cannot be read is reported as [`DurablePayload::Missing`], not as bare logical
//! bytes, because the legacy bare-blob shape is not valid in an admitted database.
//!
//! **Why nothing here fails the page.** A dangling blob reference, an envelope
//! that will not inflate, a manifest from a build that no longer exists: each is
//! a finding about one item, and a walk that returned `Err` on the first of them
//! could not report the thousand items behind it. They become
//! [`DurablePayload::Missing`] or, where the item genuinely does not exist, no
//! item at all. Only a failure that leaves *nothing* to report — a database that
//! cannot be opened or queried — downgrades the whole page, and even that is
//! reported as [`ScanCoverage::NotScanned`] rather than an error, so a caller
//! reads "nobody looked" instead of mistaking silence for "nothing here".

use lash_vocabulary::identity::SessionId;

use lash_core_store::store::DurableItem;
use lash_core_store::store::DurablePayload;
use lash_core_store::store::DurableScan;
use lash_core_store::store::DurableScanPage;
use lash_core_store::store::DurableSurface;
use lash_core_store::store::ScanCoverage;
use lash_core_store::store::StoreError;
use rusqlite::{Connection, params};

use super::SqliteStorePreflight;
use crate::conn::SqliteConnection;
use crate::location::DatabaseTarget;

/// Never returns `Err`: every failure this path can reach is attributable to a
/// database (reported as [`ScanCoverage::NotScanned`]) or to an item (reported
/// as [`DurablePayload::Missing`]). The `Result` is kept because the trait's
/// contract reserves it for a backend whose connection call can fail with
/// nothing left to report at all, which a local file open cannot.
pub(super) async fn scan_durable(
    preflight: &SqliteStorePreflight,
    scan: &DurableScan,
) -> Result<DurableScanPage, StoreError> {
    let target: DatabaseTarget = match scan.surface {
        DurableSurface::ModuleArtifact
        | DurableSurface::SessionCheckpoint
        | DurableSurface::SessionExecutionState => preflight.location.target(),
        // `DurableSurface` is `#[non_exhaustive]`: a surface added upstream
        // before this backend learns to walk it must report that nobody looked,
        // never an empty page that reads as "nothing parked here".
        surface => {
            return Ok(not_scanned(format!(
                "the SQLite backend does not enumerate {}",
                surface.name()
            )));
        }
    };

    if !target.exists() {
        // A database that was never provisioned is *scanned*: nothing is
        // parked in a database that does not exist, and the walk reached that
        // conclusion by looking. Reporting it as unscanned would put a
        // deployment with genuinely nothing to drain on the "investigate this"
        // list forever.
        return Ok(scanned(Vec::new(), None));
    }

    // A failed read-only open is a database nobody could read, never a reason to reach for a
    // connection that can write.
    let conn =
        match SqliteConnection::open_readonly_configured(&target, preflight.operational).await {
            Ok(conn) => conn,
            Err(error) => return Ok(not_scanned(error.to_string())),
        };

    let surface = scan.surface;
    let after = scan.after.clone();
    let limit = scan.limit;
    let page = conn
        .call(move |connection| {
            // The engine enforces the promise this module documents, so the
            // guarantee does not depend on which statements happen to be sent.
            connection.pragma_update(None, "query_only", true)?;
            read_page(connection, surface, after.as_deref(), limit)
        })
        .await;

    match page {
        Ok((items, next)) => Ok(scanned(items, next)),
        // The file exists but would not answer. SQLite's own words travel to
        // the operator verbatim; this module does not model failures it cannot
        // attribute, and guessing would put a verdict behind evidence nobody
        // has.
        Err(error) => Ok(not_scanned(error.to_string())),
    }
}

fn scanned(items: Vec<DurableItem>, next: Option<String>) -> DurableScanPage {
    DurableScanPage {
        items,
        next,
        coverage: ScanCoverage::Scanned,
    }
}

fn not_scanned(reason: String) -> DurableScanPage {
    DurableScanPage {
        items: Vec::new(),
        next: None,
        coverage: ScanCoverage::NotScanned { reason },
    }
}

fn read_page(
    conn: &Connection,
    surface: DurableSurface,
    after: Option<&str>,
    limit: usize,
) -> rusqlite::Result<(Vec<DurableItem>, Option<String>)> {
    match surface {
        DurableSurface::ModuleArtifact => read_module_artifacts(conn, after, limit),
        DurableSurface::SessionCheckpoint => read_session_checkpoints(conn, after, limit),
        DurableSurface::SessionExecutionState => read_session_execution_state(conn, after, limit),
        // Unreachable: `scan_durable` routes unknown surfaces to `NotScanned`
        // before a connection is opened. Kept because the enum is
        // `#[non_exhaustive]`, and answering with nothing beats a panic inside a
        // probe whose job is to survive the deployments that are already broken.
        _ => Ok((Vec::new(), None)),
    }
}

/// One persisted module artifact per module reference.
///
/// The pointer row is left-joined to its content-addressed blob so a dangling
/// artifact reference remains visible as a named item. The blob envelope is
/// storage bookkeeping; `logical_json_payload` removes it before the shared
/// preflight extractor verifies the module identity.
fn read_module_artifacts(
    conn: &Connection,
    after: Option<&str>,
    limit: usize,
) -> rusqlite::Result<(Vec<DurableItem>, Option<String>)> {
    let mut statement = conn.prepare_cached(
        crate::artifact_store::artifact_sql()
            .refs
            .select_preflight_page
            .sql(),
    )?;
    let rows = statement.query_map(
        params![
            crate::artifact_store::MODULE_ARTIFACT_NAMESPACE,
            after,
            limit_binding(limit)
        ],
        |row| {
            let artifact_ref: String = row.get(0)?;
            let blob_ref: String = crate::codec::hash_column(row, 1)?;
            let stored: Option<Vec<u8>> = row.get(2)?;
            let encoding: Option<i64> = row.get(3)?;
            let payload = match stored {
                // A document or a declaration is its graph node, stored as
                // its encoded bytes with no blob envelope to unwrap.
                Some(stored) if encoding == Some(1) => DurablePayload::GraphNode(stored),
                Some(stored) => logical_json_payload(stored),
                None => DurablePayload::Missing {
                    reason: format!(
                        "module artifact `{artifact_ref}` points at blob `{blob_ref}`, \
                         which has no row in the blobs table"
                    ),
                },
            };
            Ok(DurableItem {
                surface: DurableSurface::ModuleArtifact,
                cursor: artifact_ref,
                process_id: None,
                session_id: None,
                status: None,
                owner_record: None,
                payload,
            })
        },
    )?;
    collect_page(rows, limit)
}

/// A `LIMIT` SQLite will accept, without a panicking conversion.
///
/// A `usize` wider than `i64` cannot describe a page anyone wants; saturating is
/// the honest reading of "as many as you can" and, unlike an `expect`, cannot
/// take the probe down over a caller's arithmetic.
fn limit_binding(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

/// `Some(last cursor)` exactly when the query filled the page.
///
/// A short page is the end of the surface, so it must not mint a cursor: a
/// caller that resumed after one would read the surface as unbounded and page
/// forever. A `limit` of zero returns nothing and ends nothing, and is handled
/// by the emptiness check rather than by arithmetic.
fn next_cursor(last: Option<String>, scanned_rows: usize, limit: usize) -> Option<String> {
    if scanned_rows == limit { last } else { None }
}

/// Every session that has published a checkpoint root, with the manifest blob
/// left-joined so a dangling reference survives as a row.
///
/// The join is `LEFT` on purpose. An inner join would have made a session whose
/// manifest blob has gone missing simply *disappear* from the walk — the single
/// most alarming finding a preflight can make, silently rendered as "no such
/// session". Left-joining keeps the row and lets the missing content be
/// reported as what it is.
fn read_session_checkpoints(
    conn: &Connection,
    after: Option<&str>,
    limit: usize,
) -> rusqlite::Result<(Vec<DurableItem>, Option<String>)> {
    let mut statement = conn.prepare_cached(
        crate::artifact_store::artifact_sql()
            .blobs_sqlite
            .select_session_checkpoint_page
            .sql(),
    )?;
    let rows = statement.query_map(params![after, limit_binding(limit)], |row| {
        let session_id: SessionId = crate::codec::sql_identity(row.get::<_, String>(0)?)?;
        let checkpoint_ref: String = crate::codec::hash_column(row, 1)?;
        let stored: Option<Vec<u8>> = row.get(2)?;
        let encoding: Option<i64> = row.get(3)?;
        let payload = match stored {
            // The manifest is a SessionManifest graph node, stored as its
            // encoded bytes with no blob envelope to unwrap.
            Some(stored) if encoding == Some(1) => DurablePayload::GraphNode(stored),
            Some(_) => DurablePayload::Missing {
                reason: format!(
                    "session `{session_id}` names checkpoint manifest `{checkpoint_ref}`, \
                     which is stored as an opaque blob, not a SessionManifest node"
                ),
            },
            None => DurablePayload::Missing {
                reason: format!(
                    "session `{session_id}` names checkpoint manifest `{checkpoint_ref}`, \
                     which has no row in the blobs table"
                ),
            },
        };
        Ok(DurableItem {
            surface: DurableSurface::SessionCheckpoint,
            cursor: session_id.clone().to_string(),
            process_id: None,
            session_id: Some(session_id),
            status: None,
            owner_record: None,
            payload,
        })
    })?;
    collect_page(rows, limit)
}

/// The protocol execution state of each session's checkpoint: its
/// SessionExecution root node.
///
/// Deliberately one node deep. The manifest names the root and this walk reads
/// exactly that node; it does not descend into the graph beneath it, because a
/// preflight that walked the graph would cost an unbounded number of reads per
/// session to answer a question one read already answers.
///
/// Two shapes of session contribute nothing here rather than an item. A manifest
/// that will not decode is a manifest this build cannot name a component in —
/// and its unreadability is already reported against
/// [`DurableSurface::SessionCheckpoint`], where the manifest itself is the item.
/// A manifest that names no SessionExecution root genuinely has no execution
/// state to park. Reporting either as `Missing` here would double-count one
/// defect and invent a second.
///
/// Because a page can therefore emit fewer items than it scanned sessions, the
/// resume cursor is minted from the last *session scanned*, never from the last
/// item emitted. Deriving it from the items would have quietly re-walked every
/// contribution-free session on the next page, or — where the tail of a page
/// contributed nothing — stopped the walk early with sessions still unread.
fn read_session_execution_state(
    conn: &Connection,
    after: Option<&str>,
    limit: usize,
) -> rusqlite::Result<(Vec<DurableItem>, Option<String>)> {
    let mut statement = conn.prepare_cached(
        crate::artifact_store::artifact_sql()
            .blobs_sqlite
            .select_session_checkpoint_page
            .sql(),
    )?;
    let rows = statement.query_map(params![after, limit_binding(limit)], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<Vec<u8>>>(2)?))
    })?;

    let mut items = Vec::new();
    let mut scanned_rows = 0usize;
    let mut last = None;
    for row in rows {
        let (session_id, stored_manifest) = row?;
        scanned_rows += 1;
        last = Some(session_id.clone());

        // A missing or unreadable manifest is the checkpoint surface's finding,
        // not this one's. See the doc comment.
        let Some(stored_manifest) = stored_manifest else {
            continue;
        };
        let Some(blob_ref) = execution_state_blob_ref(&stored_manifest) else {
            continue;
        };

        let payload = match load_blob(conn, &blob_ref)? {
            // A graph node is stored as its encoded bytes, with no blob
            // envelope to unwrap.
            Some(stored) => DurablePayload::GraphNode(stored),
            // The manifest decoded and named this blob, so unlike the cases
            // above there is a component here and it is gone. That is a finding
            // an operator has to see.
            None => DurablePayload::Missing {
                reason: format!(
                    "session `{session_id}`'s checkpoint manifest names execution-state blob \
                     `{blob_ref}`, which has no row in the blobs table"
                ),
            },
        };
        items.push(DurableItem {
            surface: DurableSurface::SessionExecutionState,
            cursor: session_id.clone(),
            process_id: None,
            session_id: Some(crate::codec::sql_identity(session_id)?),
            status: None,
            owner_record: None,
            payload,
        });
    }
    Ok((items, next_cursor(last, scanned_rows, limit)))
}

/// Deliberately *not* the crate's validating manifest load: that reads every
/// child and refuses drift, which is correct for a load path and exactly wrong
/// here. A preflight that refused to describe a checkpoint written by another
/// build would fail on the deployments it exists to describe. This reads the
/// SessionExecution root the manifest's body names, and nothing else.
///
/// Every step is fallible and every failure means the same thing: this session
/// names no execution state a walk can follow.
fn execution_state_blob_ref(manifest: &[u8]) -> Option<String> {
    let node = lash_core_store::checkpoint_graph::EncodedNode {
        id: lash_core_store::checkpoint_graph::NodeId::of_envelope(manifest),
        bytes: std::sync::Arc::from(manifest),
    };
    lash_core_store::store::SessionCheckpoint::named(&node)?
        .execution
        .map(|root| root.to_hex())
}

fn load_blob(conn: &Connection, blob_ref: &str) -> rusqlite::Result<Option<Vec<u8>>> {
    let mut statement = conn.prepare_cached(
        crate::artifact_store::artifact_sql()
            .blobs
            .select_content
            .sql(),
    )?;
    let mut rows = statement.query(params![crate::codec::SqlHash(blob_ref)])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

/// Unwrap this crate's storage framing and report what is left, or say why it
/// could not be unwrapped.
///
/// An envelope that will not inflate is a finding about one blob. Returning it
/// as `Missing` rather than an error is what lets the rest of the page — very
/// possibly the healthy majority of it — still reach the operator.
fn logical_json_payload(stored: Vec<u8>) -> DurablePayload {
    match logical_bytes(stored) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => DurablePayload::Json(text),
            Err(error) => DurablePayload::Missing {
                reason: format!("module artifact blob is not UTF-8 JSON: {error}"),
            },
        },
        Err(reason) => DurablePayload::Missing { reason },
    }
}

/// The stored bytes with this crate's envelope and compression removed.
///
/// A malformed envelope is returned as a reason so the caller can report the
/// item as [`DurablePayload::Missing`] without aborting the rest of the page.
fn logical_bytes(stored: Vec<u8>) -> Result<Vec<u8>, String> {
    crate::decode_artifact_blob(&stored).map_err(|error| error.to_string())
}

fn collect_page<I>(rows: I, limit: usize) -> rusqlite::Result<(Vec<DurableItem>, Option<String>)>
where
    I: Iterator<Item = rusqlite::Result<DurableItem>>,
{
    let items = rows.collect::<rusqlite::Result<Vec<DurableItem>>>()?;
    let last = items.last().map(|item| item.cursor.clone());
    let scanned_rows = items.len();
    Ok((items, next_cursor(last, scanned_rows, limit)))
}
