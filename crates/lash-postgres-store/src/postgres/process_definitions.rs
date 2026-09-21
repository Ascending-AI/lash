//! PostgreSQL-backed store for the named process-definition registry
//! (FIG-2995).
//!
//! One row per registered definition name, pinned to a durable
//! [`ProcessDefinitionRef`], stored on the deployment-scope durable substrate
//! beside the trigger subscriptions. Consuming records resolve once at intent
//! execution and pin the reference; they never store the name. Session-scoped
//! name slots follow the ADR 0049 deletion frontier; host- and
//! platform-scoped tombstones are permanent (ADR 0067).

use crate::process_sql::process_sql;
use crate::*;
use lash_core::process_registry::{
    ProcessDefinitionExpectation, ProcessDefinitionLifecycle, ProcessDefinitionRecord,
    ProcessDefinitionRegistration, validate_process_definition_name,
};
use lash_core::process_registry::{
    ProcessDefinitionRegistrationRefusal, ProcessDefinitionRegistry,
};
use lash_core::{Clock, ProcessDefinitionRef, TriggerOwnerScope};

fn conflict(
    scope: &str,
    name: &str,
    revision: u64,
    fingerprint: &str,
    message: String,
) -> PluginError {
    PluginError::Session(
        ProcessDefinitionRegistrationRefusal::ConflictingRevision {
            owner_scope: scope.to_string(),
            name: name.to_string(),
            revision,
            fingerprint: fingerprint.to_string(),
            message,
        }
        .to_string(),
    )
}

/// PostgreSQL-backed process-definition registry.
pub struct PostgresProcessDefinitionRegistry {
    pool: PgPool,
    clock: std::sync::Arc<dyn Clock>,
}

impl PostgresProcessDefinitionRegistry {
    pub fn with_pool(pool: PgPool) -> Self {
        Self {
            pool,
            clock: std::sync::Arc::new(lash_core::facade_support::SystemClock),
        }
    }

