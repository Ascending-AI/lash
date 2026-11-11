//! How the SQLite store writes and reads a process environment: its
//! stored split, its module closure and the holder edges that keep them
//! (FIG-7700).

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
/// body, compare what is stored with `bytes`, then write its dependency
/// edges: the body (or a head stored whole) holds the module closure, the
/// head holds its body (FIG-7700).
pub(super) fn store_process_env_tx(
    tx: &rusqlite::Connection,
    env_ref: &str,
    bytes: &[u8],
    profile: BuiltinBlobProfile,
    fleet: lash_core_store::store::FleetFormat,
) -> rusqlite::Result<()> {
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
    match &split {
        Some(split) => {
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
        }
        None => {
            let whole = blob(bytes)?;
            crate::conn::cached_execute(
                tx,
                artifact_sql().refs.insert_pointer.sql(),
                params![
                    PROCESS_ENV_NAMESPACE,
                    env_ref,
                    crate::codec::SqlHash(whole.as_str())
                ],
            )?;
        }
    }
    let stored = stored_process_env_conn(tx, env_ref).map_err(sqlite_conversion_error)?;
    if stored.as_deref() != Some(bytes) {
        return Err(artifact_failure(ArtifactStoreError::Immutable {
            artifact_ref: env_ref.to_owned(),
        }));
    }
    let closure = process_env_closure_tx(tx, bytes)?;
    let json = |references: &std::collections::BTreeSet<String>| {
        serde_json::to_string(references)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
    };
    let holder = match &split {
        Some(split) => (PROCESS_ENV_BODY_NAMESPACE, split.body_ref.as_str()),
        None => (PROCESS_ENV_NAMESPACE, env_ref),
    };
    if !closure.is_empty() {
        crate::conn::cached_execute(
            tx,
            held::HELD_SQL.held.insert_dependency_edges.sql(),
            params![
                MODULE_ARTIFACT_NAMESPACE,
                json(&closure)?,
                holder.0,
                holder.1
            ],
        )?;
    }
    if let Some(split) = &split {
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
