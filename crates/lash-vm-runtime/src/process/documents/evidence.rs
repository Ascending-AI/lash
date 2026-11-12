//! Source evidence a publication retains beside its document, and the
//! records a run of it reads back for its workers (FIG-7711).

use super::{KernelDocuments, artifact_error, document_artifact, store_write_refused};
use lash_core::{ArtifactName, ArtifactStoreId, ReferrerClaim};
use lash_execution_contracts::module_artifacts::{ModuleArtifactBody, ModuleArtifactHold};
use lash_execution_contracts::process::linking::*;
use lash_kernel_doc::{Document, DocumentBytes, IdentifiedDocument};
use lash_kernel_value::EvidenceSetId;
use lash_kernel_value::data_limits::KernelDataLimits;
use lash_kernel_value::names::DocumentId;
use std::collections::BTreeMap;
use std::sync::Arc;
use tracing::Instrument as _;

impl KernelDocuments {
    /// Admit a published document with the source evidence typed authoring
    /// retained for it, in one hold: the document, its evidence record and
    /// the publication's evidence set, which holds the record. Answers the
    /// set a run of the publication names, none when there is no evidence.
    pub async fn admit_published_document(
        &self,
        claim: &ReferrerClaim,
        bytes: &DocumentBytes,
    ) -> Result<(DocumentId, Option<EvidenceSetId>), EnginePortError> {
        let document = bytes.decode(KernelDataLimits::standard()).map_err(|e| {
            EnginePortError::Refused(AdmissionRefusal::InvalidDocument {
                message: e.to_string(),
            })
        })?;
        let evidence = bytes.evidence_record();
        if let Some(record) = &evidence {
            record
                .validate(document.id(), document.document())
                .map_err(|e| {
                    EnginePortError::Refused(AdmissionRefusal::InvalidDocument {
                        message: e.to_string(),
                    })
                })?;
        }
        self.admit_with_evidence(claim, &document, evidence.as_ref())
            .await
    }

    pub(super) async fn admit_with_evidence(
        &self,
        claim: &ReferrerClaim,
        document: &IdentifiedDocument,
        evidence: Option<&lash_kernel_doc::EvidenceRecord>,
    ) -> Result<(DocumentId, Option<EvidenceSetId>), EnginePortError> {
        let span = tracing::debug_span!("lash.artifacts.admit", document_id = %document.id(), document_bytes = document.body().len());
        async {
            self.check_identified_document(document)?;
            let artifact = document_artifact(&document.id());
            #[cfg(feature = "testing")]
            {
                lash_vm_client::packages::helper_document_work::published(document);
                lash_vm_client::packages::helper_document_work::staged(document);
            }
            // The record and the set the publication holds it through are
            // written with the document, in its one guarded transaction.
            let evidence = evidence.map(|record| {
                let record_bytes = record.encode();
                let record_id = lash_kernel_doc::EvidenceRecord::id_of(&record_bytes);
                let set = lash_kernel_doc::EvidenceSet {
                    records: BTreeMap::from([(document.id(), record_id)]),
                };
                let set_bytes = set.encode();
                let set_id = lash_kernel_doc::EvidenceSet::id_of(&set_bytes);
                (
                    lash_kernel_doc::EvidenceRecord::artifact_ref(record_id),
                    record_bytes,
                    set_id,
                    set_bytes,
                )
            });
            let held: Vec<&str> = evidence
                .iter()
                .map(|(record_ref, ..)| record_ref.as_str())
                .collect();
            let set_ref = evidence
                .as_ref()
                .map(|(_, _, set_id, _)| lash_kernel_doc::EvidenceSet::artifact_ref(*set_id));
            let mut holds = vec![ModuleArtifactHold::Publish {
                module_ref: &artifact.artifact_ref,
                body: ModuleArtifactBody::Document(document.body()),
            }];
            if let (Some((record_ref, record_bytes, _, set_bytes)), Some(set_ref)) =
                (&evidence, &set_ref)
            {
                holds.push(ModuleArtifactHold::Publish {
                    module_ref: record_ref,
                    body: ModuleArtifactBody::Bytes(record_bytes),
                });
                holds.push(ModuleArtifactHold::Holder {
                    module_ref: set_ref,
                    body: ModuleArtifactBody::Bytes(set_bytes),
                    holds: &held,
                });
            }
            async {
                self.store
                    .hold_module_artifacts(claim, &holds)
                    .await
                    .inspect_err(store_write_refused)
            }
            .instrument(
                tracing::debug_span!("lash.artifacts.store_write", document_id = %document.id()),
            )
            .await
            .map_err(artifact_error)?;
            Ok((document.id(), evidence.map(|(_, _, set_id, _)| set_id)))
        }
        .instrument(span)
        .await
    }
    /// Hold the publication's evidence set under `claim`, as a run of the
    /// publication holds it: the set holds its records. A set already
    /// reclaimed is missing, never re-created.
    pub async fn hold_evidence(
        &self,
        claim: &ReferrerClaim,
        set: EvidenceSetId,
    ) -> Result<(), EnginePortError> {
        let set_ref = lash_kernel_doc::EvidenceSet::artifact_ref(set);
        self.store
            .hold_module_artifacts(
                claim,
                &[ModuleArtifactHold::Retain {
                    module_ref: &set_ref,
                }],
            )
            .await
            .inspect_err(store_write_refused)
            .map_err(artifact_error)
    }

    /// The evidence records `set` names for the run's `documents`, read
    /// from the store and checked against their digests and documents.
    /// A record the set names that is not stored, or does not describe its
    /// document, refuses the run: evidence is never dropped or guessed.
    pub(crate) async fn run_evidence(
        &self,
        set: Option<EvidenceSetId>,
        documents: &BTreeMap<DocumentId, Arc<Document>>,
    ) -> Result<Vec<lash_vm_protocol::StoredEvidence>, EnginePortError> {
        let Some(set) = set else {
            return Ok(Vec::new());
        };
        let refused = |error: lash_kernel_doc::EvidenceRefusal| {
            EnginePortError::Refused(AdmissionRefusal::InvalidDocument {
                message: error.to_string(),
            })
        };
        let read = |artifact_ref: String| async move {
            self.store
                .get_module_artifact(&artifact_ref)
                .await
                .map_err(artifact_error)?
                .ok_or(EnginePortError::Missing {
                    artifact: ArtifactName {
                        store: ArtifactStoreId::KernelDocument,
                        artifact_ref,
                    },
                })
        };
        let set_bytes = read(lash_kernel_doc::EvidenceSet::artifact_ref(set)).await?;
        let records = lash_kernel_doc::EvidenceSet::decode(&set_bytes, set).map_err(refused)?;
        let mut evidence = Vec::new();
        for (document, record_id) in records.records {
            let Some(body) = documents.get(&document) else {
                continue;
            };
            let bytes = read(lash_kernel_doc::EvidenceRecord::artifact_ref(record_id)).await?;
            lash_kernel_doc::EvidenceRecord::decode(&bytes, record_id)
                .and_then(|record| record.validate(document, body))
                .map_err(refused)?;
            evidence.push(lash_vm_protocol::StoredEvidence {
                document,
                digest: record_id,
                body: lash_vm_protocol::EncodedPayload(bytes),
            });
        }
        Ok(evidence)
    }
}
