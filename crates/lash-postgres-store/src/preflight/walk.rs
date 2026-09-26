//! Enumerate a PostgreSQL deployment's durable payloads without opening it.
//!
//! The schema answer tells a host whether the *store* would open. It does not
//! tell it what is stranded behind a refusal, and that is the question a drain
//! list is built from: which processes are parked, which wakes are undelivered,
//! which sessions carry a checkpoint this build may not be able to read. This
//! module answers it the only way a preflight may — by reading.
//!
//! **Everything here is a plain `SELECT`.** No statement in this module creates,
//! stamps, migrates, locks or deletes, and none of them constructs a
//! [`PostgresStorage`](crate::PostgresStorage): building the store is the
//! side-effectful act the whole preflight surface exists to precede (advisory
//! lock, release-stamp write, signing-secret precondition, schema-gate
//! telemetry). The two deep surfaces need more than one statement to agree with
//! each other, so they run inside an explicitly `READ ONLY` transaction — the
//! same move SQLite's side makes with `PRAGMA query_only`, and for the same
//! reason: the read-only promise should be one the engine enforces rather than a
//! property of the statements this module happens to send today.
//!
//! **Nothing here decodes a payload.** The bytes a walk returns are handed back
//! framed but uninterpreted, because deciding whether they open under this build
//! is one build-wide question owned by the format manifest, and a backend that
//! answered it locally would be a second place for the answer to drift. The one
//! decode that does happen — the checkpoint manifest, to find the
//! execution-state component it names — is a *navigation* step, not a verdict:
//! it reads one reference out of a container and deliberately refuses to judge
//! what it found. See [`execution_state_ref`] for why it cannot use the crate's
//! strict decoder.
//!
//! **A page is bounded and a cursor is exact.** Every surface orders by a unique
//! key, filters `after` on that same key with the same comparison the `ORDER BY`
//! uses, and takes `LIMIT`. A preflight over a large deployment that read the
//! whole table would be the outage it was meant to prevent.
//!
//! **An unprovisioned deployment is still reportable.** A missing table
//! (`42P01`) is [`ScanCoverage::NotScanned`], not an error: the deployment most
//! worth describing is often the one that was never provisioned, and a walk that
//! failed on it would take the whole report down with it.

use crate::artifact_store::MODULE_ARTIFACT_NAMESPACE;
use lash_core_execution::{
    DurableItem, DurablePayload, DurableScan, DurableScanPage, DurableSurface, ScanCoverage,
    StoreError,
};
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use sqlx::postgres::PgPool;
use sqlx::{Postgres, Transaction};

/// PostgreSQL's `undefined_table`. A deployment that has not been provisioned
/// yet answers every surface with this, and answering it as a failure would mean
/// no report at all for exactly the deployments a preflight is most useful on.
const UNDEFINED_TABLE: &str = "42P01";

// Two of this walk's four page statements belong to the process family and are
// named in its PostgreSQL owner module (FIG-3384):
//
// * `process_segment_handover.list_parked_segments` orders parked segments by
//   `(process_id, segment_ordinal)` — the table's primary key, and therefore a
//   total order with no ties to break — and joins in only `running` and
//   `waiting` processes. Terminal processes are excluded at the source rather
//   than filtered afterwards: their handover rows are historical residue, and
//   listing them on a drain list would send an operator after continuations
//   that nothing will ever resume. Its `after` filter is a row-value
//   comparison against the same two columns the `ORDER BY` uses, not a
//   comparison of a minted cursor string: string comparison of a joined cursor
//   does not always agree with tuple comparison of its parts, and comparing
//   the columns themselves cannot disagree with the ordering of the same
//   columns under any collation.
// * `process_wake_delivery.list_undelivered_for_walk` reports only `pending`
//   and `enqueuing` deliveries — the two states the delivery loop treats as
//   still owed, since `wake_delivery.rs` reclaims `enqueuing` back to
//   `pending` when a claim lapses. Anything else has already left the queue,
//   and putting it on a drain list would be reporting work that is done.

