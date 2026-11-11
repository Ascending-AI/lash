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
            ModuleArtifactStore, package_artifact_holds,
        };
        let holds = package_artifact_holds(records);
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
            // Held means reached: through the referrer's own edges, or what
            // those artifacts hold (FIG-7700).
            crate::observed_sql::query_scalar(artifact_sql().postgres.select_unreached.sql())
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
        let held: bool = crate::observed_sql::query_scalar(
            artifact_sql().dependencies.select_reaches_namespace.sql(),
        )
        .bind(MODULE_ARTIFACT_NAMESPACE)
        .bind(referrer.kind().as_str())
        .bind(referrer.canonical_id())
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(backend)?;
        Ok(!held && is_fenced_tx(&mut tx, referrer).await?)
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
        let held: bool =
            crate::observed_sql::query_scalar(artifact_sql().dependencies.select_reaches.sql())
                .bind(MODULE_ARTIFACT_NAMESPACE)
                .bind(referrer.kind().as_str())
                .bind(referrer.canonical_id())
                .bind(reference)
                .fetch_one(crate::observed_sql::executor(&mut **tx))
                .await
                .map_err(backend)?;
        if !held {
            return Err(ArtifactStoreError::ArtifactMissing {
                artifact_ref: reference.to_owned(),
            }
            .into());
        }
        let sql = lash_store_sql::durable::packages::PackageStatements::render(Dialect::postgres());
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
    async fn load_package_records(
        &self,
        references: &[String],
    ) -> Result<std::collections::BTreeMap<String, Vec<u8>>, lash_durable::DurableError> {
        let wanted: Vec<&str> = references
            .iter()
            .map(String::as_str)
            .filter(|reference| crate::namespace::is_valid_opaque_key(reference))
            .collect();
        if wanted.is_empty() {
            return Ok(std::collections::BTreeMap::new());
        }
        let rows: Vec<(String, Vec<u8>, Vec<String>, Vec<String>)> = crate::observed_sql::query_as(
            artifact_sql()
                .lash_vm_artifacts
                .select_records_with_edges
                .sql(),
        )
        .bind(MODULE_ARTIFACT_NAMESPACE)
        .bind(&wanted)
        .fetch_all(crate::observed_sql::executor(&self.pool))
        .await
        .map_err(backend)
        .map_err(lash_durable::DurableError::from)?;
        // A record with an edge the single read would refuse is left to it.
        Ok(rows
            .into_iter()
            .filter(|(_, _, kinds, ids)| {
                kinds.len() == ids.len()
                    && kinds
                        .iter()
                        .zip(ids)
                        .all(|(kind, id)| ArtifactReferrer::decode(kind, id).is_ok())
            })
            .map(|(reference, bytes, _, _)| (reference, bytes))
            .collect())
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
