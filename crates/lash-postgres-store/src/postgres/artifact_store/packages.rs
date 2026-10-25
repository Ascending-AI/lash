//! Immutable package retention through the artifact owner.
use super::*;
use std::collections::BTreeMap;

#[async_trait::async_trait]
impl lash_durable::packages::PackageStorage for PostgresLashVmArtifactStore {
    async fn hold_package_batch(
        &self,
        claim: &ReferrerClaim,
        records: &[lash_durable::packages::PackageRecordHold],
    ) -> Result<(), lash_durable::DurableError> {
        use lash_execution_contracts::module_artifacts::{
            ModuleArtifactBody, ModuleArtifactHold, ModuleArtifactStore,
        };
        let mut holds = Vec::new();
        for record in records {
            holds.extend(
                record
                    .dependencies
                    .iter()
                    .map(|reference| ModuleArtifactHold::Retain {
                        module_ref: reference,
                    }),
            );
            holds.push(match record.bytes.as_deref() {
                Some(bytes) => ModuleArtifactHold::Publish {
                    module_ref: &record.reference,
                    body: if record.document {
                        ModuleArtifactBody::Document(bytes)
                    } else {
                        ModuleArtifactBody::Bytes(bytes)
                    },
                },
                None => ModuleArtifactHold::Retain {
                    module_ref: &record.reference,
                },
            });
        }
        self.hold_module_artifacts(claim, &holds)
            .await
            .map_err(Into::into)
    }

    async fn unheld_publication_records(
        &self,
        claim: &ReferrerClaim,
        keys: &std::collections::BTreeSet<lash_vocabulary::package::PublicationKey>,
    ) -> Result<
        std::collections::BTreeSet<lash_vocabulary::package::PublicationKey>,
        lash_durable::DurableError,
    > {
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(ArtifactStoreError::from)?;
        let referrer = claim.referrer();
        lock_session_artifact_owner_tx(&mut tx, &referrer).await?;
        lock_referrer_tx(&mut tx, &referrer)
            .await
            .map_err(backend)?;
        admit_artifact_owner_tx(&mut tx, &referrer).await?;
        if is_fenced_tx(&mut tx, &referrer).await? {
            return Err(ArtifactStoreError::ReferrerEnded { referrer }.into());
        }
        let references: BTreeMap<_, _> = keys
            .iter()
            .map(|key| {
                (
                    lash_durable::packages::publication_reference(key),
                    key.clone(),
                )
            })
            .collect();
        let wanted: Vec<_> = references.keys().cloned().collect();
        let missing: BTreeSet<String> =
            crate::observed_sql::query_scalar(artifact_sql().lash_vm_artifacts.select_unheld.sql())
                .bind(MODULE_ARTIFACT_NAMESPACE)
                .bind(&wanted)
                .bind(referrer.kind().as_str())
                .bind(referrer.canonical_id())
                .fetch_all(crate::observed_sql::executor(&mut **tx))
                .await
                .map_err(backend)?
                .into_iter()
                .collect();
        tx.commit().await.map_err(backend)?;
        Ok(references
            .into_iter()
            .filter_map(|(reference, key)| missing.contains(&reference).then_some(key))
            .collect())
    }
    async fn package_referrer_ended(
        &self,
        referrer: &ArtifactReferrer,
    ) -> Result<bool, lash_durable::DurableError> {
        let mut tx = self.pool.acquire().await.map_err(backend)?;
        let held = crate::observed_sql::query(
            artifact_sql()
                .edges
                .select_referrer_edges_in_namespace
                .sql(),
        )
        .bind(MODULE_ARTIFACT_NAMESPACE)
        .bind(referrer.kind().as_str())
        .bind(referrer.canonical_id())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(backend)?
        .is_some();
        Ok(!held && is_fenced_tx(&mut tx, referrer).await?)
    }
    async fn hold_package_record(
        &self,
        claim: &ReferrerClaim,
        reference: &str,
        bytes: Option<&[u8]>,
        dependencies: &[String],
    ) -> Result<(), lash_durable::DurableError> {
        self.hold_package_batch(
            claim,
            &[lash_durable::packages::PackageRecordHold {
                reference: reference.to_owned(),
                bytes: bytes.map(<[u8]>::to_vec),
                document: false,
                dependencies: dependencies.to_vec(),
            }],
        )
        .await
    }
    async fn put_package_labels(
        &self,
        claim: &ReferrerClaim,
        reference: &str,
        digest: &[u8; 32],
        json: &str,
    ) -> Result<(), lash_durable::DurableError> {
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(ArtifactStoreError::from)?;
        let referrer = claim.referrer();
        lock_session_artifact_owner_tx(&mut tx, &referrer).await?;
        lock_referrer_tx(&mut tx, &referrer)
            .await
            .map_err(backend)?;
        admit_artifact_owner_tx(&mut tx, &referrer).await?;
        if is_fenced_tx(&mut tx, &referrer).await? {
            return Err(ArtifactStoreError::ReferrerEnded { referrer }.into());
        }
        lock_artifact_tx(&mut tx, MODULE_ARTIFACT_NAMESPACE, reference).await?;
        let sql = lash_store_sql::durable::packages::PackageStatements::render(Dialect::postgres());
        let held = crate::observed_sql::query(sql.labels_held.sql())
            .bind(MODULE_ARTIFACT_NAMESPACE)
            .bind(reference)
            .bind(referrer.kind().as_str())
            .bind(referrer.canonical_id())
            .fetch_optional(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(backend)?
            .is_some();
        if !held {
            return Err(ArtifactStoreError::ArtifactMissing {
                artifact_ref: reference.to_owned(),
            }
            .into());
        }
        crate::observed_sql::query(sql.insert_labels.sql())
            .bind(MODULE_ARTIFACT_NAMESPACE)
            .bind(reference)
            .bind(digest.as_slice())
            .bind(json)
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend).map_err(Into::into)
    }
    async fn package_labels(
        &self,
        reference: &str,
    ) -> Result<Vec<String>, lash_durable::DurableError> {
        let sql = lash_store_sql::durable::packages::PackageStatements::render(Dialect::postgres());
        let mut connection = self.pool.acquire().await.map_err(backend)?;
        crate::observed_sql::query_scalar::<_, String>(sql.labels.sql())
            .bind(MODULE_ARTIFACT_NAMESPACE)
            .bind(reference)
            .fetch_all(crate::observed_sql::executor(&mut *connection))
            .await
            .map_err(backend)
            .map_err(Into::into)
    }
    async fn load_package_record(
        &self,
        reference: &str,
    ) -> Result<Option<Vec<u8>>, lash_durable::DurableError> {
        use lash_execution_contracts::module_artifacts::ModuleArtifactStore;
        self.get_module_artifact(reference)
            .await
            .map_err(Into::into)
    }
    async fn end_package_records(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), lash_durable::DurableError> {
        self.end_namespaced(MODULE_ARTIFACT_NAMESPACE, cleanup)
            .await
            .map_err(Into::into)
    }
}
