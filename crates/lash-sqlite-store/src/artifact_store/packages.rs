//! Immutable package retention through the artifact owner.
use super::*;

#[async_trait::async_trait]
impl lash_durable::packages::PackageStorage for SqliteStore {
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
        let claim = claim.clone();
        let references: std::collections::BTreeMap<_, _> = keys
            .iter()
            .map(|key| {
                (
                    lash_durable::packages::publication_reference(key),
                    key.clone(),
                )
            })
            .collect();
        self.conn
            .write(move |tx| {
                let referrer = claim.referrer();
                admit_artifact_owner_tx(tx, &referrer)?;
                if artifact_fenced_tx(tx, &referrer)? {
                    return Err(artifact_failure(ArtifactStoreError::ReferrerEnded {
                        referrer,
                    }));
                }
                let wanted = serde_json::to_string(&references.keys().collect::<Vec<_>>())
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                let missing: std::collections::BTreeSet<String> = tx
                    .prepare_cached(held::HELD_SQL.held.select_unheld.sql())?
                    .query_map(
                        params![
                            MODULE_ARTIFACT_NAMESPACE,
                            wanted,
                            referrer.kind().as_str(),
                            referrer.canonical_id()
                        ],
                        |row| row.get(0),
                    )?
                    .collect::<rusqlite::Result<_>>()?;
                Ok(references
                    .into_iter()
                    .filter_map(|(reference, key)| missing.contains(&reference).then_some(key))
                    .collect())
            })
            .await
            .map_err(artifact_sqlite_error)
            .map_err(Into::into)
    }
    async fn package_referrer_ended(
        &self,
        referrer: &ArtifactReferrer,
    ) -> Result<bool, lash_durable::DurableError> {
        let referrer = referrer.clone();
        self.conn
            .call(move |tx| {
                let held = tx
                    .prepare_cached(
                        artifact_sql()
                            .edges
                            .select_referrer_edges_in_namespace
                            .sql(),
                    )?
                    .query_row(
                        params![
                            MODULE_ARTIFACT_NAMESPACE,
                            referrer.kind().as_str(),
                            referrer.canonical_id()
                        ],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                Ok(!held && artifact_fenced_tx(tx, &referrer)?)
            })
            .await
            .map_err(artifact_sqlite_error)
            .map_err(Into::into)
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
        let claim = claim.clone();
        let reference = reference.to_owned();
        let digest = *digest;
        let json = json.to_owned();
        self.conn
            .write(move |tx| {
                let referrer = claim.referrer();
                admit_artifact_owner_tx(tx, &referrer)?;
                if artifact_fenced_tx(tx, &referrer)? {
                    return Err(artifact_failure(ArtifactStoreError::ReferrerEnded {
                        referrer,
                    }));
                }
                let sql = lash_store_sql::durable::packages::PackageStatements::render(
                    crate::schema_layout::MAIN,
                );
                let held = tx
                    .prepare_cached(sql.labels_held.sql())?
                    .query_row(
                        params![
                            MODULE_ARTIFACT_NAMESPACE,
                            reference,
                            referrer.kind().as_str(),
                            referrer.canonical_id()
                        ],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !held {
                    return Err(artifact_failure(ArtifactStoreError::ArtifactMissing {
                        artifact_ref: reference,
                    }));
                }
                tx.prepare_cached(sql.insert_labels.sql())?
                    .execute(params![
                        MODULE_ARTIFACT_NAMESPACE,
                        reference,
                        digest.as_slice(),
                        json
                    ])?;
                Ok(())
            })
            .await
            .map_err(artifact_sqlite_error)
            .map_err(Into::into)
    }
    async fn package_labels(
        &self,
        reference: &str,
    ) -> Result<Vec<String>, lash_durable::DurableError> {
        let reference = reference.to_owned();
        self.conn
            .call(move |tx| {
                let sql = lash_store_sql::durable::packages::PackageStatements::render(
                    crate::schema_layout::MAIN,
                );
                tx.prepare_cached(sql.labels.sql())?
                    .query_map(params![MODULE_ARTIFACT_NAMESPACE, reference], |row| {
                        row.get(0)
                    })?
                    .collect::<rusqlite::Result<Vec<String>>>()
            })
            .await
            .map_err(artifact_sqlite_error)
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
        self.end_artifact_referrer(MODULE_ARTIFACT_NAMESPACE, cleanup.clone())
            .await
            .map_err(Into::into)
    }
}
