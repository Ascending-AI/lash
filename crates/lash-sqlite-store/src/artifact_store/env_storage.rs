//! How the SQLite store writes and reads a process environment: its
//! stored split, its module closure and the holder edges that keep them
//! (FIG-7700).
//!
//! A referrer's environment share is its edge to the head, which holds the
//! body; its module share includes its edge to the body's closure, which
//! holds the modules. Each keeps what it holds until that share ends.

use super::*;

/// An environment's module closure, read from the bytes in hand in the
/// writer transaction, its first missing module in depth-first order.
pub(super) fn process_env_closure_tx(
    tx: &rusqlite::Connection,
    bytes: &[u8],
) -> rusqlite::Result<std::collections::BTreeSet<String>> {
    fn read(tx: &rusqlite::Connection, reference: &str) -> rusqlite::Result<Vec<u8>> {
        let stored: Option<(Option<String>, Option<i64>)> = tx
            .statement(artifact_sql().refs.select_body_ref.sql())?
            .query_row(params![MODULE_ARTIFACT_NAMESPACE, reference], |row| {
                Ok((crate::codec::optional_hash_column(row, 0)?, row.get(1)?))
            })
            .optional()?;
        let Some((Some(blob), encoding)) = stored else {
            return Err(artifact_failure(ArtifactStoreError::ArtifactMissing {
                artifact_ref: reference.into(),
            }));
        };
        if encoding == Some(1) {
            SqliteStore::module_node_conn(tx, &blob).map_err(sqlite_conversion_error)
        } else {
            SqliteStore::get_blob_conn(tx, &BlobRef(blob))
                .map_err(sqlite_conversion_error)?
                .ok_or_else(|| {
                    artifact_failure(ArtifactStoreError::ArtifactMissing {
                        artifact_ref: reference.into(),
                    })
                })
        }
    }
    let mut pending = lash_durable::packages::process_env_roots(bytes)
        .map_err(|error| artifact_failure(error.into()))?;
    let mut found = std::collections::BTreeSet::new();
    while let Some(reference) = pending.pop() {
        if !found.insert(reference.clone()) {
            continue;
        }
        let bytes = read(tx, &reference)?;
        pending.extend(
            lash_durable::packages::content_dependencies(&reference, &bytes)
                .map_err(|error| artifact_failure(error.into()))?,
        );
    }
    Ok(found)
}

/// Store a published environment as a head at its reference and a shared
/// body, compare what is stored with `bytes`, then write the head's edge
/// to its body and hold the body's module closure. Answers the closure's
/// reference (FIG-7700).
pub(super) fn store_process_env_tx(
    tx: &rusqlite::Connection,
    env_ref: &str,
    bytes: &[u8],
    profile: BuiltinBlobProfile,
    fleet: lash_core_store::store::FleetFormat,
) -> rusqlite::Result<String> {
    let split = lash_core_store::process_env::stored::split(bytes);
    let blob = |content: &[u8]| {
        SqliteStore::insert_artifact_blob_conn(
            tx,
            BlobArtifactDescriptor::process_execution_env(),
            content,
            profile,
            fleet,
        )
    };
    let body = blob(&split.body)?;
    crate::conn::cached_execute(
        tx,
        artifact_sql().refs.insert_pointer.sql(),
        params![
            PROCESS_ENV_BODY_NAMESPACE,
            split.body_ref,
            crate::codec::SqlHash(body.as_str())
        ],
    )?;
    let head = blob(&split.head)?;
    let at = i64::try_from(split.at)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    crate::conn::cached_execute(
        tx,
        artifact_sql().refs.insert_head_pointer.sql(),
        params![
            PROCESS_ENV_NAMESPACE,
            env_ref,
            crate::codec::SqlHash(head.as_str()),
            split.body_ref,
            at
        ],
    )?;
    let stored = stored_process_env_conn(tx, env_ref).map_err(sqlite_conversion_error)?;
    if stored.as_deref() != Some(bytes) {
        return Err(artifact_failure(ArtifactStoreError::Immutable {
            artifact_ref: env_ref.to_owned(),
        }));
    }
    crate::conn::cached_execute(
        tx,
        artifact_sql().dependencies.insert_edge.sql(),
        params![
            PROCESS_ENV_BODY_NAMESPACE,
            split.body_ref,
            PROCESS_ENV_NAMESPACE,
            env_ref
        ],
    )?;
    hold_closure_tx(tx, &split.body_ref, || Ok(bytes.to_vec()))?;
    Ok(split.body_ref)
}