/// The entry point [`crate::PostgresStorePreflight`] delegates to; every branch
/// returns a page, and `Err` is reserved for a server that could not answer at
/// all.
pub(crate) async fn scan_durable(
    pool: &PgPool,
    scan: &DurableScan,
) -> Result<DurableScanPage, StoreError> {
    match scan.surface {
        DurableSurface::ModuleArtifact => scan_module_artifacts(pool, scan).await,
        DurableSurface::ParkedSegment => scan_parked_segments(pool, scan).await,
        DurableSurface::PendingWake => scan_pending_wakes(pool, scan).await,
        DurableSurface::StartedProcess => scan_started_processes(pool, scan).await,
        DurableSurface::SessionCheckpoint => scan_session_checkpoints(pool, scan).await,
        DurableSurface::SessionExecutionState => scan_session_execution_state(pool, scan).await,
        // The surface set is `#[non_exhaustive]`, so a build against a newer
        // lash-core can name one this backend has never heard of. Saying so is
        // the only honest answer: an empty page would read as "nothing here
        // refuses" for a surface nobody walked.
        surface => Ok(DurableScanPage {
            items: Vec::new(),
            next: None,
            coverage: ScanCoverage::NotScanned {
                reason: format!("the postgres backend does not enumerate {}", surface.name()),
            },
        }),
    }
}

/// One start record per live process (FIG-3571), in key order. The payload is
/// the process record: its start stamp names the executable generation the
/// process runs under, and only its input lets the probe recompute the
/// generation this build would run it as.
async fn scan_started_processes(
    pool: &PgPool,
    scan: &DurableScan,
) -> Result<DurableScanPage, StoreError> {
    let rows = sqlx::query_as::<_, (String, String, Option<String>, String)>(
        crate::process_sql::process_sql()
            .process_postgres
            .list_live_for_preflight
            .sql(),
    )
    .bind(scan.after.clone())
    .bind(row_limit(scan))
    .fetch_all(pool)
    .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => return read_failure(scan.surface, error),
    };
    let returned = rows.len();
    let items: Vec<DurableItem> = rows
        .into_iter()
        .map(
            |(process_id, status, wake_session_id, record_json)| DurableItem {
                surface: DurableSurface::StartedProcess,
                process_id: ProcessId::parse(&process_id).ok(),
                cursor: process_id,
                session_id: wake_session_id.map(SessionId::from),
                status: Some(status),
                owner_record: Some(record_json.clone()),
                payload: DurablePayload::Json(record_json),
            },
        )
        .collect();
    let next = page_cursor(scan, items.last().map(|item| item.cursor.clone()), returned);
    Ok(scanned(items, next))
}

/// One persisted JSON module artifact per module reference.
async fn scan_module_artifacts(
    pool: &PgPool,
    scan: &DurableScan,
) -> Result<DurableScanPage, StoreError> {
    let rows = sqlx::query_as::<_, (String, Vec<u8>)>(
        crate::artifact_store::artifact_sql()
            .lashlang_artifacts
            .list_namespace_page
            .sql(),
    )
    .bind(MODULE_ARTIFACT_NAMESPACE)
    .bind(scan.after.clone())
    .bind(row_limit(scan))
    .fetch_all(pool)
    .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => return read_failure(scan.surface, error),
    };
    let returned = rows.len();
    let items: Vec<DurableItem> = rows
        .into_iter()
        .map(|(artifact_ref, bytes)| DurableItem {
            surface: DurableSurface::ModuleArtifact,
            cursor: artifact_ref,
            process_id: None,
            session_id: None,
            status: None,
            owner_record: None,
            payload: match String::from_utf8(bytes) {
                Ok(text) => DurablePayload::Json(text),
                Err(error) => DurablePayload::Missing {
                    reason: format!("module artifact blob is not UTF-8 JSON: {error}"),
                },
            },
        })
        .collect();
    let next = page_cursor(scan, items.last().map(|item| item.cursor.clone()), returned);
    Ok(scanned(items, next))
}

