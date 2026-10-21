//! Module artifacts held and reclaimed as sets (FIG-6457, FIG-6467).
//!
//! A cell's frame retains the declaration of every helper its cells list,
//! some hundreds of artifacts, and a cell holds its document under its
//! execution, whose end releases it (FIG-6481). A hold runs as a handful of
//! statements over the set: one read of the acquisitions the referrer
//! already holds, which it skips, one read of the pointers already stored,
//! bounded multi-row blob inserts, one install of the graph nodes a
//! document's or a declaration's pointer roots (FIG-6469), one pointer
//! insert and one edge insert; a
//! referrer's end runs one read of the edges still held, one delete of every
//! pointer no edge holds and one peel of their blobs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use lash_core_store::artifact_referrer::ArtifactReferrerKind;
use lash_core_store::artifact_referrer::ReferrerClaim;
use lash_execution_contracts::module_artifacts::ArtifactStoreError;
use lash_execution_contracts::module_nodes::stage_module_node;

use super::*;

lash_store_sql::statements! {
    /// Set statements over `artifact_refs` and `artifact_referrer_edges`:
    /// SQLite binds a set as the JSON array `json_each` reads.
    pub(crate) struct HeldSqliteStatements @ "artifact_held" {
        /// The blob each reference of JSON `?2` in namespace `?1` points at.
        select_blob_refs = "SELECT artifact_ref, blob_ref FROM artifact_refs
             WHERE namespace = ?1 AND artifact_ref IN (SELECT value FROM json_each(?2))";

        /// Each reference of JSON `?2` in namespace `?1` that referrer
        /// `?3`/`?4` holds no edge to.
        select_unheld = "SELECT value FROM json_each(?2) AS wanted
             WHERE NOT EXISTS (
                 SELECT 1 FROM artifact_referrer_edges AS edge
                 WHERE edge.namespace = ?1 AND edge.artifact_ref = wanted.value
                   AND edge.referrer_kind = ?3 AND edge.referrer_id = ?4
             )";

        /// Point each `[reference, blob]` of JSON `?2` in namespace `?1`. An
        /// artifact is immutable: a pointer already there is kept.
        insert_pointers = "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref)
             SELECT ?1, value ->> 0, unhex(value ->> 1) FROM json_each(?2) WHERE true
             ON CONFLICT (namespace, artifact_ref) DO NOTHING";

        /// Referrer `?3`/`?4`'s edge to each reference of JSON `?2` in
        /// namespace `?1`.
        insert_edges = "INSERT INTO artifact_referrer_edges
             (namespace, artifact_ref, referrer_kind, referrer_id)
             SELECT ?1, value, ?3, ?4 FROM json_each(?2) WHERE true
             ON CONFLICT DO NOTHING";

        /// Every edge still held to a reference of JSON `?2` in namespace `?1`.
        select_edges = "SELECT artifact_ref, referrer_kind, referrer_id
             FROM artifact_referrer_edges
             WHERE namespace = ?1 AND artifact_ref IN (SELECT value FROM json_each(?2))";

        /// Delete the pointer of each reference of JSON `?2` in namespace
        /// `?1` that no edge holds, and answer the blob it pointed at.
        /// `NOT EXISTS` is the whole safety argument: a referrer acquired
        /// after the edges were read keeps its pointer.
        delete_unreferenced = "DELETE FROM artifact_refs AS doomed
             WHERE doomed.namespace = ?1
               AND doomed.artifact_ref IN (SELECT value FROM json_each(?2))
               AND NOT EXISTS (
                   SELECT 1 FROM artifact_referrer_edges AS edge
                   WHERE edge.namespace = doomed.namespace
                     AND edge.artifact_ref = doomed.artifact_ref
               )
             RETURNING blob_ref";
    }
}

/// Rows per multi-row blob insert: 64 bound parameters per full statement.
const BLOB_BATCH_ROWS: usize = 32;

struct HeldSql {
    held: HeldSqliteStatements,
    /// The multi-row `insert_ignore` for 1 to [`BLOB_BATCH_ROWS`] rows.
    blob_batches: [String; BLOB_BATCH_ROWS],
}

static HELD_SQL: LazyLock<HeldSql> = LazyLock::new(|| {
    let one_row = artifact_sql().blobs_sqlite.insert_ignore.sql();
    HeldSql {
        held: HeldSqliteStatements::render(crate::schema_layout::MAIN),
        blob_batches: std::array::from_fn(|index| {
            let values = (0..=index)
                .map(|row| format!("(?{}, ?{}, 0)", row * 2 + 1, row * 2 + 2))
                .collect::<Vec<_>>()
                .join(", ");
            one_row.replacen("(?1, ?2, 0)", &values, 1)
        }),
    }
});