/// Environment `env_ref`'s module closure reference, the closure stored
/// first if it is not: a pointer at the body's reference in
/// [`PROCESS_ENV_CLOSURE_NAMESPACE`] holding every module the body names.
/// A module the closure needs that is not stored refuses as missing. None
/// when the environment is not stored.
pub(super) fn env_closure_tx(
    tx: &rusqlite::Connection,
    env_ref: &str,
) -> rusqlite::Result<Option<String>> {
    let body_ref: Option<String> = tx
        .statement(artifact_sql().refs.select_head_body_ref.sql())?
        .query_row(params![PROCESS_ENV_NAMESPACE, env_ref], |row| row.get(0))
        .optional()?;
    let Some(body_ref) = body_ref else {
        return Ok(None);
    };
    hold_closure_tx(tx, &body_ref, || {
        stored_process_env_conn(tx, env_ref)
            .map_err(sqlite_conversion_error)?
            .ok_or_else(|| {
                artifact_failure(ArtifactStoreError::ArtifactMissing {
                    artifact_ref: env_ref.to_owned(),
                })
            })
    })?;
    Ok(Some(body_ref))
}

/// Store body `body_ref`'s closure unless it is stored: a pointer at the
/// body's blob and an edge to every module the environment's bytes name.
fn hold_closure_tx(
    tx: &rusqlite::Connection,
    body_ref: &str,
    bytes: impl FnOnce() -> rusqlite::Result<Vec<u8>>,
) -> rusqlite::Result<()> {
    let stored: bool = tx
        .statement(artifact_sql().refs.select_exists.sql())?
        .query_row(params![PROCESS_ENV_CLOSURE_NAMESPACE, body_ref], |row| {
            row.get(0)
        })?;
    if stored {
        return Ok(());
    }
    let closure = process_env_closure_tx(tx, &bytes()?)?;
    crate::conn::cached_execute(
        tx,
        artifact_sql().refs.insert_closure_pointer.sql(),
        params![
            PROCESS_ENV_CLOSURE_NAMESPACE,
            body_ref,
            PROCESS_ENV_BODY_NAMESPACE
        ],
    )?;
    if !closure.is_empty() {
        crate::conn::cached_execute(
            tx,
            held::HELD_SQL.held.insert_dependency_edges.sql(),
            params![
                MODULE_ARTIFACT_NAMESPACE,
                serde_json::to_string(&closure)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?,
                PROCESS_ENV_CLOSURE_NAMESPACE,
                body_ref
            ],
        )?;
    }
    Ok(())
}

/// A head's bytes with its body spliced back; `head` itself for an
/// environment stored whole, none when its body is missing.
pub(super) fn splice_env_body_conn(
    conn: &rusqlite::Connection,
    env_ref: &str,
    head: Vec<u8>,
) -> Result<Option<Vec<u8>>, StoreError> {
    let body: Option<(i64, Option<String>)> = conn
        .statement(artifact_sql().refs.select_env_body.sql())
        .and_then(|mut statement| {
            statement.query_row(params![PROCESS_ENV_NAMESPACE, env_ref], |row| {
                Ok((row.get(0)?, crate::codec::optional_hash_column(row, 1)?))
            })
        })
        .optional()
        .map_err(sqlite_error)?;
    let Some((at, body)) = body else {
        return Ok(Some(head));
    };
    let Some(body) = body else {
        return Ok(None);
    };
    let Some(body) = SqliteStore::get_blob_conn(conn, &BlobRef(body))? else {
        return Ok(None);
    };
    splice_env_parts(&head, at, &body).map(Some)
}

/// An environment's bytes from its decoded head, the body's offset and its
/// decoded body.
pub(crate) fn splice_env_parts(head: &[u8], at: i64, body: &[u8]) -> Result<Vec<u8>, StoreError> {
    usize::try_from(at)
        .ok()
        .and_then(|at| lash_core_store::process_env::stored::splice(head, at, body))
        .ok_or_else(|| {
            stored_data_corrupt(
                "process execution environment",
                "body offset lies outside its head",
            )
        })
}

/// Environment `env_ref`'s stored bytes, its body spliced back.
pub(super) fn stored_process_env_conn(
    conn: &rusqlite::Connection,
    env_ref: &str,
) -> Result<Option<Vec<u8>>, StoreError> {
    let head: Option<Option<String>> = conn
        .statement(artifact_sql().refs.select_blob_ref.sql())
        .and_then(|mut statement| {
            statement.query_row(params![PROCESS_ENV_NAMESPACE, env_ref], |row| {
                crate::codec::optional_hash_column(row, 0)
            })
        })
        .optional()
        .map_err(sqlite_error)?;
    let Some(head) = head else {
        return Ok(None);
    };
    let missing = || {
        stored_data_corrupt(
            "artifact reference",
            format_args!("process execution env `{env_ref}` points at a missing blob"),
        )
    };
    let head = head
        .map(|head| SqliteStore::get_blob_conn(conn, &BlobRef(head)))
        .transpose()?
        .flatten()
        .ok_or_else(missing)?;
    splice_env_body_conn(conn, env_ref, head)?
        .ok_or_else(missing)
        .map(Some)
}