/// One parked segment-handover envelope per live process that has one.
async fn scan_parked_segments(
    pool: &PgPool,
    scan: &DurableScan,
) -> Result<DurableScanPage, StoreError> {
    let (after_process, after_ordinal) = match scan.after.as_deref() {
        Some(cursor) => {
            let (process_id, ordinal) = split_segment_cursor(cursor)?;
            (Some(process_id), Some(ordinal))
        }
        None => (None, None),
    };
    let rows = sqlx::query_as::<_, ParkedSegmentRow>(
        crate::process_sql::process_sql()
            .handover_postgres
            .list_parked_segments
            .sql(),
    )
    .bind(after_process)
    .bind(after_ordinal)
    .bind(row_limit(scan))
    .fetch_all(pool)
    .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => return read_failure(scan.surface, error),
    };

    let returned = rows.len();
    let items: Vec<DurableItem> = rows
        .into_iter()
        .map(
            |(process_id, segment_ordinal, handover_json, status, wake_session_id, record_json)| {
                let process_id = ProcessId::parse(&process_id).ok();
                DurableItem {
                    surface: DurableSurface::ParkedSegment,
                    cursor: segment_cursor(process_id.as_ref(), segment_ordinal),
                    process_id,
                    // The session the process wakes into, which is the identity
                    // an operator draining a stuck continuation looks for.
                    session_id: wake_session_id.map(SessionId::from),
                    status: Some(status),
                    // The registry record travels with the item because an
                    // identity-only durable format cannot be checked from the
                    // payload alone: recomputing a stored program identity
                    // needs the inputs only the owner's record holds.
                    owner_record: Some(record_json),
                    payload: DurablePayload::Json(handover_json),
                }
            },
        )
        .collect();
    let next = page_cursor(scan, items.last().map(|item| item.cursor.clone()), returned);
    Ok(scanned(items, next))
}

/// One undelivered wake payload per pending delivery.
async fn scan_pending_wakes(
    pool: &PgPool,
    scan: &DurableScan,
) -> Result<DurableScanPage, StoreError> {
    let rows = sqlx::query_as::<_, PendingWakeRow>(
        crate::process_sql::process_sql()
            .wake_postgres
            .list_undelivered_for_walk
            .sql(),
    )
    .bind(scan.after.clone())
    .bind(row_limit(scan))
    .fetch_all(pool)
    .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => return read_failure(scan.surface, error),
    };

    let returned = rows.len();
    let items: Vec<DurableItem> = rows
        .into_iter()
        .map(
            |(delivery_id, process_id, target_session_id, state, delivery_json)| DurableItem {
                surface: DurableSurface::PendingWake,
                cursor: delivery_id,
                process_id: ProcessId::parse(&process_id).ok(),
                session_id: Some(SessionId::from(target_session_id)),
                // The delivery's own state word, verbatim: an operator reading
                // `enqueuing` learns the claim lapsed mid-flight, which a
                // translation to "pending" would have hidden.
                status: Some(state),
                // A delivery owns no separate record; the payload is the whole
                // of it, and inventing an owner record here would put the same
                // bytes on the report twice.
                owner_record: None,
                payload: DurablePayload::Json(delivery_json),
            },
        )
        .collect();
    let next = page_cursor(scan, items.last().map(|item| item.cursor.clone()), returned);
    Ok(scanned(items, next))
}

