//! SQLite-backed store for the named process-definition registry (FIG-2995).
//!
//! The registry shares the deployment-scope durable-core database: it stores
//! one row per registered definition name, pinned to a durable
//! [`ProcessDefinitionRef`], outside any session's own lifecycle. A consumer
//! record never holds the name; the name column is the fence that makes an
//! owner-scoped registration addressable. Session-scoped name slots follow
//! the ADR 0049 deletion frontier; host- and platform-scoped tombstones are
//! permanent (ADR 0067).

use std::path::Path;
use std::sync::Arc;

use lash_core::process_registry::ProcessDefinitionRegistrationRefusal;
use lash_core::process_registry::{
    ProcessDefinitionExpectation, ProcessDefinitionLifecycle, ProcessDefinitionRecord,
    ProcessDefinitionRegistration, validate_process_definition_name,
};
use lash_core::{Clock, PluginError, ProcessDefinitionRef, TriggerOwnerScope};
use rusqlite::OptionalExtension;

use super::apply_pragmas;
use super::conn::TxOutcome;
use super::{SqliteConnection, SqliteDatabase, StoreBacking, ensure_versioned_schema};

fn sqlite_plugin_error(err: rusqlite::Error) -> PluginError {
    PluginError::Session(format!(
        "process-definition store: sqlite operation failed: {err}"
    ))
}

fn record_error(failure: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(failure.to_string())))
}

fn decode_record(json: &str) -> Result<ProcessDefinitionRecord, rusqlite::Error> {
    serde_json::from_str(json)
        .map_err(|err| record_error(format!("failed to decode process-definition record: {err}")))
}

fn encode_record(record: &ProcessDefinitionRecord) -> Result<String, rusqlite::Error> {
    serde_json::to_string(record)
        .map_err(|err| record_error(format!("failed to encode process-definition record: {err}")))
}

fn encode_owner(owner_scope: &TriggerOwnerScope) -> Result<String, rusqlite::Error> {
    serde_json::to_string(owner_scope).map_err(|err| {
        let failure = format!("failed to encode process-definition owner scope: {err}");
        rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(failure)))
    })
}

/// SQLite-backed process-definition registry.
pub struct SqliteProcessDefinitionRegistry {
    conn: SqliteConnection,
    clock: Arc<dyn Clock>,
}

impl SqliteProcessDefinitionRegistry {
    pub async fn open(path: &Path) -> tokio_rusqlite::Result<Self> {
        Self::open_with_clock(path, Arc::new(lash_core::facade_support::SystemClock)).await
    }

    pub async fn open_with_clock(
        path: &Path,
        clock: Arc<dyn Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        let conn = SqliteConnection::open(path).await?;
        ensure_versioned_schema(&conn, SqliteDatabase::DurableCore).await?;
        apply_pragmas(&conn, StoreBacking::File).await?;
        Ok(Self { conn, clock })
    }

    pub async fn memory() -> tokio_rusqlite::Result<Self> {
        Self::memory_with_clock(Arc::new(lash_core::facade_support::SystemClock)).await
    }