/// `values` as the JSON array a `json_each` statement reads.
fn json_set(values: &impl serde::Serialize) -> rusqlite::Result<String> {
    serde_json::to_string(values)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

/// One module artifact a batch holds: its reference, its bytes, and
/// whether stored bytes must equal them.
pub(super) struct HeldArtifact {
    pub(super) artifact_ref: String,
    pub(super) bytes: Option<Vec<u8>>,
    /// The graph node kind the body is stored as; none for bytes.
    pub(super) node: Option<lash_core_store::checkpoint_graph::NodeKind>,
    pub(super) verified: bool,
}

impl SqliteStore {
    /// Hold every module artifact under the claim in one transaction: the
    /// referrer's fence and the claim's guard once, then each artifact
    /// stored as its hold says, then the claim's edge to each. An
    /// acquisition the referrer already holds writes nothing: its edge
    /// keeps the stored bytes, whatever they are.
    pub(super) async fn hold_module_artifact_batch(
        &self,
        claim: ReferrerClaim,
        artifacts: Vec<HeldArtifact>,
    ) -> Result<(), ArtifactStoreError> {
        if !claim.referrer().kind().holds_artifacts() {
            return Err(ArtifactStoreError::ReferrerKindRefused {
                kind: claim.referrer().kind(),
            });
        }
        let blob_profile = self.options.blob_profile;
        let images = Arc::clone(&self.encoded_images);
        let now_ms = self.clock.timestamp_ms();
        self.conn
            .write(move |tx| {
                let referrer = &claim.referrer();
                if artifact_fenced_tx(tx, referrer)? {
                    return Err(artifact_failure(ArtifactStoreError::ReferrerEnded {
                        referrer: referrer.clone(),
                    }));
                }
                if let Some(cleanup) = claim.guard_cleanup() {
                    crate::obligation_ledger::arm_cleanup_tx(tx, &cleanup, now_ms)
                        .map_err(sqlite_conversion_error)?;
                }
                let acquired: Vec<&str> = artifacts
                    .iter()
                    .filter(|artifact| !artifact.verified)
                    .map(|artifact| artifact.artifact_ref.as_str())
                    .collect();
                let unheld: BTreeSet<String> = if acquired.is_empty() {
                    BTreeSet::new()
                } else {
                    tx.prepare_cached(HELD_SQL.held.select_unheld.sql())?
                        .query_map(
                            params![
                                MODULE_ARTIFACT_NAMESPACE,
                                json_set(&acquired)?,
                                referrer.kind().as_str(),
                                referrer.canonical_id()
                            ],
                            |row| row.get(0),
                        )?
                        .collect::<rusqlite::Result<_>>()?
                };
                let artifacts: Vec<&HeldArtifact> = artifacts
                    .iter()
                    .filter(|artifact| artifact.verified || artifact.bytes.is_none() || unheld.contains(&artifact.artifact_ref))
                    .collect();
                if artifacts.is_empty() {
                    return Ok(());
                }
                let refs = json_set(
                    &artifacts
                        .iter()
                        .map(|artifact| artifact.artifact_ref.as_str())
                        .collect::<Vec<_>>(),
                )?;
                let mut stored: BTreeMap<String, String> = tx
                    .prepare_cached(HELD_SQL.held.select_blob_refs.sql())?
                    .query_map(params![MODULE_ARTIFACT_NAMESPACE, refs], |row| {
                        Ok((row.get(0)?, crate::codec::hash_column(row, 1)?))
                    })?
                    .collect::<rusqlite::Result<_>>()?;
                let mut pointers = Vec::new();
                let mut blobs = Vec::new();
                let mut nodes = BTreeMap::new();
                for artifact in artifacts.iter().copied() {
                    let Some(bytes) = artifact.bytes.as_deref() else {
                        if !stored.contains_key(&artifact.artifact_ref) {
                            return Err(artifact_failure(ArtifactStoreError::ArtifactMissing { artifact_ref: artifact.artifact_ref.clone() }));
                        }
                        continue;
                    };
                    match (stored.get(&artifact.artifact_ref), artifact.node) {
                        (Some(_), _) if !artifact.verified => {}
                        (Some(blob_ref), node) => {
                            let expected = match node {
                                None => BlobRef::for_content(bytes).0,
                                Some(kind) => stage_module_node(kind, bytes)
                                    .map_err(artifact_failure)?
                                    .root
                                    .to_hex(),
                            };
                            if *blob_ref != expected {
                                return Err(artifact_failure(ArtifactStoreError::Immutable {
                                    artifact_ref: artifact.artifact_ref.clone(),
                                }));
                            }
                        }
                        // A node body is stored once, as its graph node,
                        // which the artifact's pointer roots.
                        (None, Some(kind)) => {
                            let staged = stage_module_node(kind, bytes)
                                .map_err(artifact_failure)?;
                            let root = staged.root.to_hex();
                            stored.insert(artifact.artifact_ref.clone(), root.clone());
                            pointers.push((artifact.artifact_ref.as_str(), root));
                            nodes.extend(staged.nodes);
                        }
                        (None, None) => {
                            let content = BlobRef::for_content(bytes);
                            let envelope = Self::module_envelope(
                                &content,
                                bytes,
                                blob_profile,
                                tx.fleet(),
                                &images,
                            )
                            .map_err(sqlite_conversion_error)?;
                            stored.insert(artifact.artifact_ref.clone(), content.0.clone());
                            pointers.push((artifact.artifact_ref.as_str(), content.0.clone()));
                            blobs.push((content, envelope));
                        }
                    }
                }
                for chunk in blobs.chunks(BLOB_BATCH_ROWS) {
                    let mut insert = tx.prepare_cached(&HELD_SQL.blob_batches[chunk.len() - 1])?;
                    for (index, (blob_ref, envelope)) in chunk.iter().enumerate() {
                        insert.raw_bind_parameter(index * 2 + 1, crate::codec::SqlHash(blob_ref.as_str()))?;
                        insert.raw_bind_parameter(index * 2 + 2, &envelope[..])?;
                    }
                    insert.raw_execute()?;
                }
                if !nodes.is_empty() {
                    crate::durable::install_staged(tx, &nodes)?.map_err(|refusal| {
                        artifact_failure(ArtifactStoreError::StoredDataCorrupt {
                            source: lash_protocol_types::module_artifact_refusal::ModuleArtifactCorruption::Storage {
                                record_kind: "module artifact node".into(),
                                message: refusal.to_string(),
                            },
                        })
                    })?;
                }
                if !pointers.is_empty() {
                    crate::conn::cached_execute(
                        tx,
                        HELD_SQL.held.insert_pointers.sql(),
                        params![MODULE_ARTIFACT_NAMESPACE, json_set(&pointers)?],
                    )?;
                }
                crate::conn::cached_execute(
                    tx,
                    HELD_SQL.held.insert_edges.sql(),
                    params![
                        MODULE_ARTIFACT_NAMESPACE,
                        refs,
                        referrer.kind().as_str(),
                        referrer.canonical_id()
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(artifact_sqlite_error)
    }

    /// Reclaim each of `artifact_refs` in `namespace` that no edge holds any
    /// more: every edge still held is read and decoded, each artifact's
    /// referrers are enumerated complete, and the pointers no referrer holds
    /// are deleted together, their blobs peeled with what only they reached.
    pub(crate) fn reclaim_unreferenced_artifacts_tx(
        tx: &rusqlite::Connection,
        namespace: &str,
        artifact_refs: &[String],
    ) -> rusqlite::Result<()> {
        use lash_core_store::store::EnumerationProgress;
        use lash_core_store::store::ReclamationEnumeration;
        if artifact_refs.is_empty() {
            return Ok(());
        }
        let refs = json_set(&artifact_refs)?;
        let mut held: BTreeMap<String, Vec<(ArtifactReferrerKind, String)>> = artifact_refs
            .iter()
            .map(|artifact_ref| (artifact_ref.clone(), Vec::new()))
            .collect();
        {
            let mut statement = tx.prepare_cached(HELD_SQL.held.select_edges.sql())?;
            let mut rows = statement.query(params![namespace, refs])?;
            while let Some(row) = rows.next()? {
                let artifact_ref: String = row.get(0)?;
                let kind: String = row.get(1)?;
                let id: String = row.get(2)?;
                let referrer = decode_stored_edge(&kind, &id).map_err(sqlite_conversion_error)?;
                held.entry(artifact_ref)
                    .or_default()
                    .push((referrer.kind(), referrer.canonical_id()));
            }
        }
        let mut unreferenced = Vec::new();
        for (artifact_ref, referrers) in held {
            let mut enumeration = ReclamationEnumeration::<
                ArtifactReferrerKind,
                (ArtifactReferrerKind, String),
            >::new();
            for kind in ArtifactReferrerKind::ALL {
                enumeration
                    .page(
                        kind,
                        0,
                        referrers
                            .iter()
                            .filter(|(source, _)| *source == kind)
                            .cloned(),
                        EnumerationProgress::Exhausted,
                    )
                    .map_err(sqlite_conversion_error)?;
            }
            if enumeration
                .finish()
                .map_err(sqlite_conversion_error)?
                .is_empty()
            {
                unreferenced.push(artifact_ref);
            }
        }
        if unreferenced.is_empty() {
            return Ok(());
        }
        // Each pointer was its blob's root: peel the blobs and what only
        // they reached.
        let blobs: BTreeSet<String> = tx
            .prepare_cached(HELD_SQL.held.delete_unreferenced.sql())?
            .query_map(params![namespace, json_set(&unreferenced)?], |row| {
                crate::codec::hash_column(row, 0)
            })?
            .collect::<rusqlite::Result<_>>()?;
        crate::durable::peel(tx, blobs)?;
        Ok(())
    }
}