/// One checkpoint manifest per session that has published a checkpoint root.
///
/// **The blob content is the logical payload, with no unwrapping to do.** The
/// PostgreSQL write path (`postgres/support.rs`, `put_checkpoint_tx`) encodes
/// the manifest with `encode_msgpack` and stores that buffer directly through
/// `put_blob_tx` — `lash_blobs.content` is the msgpack record itself, with no
/// envelope, framing or compression around it. Unlike SQLite, which wraps stored
/// blobs in a frame of its own, this backend has nothing to strip, and stripping
/// something anyway would corrupt every payload the report carries.
async fn scan_session_checkpoints(
    pool: &PgPool,
    scan: &DurableScan,
) -> Result<DurableScanPage, StoreError> {
    let mut snapshot = read_only_snapshot(pool).await?;
    let sessions = match fetch_sessions(&mut snapshot, scan).await {
        Ok(sessions) => sessions,
        Err(error) => return finish(snapshot, read_failure(scan.surface, error)).await,
    };
    let refs: Vec<String> = sessions
        .iter()
        .map(|session| session.checkpoint_ref.clone())
        .collect();
    let blobs = match fetch_blobs(&mut snapshot, &refs).await {
        Ok(blobs) => blobs,
        Err(error) => return finish(snapshot, read_failure(scan.surface, error)).await,
    };

    let items: Vec<DurableItem> = sessions
        .iter()
        .map(|session| DurableItem {
            surface: DurableSurface::SessionCheckpoint,
            cursor: session.session_id.clone().to_string(),
            process_id: None,
            session_id: Some(session.session_id.clone()),
            status: None,
            owner_record: None,
            payload: match blobs.get(session.checkpoint_ref.as_str()) {
                Some(content) => DurablePayload::MessagePack(content.clone()),
                // A dangling root is a finding, not an error and not an
                // omission: the session is named on the report with the ref it
                // points at, which is the only form of this defect an operator
                // can chase.
                None => DurablePayload::Missing {
                    reason: format!(
                        "session `{}` points at checkpoint blob `{}`, which is absent from \
                         lash_blobs",
                        session.session_id, session.checkpoint_ref
                    ),
                },
            },
        })
        .collect();
    let next = page_cursor(
        scan,
        sessions
            .last()
            .map(|session| session.session_id.to_string()),
        sessions.len(),
    );
    finish(snapshot, Ok(scanned(items, next))).await
}

/// One protocol execution-state component body per session that stores one.
///
/// This is the session walk one level deeper: read the manifest, follow the
/// reference it holds under the `execution_state` key, and return that blob's
/// bytes. Two properties of the result are load-bearing.
///
/// **A page can be shorter than the sessions it scanned.** A session whose
/// manifest names no execution-state component contributes no item — that
/// session genuinely has none, and emitting an empty or `Missing` item for it
/// would invent a defect. The page's `next` is therefore taken from the *last
/// session scanned*, never from the last item emitted: a page of ten sessions
/// that yielded one item must resume after the tenth, and resuming after the
/// first would walk the other nine forever.
///
/// **Only the named component is read.** Execution state is stored as a
/// component whose body may itself be split into leaves keyed
/// `execution_state/…`; those are the component's internals and reading them
/// here would turn a bounded per-session read into an unbounded fan-out, on a
/// surface whose entire justification is that it is bounded.
async fn scan_session_execution_state(
    pool: &PgPool,
    scan: &DurableScan,
) -> Result<DurableScanPage, StoreError> {
    let mut snapshot = read_only_snapshot(pool).await?;
    let sessions = match fetch_sessions(&mut snapshot, scan).await {
        Ok(sessions) => sessions,
        Err(error) => return finish(snapshot, read_failure(scan.surface, error)).await,
    };
    let manifest_refs: Vec<String> = sessions
        .iter()
        .map(|session| session.checkpoint_ref.clone())
        .collect();
    let manifests = match fetch_blobs(&mut snapshot, &manifest_refs).await {
        Ok(manifests) => manifests,
        Err(error) => return finish(snapshot, read_failure(scan.surface, error)).await,
    };

    // A manifest blob that is itself absent drops out here too: the dangling root is already a
    // `Missing` item on the `SessionCheckpoint` surface, and reporting it a second time as an
    // absent execution state would claim a component the store never said existed.
    let mut resolved: Vec<(String, String)> = Vec::with_capacity(sessions.len());
    for session in &sessions {
        let Some(manifest) = manifests.get(session.checkpoint_ref.as_str()) else {
            continue;
        };
        if let Some(blob_ref) = execution_state_ref(manifest) {
            resolved.push((session.session_id.clone().to_string(), blob_ref));
        }
    }
    let component_refs: Vec<String> = resolved
        .iter()
        .map(|(_session_id, blob_ref)| blob_ref.clone())
        .collect();
    let components = match fetch_blobs(&mut snapshot, &component_refs).await {
        Ok(components) => components,
        Err(error) => return finish(snapshot, read_failure(scan.surface, error)).await,
    };

    let items: Vec<DurableItem> = resolved
        .iter()
        .map(|(session_id, blob_ref)| DurableItem {
            surface: DurableSurface::SessionExecutionState,
            cursor: session_id.clone(),
            process_id: None,
            session_id: Some(SessionId::from(session_id.clone())),
            status: None,
            owner_record: None,
            payload: match components.get(blob_ref.as_str()) {
                Some(content) => DurablePayload::MessagePack(content.clone()),
                // The manifest named it, so the store believes it exists. That
                // makes its absence a real dangling reference rather than the
                // "this session has none" case above.
                None => DurablePayload::Missing {
                    reason: format!(
                        "session `{session_id}` names execution-state blob `{blob_ref}`, which is \
                         absent from lash_blobs"
                    ),
                },
            },
        })
        .collect();
    // Deliberately the last *session*, not the last item — see the doc comment.
    let next = page_cursor(
        scan,
        sessions
            .last()
            .map(|session| session.session_id.to_string()),
        sessions.len(),
    );
    finish(snapshot, Ok(scanned(items, next))).await
}