    pub async fn memory_with_clock(clock: Arc<dyn Clock>) -> tokio_rusqlite::Result<Self> {
        let conn = SqliteConnection::open_in_memory().await?;
        ensure_versioned_schema(&conn, SqliteDatabase::DurableCore).await?;
        apply_pragmas(&conn, StoreBacking::Memory).await?;
        Ok(Self { conn, clock })
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessDefinitionRegistry for SqliteProcessDefinitionRegistry {
    async fn register_definition(
        &self,
        operation_id: &str,
        owner_scope: TriggerOwnerScope,
        name: &str,
        definition: ProcessDefinitionRef,
        expectation: Option<&ProcessDefinitionExpectation>,
    ) -> Result<ProcessDefinitionRegistration, PluginError> {
        validate_process_definition_name(name)?;
        if operation_id.trim().is_empty()
            || !crate::namespace::is_valid_opaque_key(operation_id.trim())
        {
            return Err(PluginError::Session(
                "process definition registration requires a valid operation id".to_string(),
            ));
        }
        let owner_json = encode_owner(&owner_scope).map_err(sqlite_plugin_error)?;
        let owner_namespace = owner_scope.namespace();
        let definition_id = format!("lash.process-definition:{}:{}", owner_namespace, name);
        let fingerprint = definition.fingerprint();
        let name = name.trim().to_string();
        let definition = definition.clone();
        let expectation = expectation.cloned();
        let clock = self.clock.clone();

        self.conn
            .write_flow(move |tx| {
                let now_ms = clock.timestamp_ms();
                let existing: Option<(i64, String, String)> = tx
                    .query_row(
                        "SELECT revision, fingerprint, record_json FROM process_definitions \
                         WHERE owner_scope = ?1 AND name = ?2",
                        rusqlite::params![owner_json, name],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        },
                    )
                    .optional()?;
                let attempted: Result<ProcessDefinitionRegistration, PluginError> = match existing
                {
                    None => match expectation {
                        Some(expectation) => Err(PluginError::Session(
                            ProcessDefinitionRegistrationRefusal::ConflictingRevision {
                                owner_scope: owner_namespace.clone(),
                                name: name.clone(),
                                revision: 0,
                                fingerprint: String::new(),
                                message: format!(
                                    "expected revision {} with fingerprint {}: \
                                     no slot exists at this name",
                                    expectation.expected_revision,
                                    expectation.expected_fingerprint
                                ),
                            }
                            .to_string(),
                        )),
                        None => {
                            let record = ProcessDefinitionRecord {
                                owner_scope,
                                name: name.clone(),
                                revision: 1,
                                fingerprint: fingerprint.clone(),
                                definition: definition.clone(),
                                lifecycle: ProcessDefinitionLifecycle::Enabled,
                                change_seq: 1,
                                created_at_ms: now_ms,
                                updated_at_ms: now_ms,
                            };
                            let record_json = encode_record(&record)?;
                            tx.execute(
                                "INSERT INTO process_definitions \
                                 (definition_id, owner_scope, name, revision, fingerprint, \
                                  lifecycle, deleted_at_ms, change_seq, created_at_ms, \
                                  updated_at_ms, record_json) \
                                 VALUES (?1, ?2, ?3, 1, ?4, 'enabled', NULL, 1, ?5, ?5, ?6)",
                                rusqlite::params![
                                    definition_id,
                                    owner_json,
                                    name,
                                    fingerprint,
                                    i64::try_from(now_ms).unwrap_or(0),
                                    record_json,
                                ],
                            )?;
                            Ok(ProcessDefinitionRegistration::Admitted(Box::new(record)))
                        }
                    },
                    Some((revision, existing_fingerprint, existing_json)) => {
                        let existing_record = decode_record(&existing_json)?;
                        if expectation.is_none() && !existing_record.definition.names_same_definition(&definition) {
                            return Ok(TxOutcome::Rollback(Err(PluginError::Session(
                                ProcessDefinitionRegistrationRefusal::ConflictingRevision {
                                    owner_scope: owner_namespace.clone(),
                                    name: name.clone(),
                                    revision: revision.unsigned_abs(),
                                    fingerprint: existing_fingerprint.clone(),
                                    message: format!(
                                        "name `{name}` is registered at revision {}; \
                                         re-registration requires the caller's revision compare-and-swap",
                                        revision.unsigned_abs()
                                    ),
                                }
                                .to_string(),
                            ))));
                        }
                        if existing_record.definition.names_same_definition(&definition) {
                            return Ok(TxOutcome::Commit(Ok(
                                ProcessDefinitionRegistration::Existing(Box::new(existing_record)),
                            )));
                        }
                        if let Some(expectation) = &expectation
                            && (expectation.expected_revision != revision.unsigned_abs()
                                || expectation.expected_fingerprint != existing_fingerprint)
                        {
                            return Ok(TxOutcome::Rollback(Err(PluginError::Session(
                                ProcessDefinitionRegistrationRefusal::ConflictingRevision {
                                    owner_scope: owner_namespace.clone(),
                                    name: name.clone(),
                                    revision: revision.unsigned_abs(),
                                    fingerprint: existing_fingerprint,
                                    message: format!(
                                        "expected revision {} with fingerprint {}",
                                        expectation.expected_revision,
                                        expectation.expected_fingerprint
                                    ),
                                }
                                .to_string(),
                            ))));
                        }
                        let new_revision = revision.unsigned_abs() + 1;
                        let mut new_record = existing_record;
                        new_record.revision = new_revision;
                        new_record.fingerprint = fingerprint.clone();
                        new_record.definition = definition.clone();
                        new_record.lifecycle = ProcessDefinitionLifecycle::Enabled;
                        new_record.updated_at_ms = now_ms;
                        new_record.change_seq += 1;
                        let record_json = encode_record(&new_record)?;
                        let updated = tx.execute(
                            "UPDATE process_definitions SET revision = ?3, fingerprint = ?4, \
                             lifecycle = 'enabled', deleted_at_ms = NULL, \
                             change_seq = change_seq + 1, updated_at_ms = ?5, record_json = ?6 \
                             WHERE owner_scope = ?1 AND name = ?2",
                            rusqlite::params![
                                owner_json,
                                name,
                                i64::try_from(new_revision).unwrap_or(0),
                                fingerprint,
                                i64::try_from(now_ms).unwrap_or(0),
                                record_json,
                            ],
                        )?;
                        if updated != 1 {
                            return Ok(TxOutcome::Rollback(Err(PluginError::Session(
                                ProcessDefinitionRegistrationRefusal::ConflictingRevision {
                                    owner_scope: owner_namespace,
                                    name: name.clone(),
                                    revision: new_revision.saturating_sub(1),
                                    fingerprint: existing_fingerprint,
                                    message: "the definition slot changed concurrently; \
                                              retry from the fresh row"
                                        .to_string(),
                                }
                                .to_string(),
                            ))));
                        }
                        Ok(ProcessDefinitionRegistration::Admitted(Box::new(new_record)))
                    }
                };
                match attempted {
                    Ok(record) => Ok(TxOutcome::Commit(Ok(record))),
                    Err(error) => Ok(TxOutcome::Rollback(Err(error))),
                }
            })
            .await
            .map_err(sqlite_plugin_error)?
    }

    async fn list_definitions(
        &self,
        owner_scope: &TriggerOwnerScope,
    ) -> Result<Vec<ProcessDefinitionRecord>, PluginError> {
        let owner_json = encode_owner(owner_scope).map_err(sqlite_plugin_error)?;
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT record_json FROM process_definitions \
                     WHERE owner_scope = ?1 ORDER BY name ASC",
                )?;
                let rows =
                    stmt.query_map(rusqlite::params![owner_json], |row| row.get::<_, String>(0))?;
                let mut records = Vec::new();
                for row in rows {
                    let record = decode_record(&row?)?;
                    records.push(record);
                }
                Ok(records)
            })
            .await
            .map_err(sqlite_plugin_error)
    }
}