    pub fn with_clock(mut self, clock: std::sync::Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
}

#[async_trait::async_trait]
impl ProcessDefinitionRegistry for PostgresProcessDefinitionRegistry {
    async fn register_definition(
        &self,
        operation_id: &str,
        owner_scope: TriggerOwnerScope,
        name: &str,
        definition: ProcessDefinitionRef,
        expectation: Option<&ProcessDefinitionExpectation>,
    ) -> Result<ProcessDefinitionRegistration, PluginError> {
        // A name slot fences like the trigger-subscription table: the
        // transaction holds an advisory lock keyed to the definition id, so
        // two concurrent registrations under one name serialize instead of
        // racing the revision CAS.
        validate_process_definition_name(name)?;
        if operation_id.trim().is_empty()
            || !crate::namespace::is_valid_opaque_key(operation_id.trim())
        {
            return Err(PluginError::Session(
                "process definition registration requires a valid operation id".to_string(),
            ));
        }
        let owner_json = serde_json::to_string(&owner_scope)
            .map_err(|err| PluginError::Session(err.to_string()))?;
        let owner_namespace = owner_scope.namespace();
        let definition_id = format!("pd:{}:{}", owner_namespace, name.trim());
        let fingerprint = definition.fingerprint();
        let now_ms = self.clock.timestamp_ms();
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        sqlx::query(
            crate::connection_sql::connection_sql()
                .lock_xact_by_text
                .sql(),
        )
        .bind(&definition_id)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        let existing: Option<(i64, String, String)> =
            sqlx::query_as(process_sql().definition.select_for_cas.sql())
                .bind(&owner_json)
                .bind(name)
                .fetch_optional(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
        type PgExisting = Option<(i64, String, String)>;
        let existing: PgExisting = existing;
        let registration: ProcessDefinitionRegistration = match existing {
            None => {
                if let Some(expectation) = expectation {
                    tx.rollback().await.map_err(plugin_sqlx_error)?;
                    return Err(conflict(
                        &owner_namespace,
                        name,
                        0,
                        "",
                        "expected revision {} with fingerprint {}: no slot exists at this name"
                            .replace(
                                "{}",
                                &format!(
                                    "{}/{}",
                                    expectation.expected_revision, expectation.expected_fingerprint
                                ),
                            ),
                    ));
                }
                let record = ProcessDefinitionRecord {
                    owner_scope,
                    name: name.trim().to_string(),
                    revision: 1,
                    fingerprint: fingerprint.clone(),
                    definition: definition.clone(),
                    lifecycle: ProcessDefinitionLifecycle::Enabled,
                    change_seq: 1,
                    created_at_ms: now_ms,
                    updated_at_ms: now_ms,
                };
                let record_json = serde_json::to_string(&record)
                    .map_err(|err| PluginError::Session(err.to_string()))?;
                sqlx::query(process_sql().definition.insert_first_revision.sql())
                    .bind(&definition_id)
                    .bind(&owner_json)
                    .bind(name)
                    .bind(&fingerprint)
                    .bind(i64::try_from(now_ms).unwrap_or(0))
                    .bind(&record_json)
                    .execute(&mut *tx)
                    .await
                    .map_err(plugin_sqlx_error)?;
                ProcessDefinitionRegistration::Admitted(Box::new(record))
            }
            Some((revision, existing_fingerprint, existing_json)) => {
                let existing_record: ProcessDefinitionRecord = serde_json::from_str(&existing_json)
                    .map_err(|err| PluginError::Session(err.to_string()))?;
                if expectation.is_none()
                    && !existing_record
                        .definition
                        .names_same_definition(&definition)
                {
                    tx.rollback().await.map_err(plugin_sqlx_error)?;
                    return Err(conflict(
                        &owner_namespace,
                        name,
                        revision.max(0) as u64,
                        &existing_fingerprint,
                        format!(
                            "name `{}` is registered at revision {}; \
                             re-registration requires the caller's revision compare-and-swap",
                            name, revision
                        ),
                    ));
                }
                if existing_record
                    .definition
                    .names_same_definition(&definition)
                {
                    tx.commit().await.map_err(plugin_sqlx_error)?;
                    return Ok(ProcessDefinitionRegistration::Existing(Box::new(
                        existing_record,
                    )));
                }
                if let Some(expectation) = expectation
                    && (expectation.expected_revision != revision.max(0) as u64
                        || expectation.expected_fingerprint != existing_fingerprint)
                {
                    tx.rollback().await.map_err(plugin_sqlx_error)?;
                    return Err(conflict(
                        &owner_namespace,
                        name,
                        revision.max(0) as u64,
                        &existing_fingerprint,
                        format!(
                            "expected revision {} with fingerprint {}",
                            expectation.expected_revision, expectation.expected_fingerprint
                        ),
                    ));
                }
                let new_revision: u64 = supply_revision(revision).max(0) as u64;
                let new_change_seq = existing_record.change_seq + 1;
                let mut new_record = existing_record.clone();
                new_record.revision = new_revision;
                new_record.fingerprint = fingerprint.clone();
                new_record.definition = definition.clone();
                new_record.lifecycle = ProcessDefinitionLifecycle::Enabled;
                new_record.updated_at_ms = now_ms;
                new_record.change_seq = new_change_seq;
                let record_json = serde_json::to_string(&new_record)
                    .map_err(|err| PluginError::Session(err.to_string()))?;
                let changed = sqlx::query(process_sql().definition.update_revision.sql())
                    .bind(&owner_json)
                    .bind(name)
                    .bind(new_revision.cast_signed())
                    .bind(&fingerprint)
                    .bind(i64::try_from(now_ms).unwrap_or(0))
                    .bind(&record_json)
                    .execute(&mut *tx)
                    .await
                    .map_err(plugin_sqlx_error)?
                    .rows_affected();
                if changed != 1 {
                    tx.rollback().await.map_err(plugin_sqlx_error)?;
                    return Err(conflict(
                        &owner_namespace,
                        name,
                        revision.max(0) as u64,
                        &existing_fingerprint,
                        "the definition slot changed concurrently; retry from the fresh row"
                            .to_string(),
                    ));
                }
                ProcessDefinitionRegistration::Admitted(Box::new(new_record))
            }
        };
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(registration)
    }

    async fn list_definitions(
        &self,
        owner_scope: &TriggerOwnerScope,
    ) -> Result<Vec<ProcessDefinitionRecord>, PluginError> {
        let owner_json = serde_json::to_string(owner_scope)
            .map_err(|err| PluginError::Session(err.to_string()))?;
        let rows: Vec<(String,)> =
            sqlx::query_as(process_sql().definition.list_by_owner_scope.sql())
                .bind(&owner_json)
                .fetch_all(&self.pool)
                .await
                .map_err(plugin_sqlx_error)?;
        let mut records = Vec::with_capacity(rows.len());
        for (record_json,) in rows {
            let record: ProcessDefinitionRecord = serde_json::from_str(&record_json)
                .map_err(|err| PluginError::Session(err.to_string()))?;
            records.push(record);
        }
        Ok(records)
    }
}

fn supply_revision(revision: i64) -> i64 {
    revision.max(0) + 1
}