/// **Why not the crate's `decode_versioned_msgpack_record`.** That helper
/// validates the record's schema version and fails when it disagrees with this
/// build — which is precisely the condition a preflight exists to *describe*. A
/// walk that used it would refuse to enumerate the deployment whose format
/// mismatch is the finding, turning the most valuable report into an error.
///
/// **Why a bespoke probe rather than a generic value tree.** Reading the
/// manifest into `serde_json::Value` cannot represent MessagePack's binary type,
/// so any byte-carrying field anywhere in the record — present or added later —
/// would fail the whole decode and silently drop a session that does have
/// execution state. A struct that names only what is navigated lets serde ignore
/// everything else, whatever type it is.
///
/// A manifest that will not decode at all contributes nothing rather than
/// erroring: it is one session's container, and failing the page over it would
/// hide every session behind it.
fn execution_state_ref(manifest: &[u8]) -> Option<String> {
    let probe: ManifestProbe = rmp_serde::from_slice(manifest).ok()?;
    probe
        .components
        .get(lash_core_execution::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
        .map(|component| component.blob_ref.clone())
}

/// The narrowest possible view of a checkpoint manifest: the one map, and the
/// one field of the one entry, that navigation needs. Everything else is
/// ignored by construction.
#[derive(serde::Deserialize)]
struct ManifestProbe {
    #[serde(default)]
    components: std::collections::BTreeMap<String, ManifestComponentProbe>,
}

#[derive(serde::Deserialize)]
struct ManifestComponentProbe {
    blob_ref: String,
}

/// One page of sessions holding a checkpoint root, shared by both deep surfaces.
async fn fetch_sessions(
    snapshot: &mut Transaction<'_, Postgres>,
    scan: &DurableScan,
) -> Result<Vec<SessionCheckpointRow>, sqlx::Error> {
    // One statement per filter shape, chosen exhaustively. A single statement
    // carrying `$1::text IS NULL OR session_id > $1::text` cannot seek on
    // `session_id`, so every page of the walk this exists to bound would scan
    // the whole table.
    let sql = crate::session_sql::session_sql();
    let query = match scan.after.as_deref() {
        None => sqlx::query_as::<_, (String, String)>(sql.head.scan_checkpoints_first_page.sql())
            .bind(row_limit(scan)),
        Some(after) => sqlx::query_as::<_, (String, String)>(sql.head.scan_checkpoints_after.sql())
            .bind(after.to_string())
            .bind(row_limit(scan)),
    };
    let rows = query.fetch_all(&mut **snapshot).await?;
    Ok(rows
        .into_iter()
        .map(|(session_id, checkpoint_ref)| SessionCheckpointRow {
            session_id: SessionId::from(session_id),
            checkpoint_ref,
        })
        .collect())
}

/// Fetch the requested blobs, keyed by hash. Absent hashes are simply missing
/// from the map — the caller turns that into a reportable
/// [`DurablePayload::Missing`] naming the reference, which an error here could
/// not do.
async fn fetch_blobs(
    snapshot: &mut Transaction<'_, Postgres>,
    hashes: &[String],
) -> Result<std::collections::HashMap<String, Vec<u8>>, sqlx::Error> {
    if hashes.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    // One statement per session would turn a hundred-session page into a
    // hundred round trips against a server the host has not decided to depend
    // on yet.
    let rows = sqlx::query_as::<_, (String, Vec<u8>)>(
        crate::blobs::blob_sql()
            .postgres
            .select_bodies_by_hash
            .sql(),
    )
    .bind(hashes)
    .fetch_all(&mut **snapshot)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Begin a transaction that the server itself will refuse to let write.
///
/// `READ ONLY` makes this module's read-only promise an engine-enforced
/// invariant rather than a property of the statements it happens to send, and
/// `REPEATABLE READ` is what lets a manifest and the blob it names come from one
/// snapshot: under read-committed, a concurrent GC between the two statements
/// would make a perfectly healthy store report a dangling reference.
async fn read_only_snapshot(pool: &PgPool) -> Result<Transaction<'_, Postgres>, StoreError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| StoreError::StorageFailure {
            backend: "postgres",
            message: error.to_string(),
        })?;
    sqlx::query(
        crate::connection_sql::connection_sql()
            .begin_repeatable_read_read_only
            .sql(),
    )
    .execute(&mut *transaction)
    .await
    .map_err(|error| StoreError::StorageFailure {
        backend: "postgres",
        message: error.to_string(),
    })?;
    Ok(transaction)
}

/// End the snapshot and hand back the page.
///
/// The rollback's own result is dropped on purpose: the transaction wrote
/// nothing, so there is no outcome for it to report, and failing a completed
/// report on the way out would discard findings that are already in hand.
async fn finish(
    snapshot: Transaction<'_, Postgres>,
    page: Result<DurableScanPage, StoreError>,
) -> Result<DurableScanPage, StoreError> {
    let _ = snapshot.rollback().await;
    page
}

/// A page whose surface was genuinely read.
fn scanned(items: Vec<DurableItem>, next: Option<String>) -> DurableScanPage {
    DurableScanPage {
        items,
        next,
        coverage: ScanCoverage::Scanned,
    }
}

/// Turn a read failure into either an unwalked surface or a hard error.
///
/// A missing table means this deployment has not been provisioned, which is a
/// coverage answer; anything else means the server could not answer, which
/// leaves nothing to report and so is the `Result`'s job.
fn read_failure(
    surface: DurableSurface,
    error: sqlx::Error,
) -> Result<DurableScanPage, StoreError> {
    if let sqlx::Error::Database(database) = &error
        && database.code().as_deref() == Some(UNDEFINED_TABLE)
    {
        return Ok(DurableScanPage {
            items: Vec::new(),
            next: None,
            coverage: ScanCoverage::NotScanned {
                reason: format!(
                    "{} are not enumerable in this deployment: {}",
                    surface.name(),
                    database.message()
                ),
            },
        });
    }
    Err(StoreError::StorageFailure {
        backend: "postgres",
        message: error.to_string(),
    })
}

/// The `LIMIT` binding.
///
/// A limit beyond `i64::MAX` saturates rather than wrapping: a wrapped negative
/// limit is a syntax error at the server, and a caller asking for more rows than
/// exist is asking for all of them anyway.
fn row_limit(scan: &DurableScan) -> i64 {
    i64::try_from(scan.limit).unwrap_or(i64::MAX)
}

/// The cursor to resume after.
///
/// `Some` exactly when the query returned a full page, because a short page is
/// the only evidence a keyset walk has that it reached the end. `last` is
/// `None` only for an empty page, which cannot be a full one unless the caller
/// asked for zero rows — and a zero-row page has no row to resume after.
fn page_cursor(scan: &DurableScan, last: Option<String>, returned: usize) -> Option<String> {
    if returned == scan.limit { last } else { None }
}

/// Mint a parked-segment cursor.
///
/// The ordinal is zero-padded so the cursor reads in the same order the rows
/// do, which keeps a cursor an operator sees in a report meaningful rather than
/// arbitrary. Paging itself never relies on that: see the family's
/// `process_segment_handover.list_parked_segments`.
fn segment_cursor(process_id: Option<&ProcessId>, segment_ordinal: i64) -> String {
    let process_id = process_id.map_or("", ProcessId::as_str);
    format!("{process_id}:{segment_ordinal:020}")
}

/// Split at the **last** separator: a process id may itself contain one, while
/// the fixed-width decimal ordinal never can, so the last separator is always
/// the one this function put there.
///
/// A cursor that does not parse is an error rather than a silently-ignored
/// filter. Ignoring it would restart the walk at the beginning while reporting
/// the page as a continuation, which duplicates every earlier item — the exact
/// failure the caller was paging to avoid.
fn split_segment_cursor(cursor: &str) -> Result<(String, i64), StoreError> {
    let Some((process_id, ordinal)) = cursor.rsplit_once(':') else {
        return Err(invalid_cursor(cursor));
    };
    let ordinal: i64 = ordinal.parse().map_err(|_| invalid_cursor(cursor))?;
    Ok((process_id.to_string(), ordinal))
}

fn invalid_cursor(cursor: &str) -> StoreError {
    StoreError::Backend(format!(
        "`{cursor}` is not a parked-segment cursor minted by this backend"
    ))
}

/// `(process_id, segment_ordinal, handover_json, status, wake_session_id,
/// record_json)`, in the order `process_segment_handover.list_parked_segments`
/// selects them.
///
/// Rows are decoded positionally rather than through a derived `FromRow`: this
/// crate does not enable sqlx's `derive` feature, and the alias keeps the column
/// order the query fixes visible next to the query itself.
type ParkedSegmentRow = (String, i64, String, String, Option<String>, String);

/// `(delivery_id, process_id, target_session_id, state, delivery_json)`, in the
/// order `process_wake_delivery.list_undelivered_for_walk` selects them.
type PendingWakeRow = (String, String, String, String, String);

/// A session that has published a checkpoint root, named rather than positional
/// because both deep surfaces pass it around well away from its query.
struct SessionCheckpointRow {
    session_id: SessionId,
    checkpoint_ref: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parked_segment_join_uses_index_order_without_sort() {
        let Some(url) = crate::postgres_test_support::database_url() else {
            return;
        };
        let _lock = crate::postgres_test_support::SharedDatabaseLock::acquire(&url).await;
        let storage = crate::PostgresStorage::connect(&url)
            .await
            .expect("connect planner witness");
        let mut tx = storage.pool().begin().await.expect("begin planner witness");
        // As in the worklist planner witness, remove small-table cost preference.
        // Disable alternative joins to prove the existing btrees can supply merge
        // order directly; a collation mismatch still requires an explicit sort.
        for setting in [
            "SET LOCAL enable_seqscan = off",
            "SET LOCAL enable_bitmapscan = off",
            "SET LOCAL enable_hashjoin = off",
            "SET LOCAL enable_nestloop = off",
        ] {
            sqlx::query(setting)
                .execute(&mut *tx)
                .await
                .expect("set planner witness preference");
        }
        let plan = sqlx::query_scalar::<_, String>(&format!(
            "EXPLAIN (COSTS OFF) {}",
            crate::process_sql::process_sql()
                .handover_postgres
                .list_parked_segments
                .sql()
        ))
        .bind(None::<String>)
        .bind(0_i64)
        .bind(65_i64)
        .fetch_all(&mut *tx)
        .await
        .expect("explain parked segment walk")
        .join(" | ");
        eprintln!("parked segment plan: {plan}");
        assert!(
            plan.contains("Merge Join")
                && plan.contains("lash_process_segment_handovers_pkey")
                && !plan.contains("Sort"),
            "parked segment merge join must inherit btree order without sorting: {plan}"
        );
        tx.rollback().await.expect("rollback planner witness");
    }

    #[test]
    fn a_minted_segment_cursor_round_trips_through_its_split() {
        let process_id = ProcessId::fixture("proc-7");
        let cursor = segment_cursor(Some(&process_id), 42);
        assert_eq!(cursor, format!("{process_id}:00000000000000000042"));
        assert_eq!(
            split_segment_cursor(&cursor).expect("a minted cursor parses"),
            (process_id.to_string(), 42)
        );
    }

    #[test]
    fn a_cursor_this_backend_did_not_mint_is_refused_rather_than_ignored() {
        // Ignoring it would silently restart the walk and re-emit every item
        // the caller already has.
        assert!(split_segment_cursor("proc-7").is_err());
        assert!(split_segment_cursor("proc-7:not-a-number").is_err());
    }

    #[test]
    fn a_short_page_ends_the_walk_and_a_full_one_continues_it() {
        let scan = DurableScan::first(DurableSurface::PendingWake, 2);
        assert_eq!(
            page_cursor(&scan, Some("b".to_string()), 2),
            Some("b".into())
        );
        assert_eq!(page_cursor(&scan, Some("b".to_string()), 1), None);
        assert_eq!(page_cursor(&scan, None, 0), None);
        // A zero-row request has no row to resume after, even though it
        // trivially "filled" its page.
        let zero = DurableScan::first(DurableSurface::PendingWake, 0);
        assert_eq!(page_cursor(&zero, None, 0), None);
    }

    #[test]
    fn a_manifest_without_execution_state_names_nothing() {
        #[derive(serde::Serialize)]
        struct Manifest {
            schema_version: u32,
            components: std::collections::BTreeMap<String, Component>,
        }
        #[derive(Clone, serde::Serialize)]
        struct Component {
            blob_ref: String,
            encoding_version: u32,
        }

        let mut components = std::collections::BTreeMap::new();
        components.insert(
            "tool_state".to_string(),
            Component {
                blob_ref: "aaaa".to_string(),
                encoding_version: 2,
            },
        );
        let mut buf = Vec::new();
        rmp_serde::encode::write_named(
            &mut buf,
            &Manifest {
                schema_version: 1,
                components: components.clone(),
            },
        )
        .expect("encode probe manifest");
        assert_eq!(execution_state_ref(&buf), None);

        components.insert(
            lash_core_execution::store::EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string(),
            Component {
                blob_ref: "bbbb".to_string(),
                encoding_version: 2,
            },
        );
        let mut buf = Vec::new();
        rmp_serde::encode::write_named(
            &mut buf,
            &Manifest {
                schema_version: 1,
                components,
            },
        )
        .expect("encode probe manifest");
        assert_eq!(execution_state_ref(&buf), Some("bbbb".to_string()));
    }

    #[test]
    fn an_undecodable_manifest_names_nothing_rather_than_failing() {
        assert_eq!(execution_state_ref(b"not messagepack at all"), None);
    }
}
