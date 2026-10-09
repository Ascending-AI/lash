//! Reading a version out of one stored payload, for each surface a walk
//! enumerates.
//!
//! All the format knowledge lives here rather than in the backends, and that is
//! the point of the split: which field carries a version, whether a found
//! version opens, and what a missing field means are one build-wide set of
//! answers. A backend that answered them locally would be a second place for
//! them to drift, and drift between "the version the runtime writes" and "the
//! version the probe compares" is precisely the bug a preflight cannot have.
//!
//! Every function here is total over arbitrary bytes. A payload that will not
//! parse produces an [`Extraction::Undecodable`] carrying the reason, never a
//! panic and never a silent skip: the probe's whole job is to describe stored
//! data it may not be able to read.

use lash_core::{DurableItem, DurablePayload, DurableSurface};
#[cfg(feature = "rlm")]
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;

use super::msgpack;
use super::{PRIMARY_FORMATS, SurfaceRelation, format_surface};
use crate::formats::DurableFormat;

/// One format observation pulled out of one stored payload.
pub(super) enum Extraction {
    /// A version was read.
    Found {
        /// Which format the version belongs to.
        format: DurableFormat,
        /// The version the stored bytes carry.
        version: u32,
    },
    /// The bytes could not be read far enough to find this format's version.
    Undecodable {
        /// Which format was being looked for.
        format: DurableFormat,
        /// What stopped the read, in operator-facing words.
        reason: String,
    },
    /// The item carries a stored identity that is not this build's.
    ///
    /// Distinct from a version mismatch because there is no found version to
    /// report: the identity is a hash whose preimage includes a format version,
    /// so a mismatch is a decided refusal that no integer describes.
    ///
    /// Available when this build carries the optional Lash VM verifier.
    #[cfg(feature = "rlm")]
    IdentityMismatch {
        /// Which format the identity belongs to.
        format: DurableFormat,
        /// The refusal, in operator-facing words.
        detail: String,
    },
    /// The stored identity was successfully verified. Identity-only formats
    /// have no found version integer to report, so this increments the scan
    /// count without inventing one.
    #[cfg(feature = "rlm")]
    IdentityMatch { format: DurableFormat },
}

/// The format a surface's payload primarily carries, used to attribute a
/// payload that could not be fetched at all.
pub(super) fn primary_format(surface: DurableSurface) -> DurableFormat {
    PRIMARY_FORMATS
        .iter()
        .find_map(|format| match format_surface(*format) {
            SurfaceRelation::Walk {
                surface: primary_surface,
                primary: true,
            } if primary_surface == surface => Some(*format),
            SurfaceRelation::Walk { .. }
            | SurfaceRelation::CarriedBy(_)
            | SurfaceRelation::Unwalkable(_)
            | SurfaceRelation::NotPersisted => None,
        })
        // A surface this build does not know is not a surface it can
        // attribute; the manifest row it lands on is the checkpoint manifest,
        // which is the one every backend has.
        .unwrap_or(DurableFormat::SessionCheckpointManifest)
}

/// Every format observation one item yields.
pub(super) async fn extract(
    item: &DurableItem,
    #[cfg(feature = "rlm")] workers: &lash_vm_client::service::Service,
) -> Vec<Extraction> {
    let format = primary_format(item.surface);
    let payload = match &item.payload {
        DurablePayload::Json(text) => Payload::Json(text.as_str()),
        DurablePayload::MessagePack(bytes) => Payload::MessagePack(bytes.as_slice()),
        DurablePayload::Missing { reason } => {
            return vec![Extraction::Undecodable {
                format,
                reason: format!("payload could not be read: {reason}"),
            }];
        }
        // A framing this build does not know is not a framing it can read a
        // version out of, and guessing would be worse than saying so.
        _ => {
            return vec![Extraction::Undecodable {
                format,
                reason: "payload uses a framing this build does not recognise".to_string(),
            }];
        }
    };
    match item.surface {
        DurableSurface::StartedProcess => started_process(payload),
        DurableSurface::SessionCheckpoint => session_checkpoint(payload),
        DurableSurface::SessionExecutionState => session_execution_state(payload),
        DurableSurface::ModuleArtifact => {
            module_artifact(
                payload,
                #[cfg(feature = "rlm")]
                workers,
            )
            .await
        }
        _ => Vec::new(),
    }
}

/// Inspect one persisted module artifact without inventing a version field.
///
/// With `rlm`, the artifact's module ref is the identity fence: a valid current
/// artifact contributes a readable identity, while a hash mismatch or known
/// future shape is a decided refusal. Without the verifier, the manifest row
/// remains visible but stored artifacts are honestly undecidable. Malformed
/// JSON is likewise undecidable because it is not evidence of another build.
async fn module_artifact(
    payload: Payload<'_>,
    #[cfg(feature = "rlm")] workers: &lash_vm_client::service::Service,
) -> Vec<Extraction> {
    let format = DurableFormat::ModuleArtifact;
    #[cfg(not(feature = "rlm"))]
    {
        let _ = payload;
        vec![Extraction::Undecodable {
            format,
            reason: "module artifact identity verification requires the `rlm` feature".to_string(),
        }]
    }
    #[cfg(feature = "rlm")]
    {
        let bytes = match payload {
            Payload::Json(text) => text.as_bytes(),
            Payload::MessagePack(_) => {
                return vec![Extraction::Undecodable {
                    format,
                    reason: "payload is MessagePack where this format is JSON".to_string(),
                }];
            }
        };
        use lash_vm_client::service::{ArtifactVerification, Request, Response};
        match workers
            .request_accounted(Request::VerifyArtifact {
                bytes: bytes.to_vec(),
            })
            .await
        {
            Ok(Response::ArtifactVerification(ArtifactVerification::Match)) => {
                vec![Extraction::IdentityMatch { format }]
            }
            Ok(Response::ArtifactVerification(ArtifactVerification::Refused(refusal))) => {
                match refusal {
                    lash_vm::ModuleArtifactRefusal::Generation(source) => {
                        vec![Extraction::IdentityMismatch {
                            format,
                            detail: format!("{source}; recompile and republish the module"),
                        }]
                    }
                    lash_vm::ModuleArtifactRefusal::Corrupt(source) => {
                        vec![Extraction::Undecodable {
                            format,
                            reason: source.to_string(),
                        }]
                    }
                }
            }
            Ok(_) => vec![Extraction::Undecodable {
                format,
                reason: "unexpected worker artifact verification response".into(),
            }],
            Err(error) => vec![Extraction::Undecodable {
                format,
                reason: format!("worker artifact verification failed: {error}"),
            }],
        }
    }
}

/// The two framings a walk yields, narrowed to borrowed bytes.
#[derive(Clone, Copy)]
enum Payload<'a> {
    Json(&'a str),
    MessagePack(&'a [u8]),
}

impl<'a> Payload<'a> {
    fn json(self, format: DurableFormat) -> Result<serde_json::Value, Extraction> {
        match self {
            Payload::Json(text) => {
                serde_json::from_str(text).map_err(|error| Extraction::Undecodable {
                    format,
                    reason: format!("payload is not JSON: {error}"),
                })
            }
            Payload::MessagePack(_) => Err(Extraction::Undecodable {
                format,
                reason: "payload is MessagePack where this format is JSON".to_string(),
            }),
        }
    }

    fn messagepack(self, format: DurableFormat) -> Result<&'a [u8], Extraction> {
        match self {
            Payload::MessagePack(bytes) => Ok(bytes),
            Payload::Json(_) => Err(Extraction::Undecodable {
                format,
                reason: "payload is JSON where this format is MessagePack".to_string(),
            }),
        }
    }
}

/// The start stamp of a started process (FIG-3571), a recompute: the stamp
/// names the executable generation the incarnation runs under, and the only
/// honest check is to recompute the generation this build would run its input
/// as. A process that has not started carries no stamp and yields nothing; a
/// started one whose stamp is missing or another build's parks at its next
/// claim, so it is a refusal.
fn started_process(payload: Payload<'_>) -> Vec<Extraction> {
    let format = DurableFormat::Bytecode;
    let record = match payload.json(format) {
        Ok(record) => record,
        Err(extraction) => return vec![extraction],
    };
    let Some(started) = record
        .get("first_started")
        .filter(|started| !started.is_null())
    else {
        return Vec::new();
    };
    let stamp = started
        .get("generation")
        .and_then(serde_json::Value::as_str);
    start_generation(&record, stamp).into_iter().collect()
}

#[cfg(feature = "rlm")]
fn start_generation(record: &serde_json::Value, stamp: Option<&str>) -> Option<Extraction> {
    let format = DurableFormat::Bytecode;
    let input = record.get("input")?;
    // Only a Lash VM engine process runs under a generation; a tool-call or
    // session-turn process has nothing to recompute and is not a gap.
    if input.get("type").and_then(serde_json::Value::as_str) != Some("engine")
        || input.get("kind").and_then(serde_json::Value::as_str)
            != Some(lash_vm_runtime::LASH_VM_ENGINE_KIND)
    {
        return None;
    }
    let payload = input.get("payload")?;
    let Ok(parsed) = serde_json::from_value::<lash_vm_runtime::LashVmProcessInput>(payload.clone())
    else {
        return Some(Extraction::Undecodable {
            format,
            reason: "started process payload is not a lash_vm process input".to_string(),
        });
    };
    let current = parsed.executable_generation();
    Some(if stamp == Some(current.as_str()) {
        Extraction::IdentityMatch { format }
    } else {
        Extraction::IdentityMismatch {
            format,
            detail: format!(
                "the process was started under executable generation {}; bytecode v{} runs it \
                 as {current}, so its next claim parks it as a retired generation",
                stamp.unwrap_or("none"),
                crate::formats::BYTECODE_FORMAT_VERSION
            ),
        }
    })
}

/// A build without the language cannot recompute a start stamp.
#[cfg(not(feature = "rlm"))]
fn start_generation(_record: &serde_json::Value, _stamp: Option<&str>) -> Option<Extraction> {
    None
}

/// A checkpoint manifest answers for itself and for every component descriptor
/// it names — the component encodings are stored in the manifest, not in the
/// component bodies, so one blob read decides both formats.
fn session_checkpoint(payload: Payload<'_>) -> Vec<Extraction> {
    let manifest = DurableFormat::SessionCheckpointManifest;
    let bytes = match payload.messagepack(manifest) {
        Ok(bytes) => bytes,
        Err(extraction) => return vec![extraction],
    };
    let root = msgpack::Value::root(bytes);
    let mut found = Vec::new();
    match root
        .field("schema_version")
        .and_then(msgpack::Value::as_u32)
    {
        Some(version) => found.push(Extraction::Found {
            format: manifest,
            version,
        }),
        None => found.push(Extraction::Undecodable {
            format: manifest,
            reason: "checkpoint root carries no readable `schema_version`".to_string(),
        }),
    }
    let encoding = DurableFormat::CheckpointComponentEncoding;
    // An absent `components` map is a checkpoint with no components, which the
    // manifest omits rather than encodes as empty. Nothing to report is not the
    // same as nothing readable.
    if let Some(components) = root.field("components").and_then(msgpack::Value::entries) {
        for (key, descriptor) in components {
            match descriptor
                .field("encoding_version")
                .and_then(msgpack::Value::as_u32)
            {
                Some(version) => found.push(Extraction::Found {
                    format: encoding,
                    version,
                }),
                None => found.push(Extraction::Undecodable {
                    format: encoding,
                    reason: format!("component `{key}` carries no readable `encoding_version`"),
                }),
            }
        }
    }
    found
}

fn session_execution_state(payload: Payload<'_>) -> Vec<Extraction> {
    let format = DurableFormat::RlmSnapshotEnvelope;
    let bytes = match payload.messagepack(format) {
        Ok(bytes) => bytes,
        Err(extraction) => return vec![extraction],
    };
    match msgpack::Value::root(bytes)
        .field("version")
        .and_then(msgpack::Value::as_u32)
    {
        Some(version) => vec![Extraction::Found { format, version }],
        None => vec![Extraction::Undecodable {
            format,
            reason: "execution-state root carries no readable `version`".to_string(),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_sansio::ProcessId;
    use lash_sansio::SessionId;

    async fn extract(item: &DurableItem) -> Vec<Extraction> {
        super::extract(
            item,
            #[cfg(feature = "rlm")]
            &lash_vm_client::service::Service::default(),
        )
        .await
    }

    fn item(surface: DurableSurface, payload: DurablePayload) -> DurableItem {
        DurableItem {
            surface,
            cursor: "c".to_string(),
            process_id: Some(ProcessId::fixture("p-1")),
            session_id: Some(SessionId::from("s-1")),
            status: Some("waiting".to_string()),
            owner_record: None,
            payload,
        }
    }

    fn versions(extractions: &[Extraction], wanted: DurableFormat) -> Vec<u32> {
        extractions
            .iter()
            .filter_map(|extraction| match extraction {
                Extraction::Found { format, version } if *format == wanted => Some(*version),
                _ => None,
            })
            .collect()
    }

    fn undecodable(extractions: &[Extraction], wanted: DurableFormat) -> Vec<&str> {
        extractions
            .iter()
            .filter_map(|extraction| match extraction {
                Extraction::Undecodable { format, reason } if *format == wanted => {
                    Some(reason.as_str())
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_payload_that_could_not_be_fetched_is_attributed_to_its_surface() {
        let extractions = extract(&item(
            DurableSurface::SessionCheckpoint,
            DurablePayload::Missing {
                reason: "blob sha256:abc is absent".to_string(),
            },
        ))
        .await;
        let reasons = undecodable(&extractions, DurableFormat::SessionCheckpointManifest);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("sha256:abc"), "{reasons:?}");
    }

    #[tokio::test]
    #[cfg(feature = "rlm")]
    async fn a_frozen_predecessor_module_artifact_retains_its_generation_refusal() {
        let mut raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../../lash-vm/tests/fixtures/module-artifact-old.json"
        ))
        .expect("frozen fixture should be JSON");
        let object = raw.as_object_mut().expect("artifact should be an object");
        // Reach the predecessor carrier past its retired top-level fields.
        object.remove("trigger_key_manifest");
        object.remove("compilation_dialect");
        let extractions = extract(&item(
            DurableSurface::ModuleArtifact,
            DurablePayload::Json(
                serde_json::to_string(&raw).expect("legacy artifact should encode"),
            ),
        ))
        .await;
        let [
            Extraction::IdentityMismatch {
                format: DurableFormat::ModuleArtifact,
                detail,
            },
        ] = extractions.as_slice()
        else {
            panic!("predecessor carrier must retain its generation refusal");
        };
        assert!(
            detail.contains("unsupported artifact shape artifact shape: unknown field"),
            "{detail}"
        );
        assert!(detail.contains("recompile and republish"), "{detail}");
    }

    #[tokio::test]
    #[cfg(not(feature = "rlm"))]
    async fn a_module_artifact_is_undecidable_without_the_identity_verifier() {
        let extractions = extract(&item(
            DurableSurface::ModuleArtifact,
            DurablePayload::Json("{}".to_string()),
        ))
        .await;
        let reasons = undecodable(&extractions, DurableFormat::ModuleArtifact);
        assert_eq!(reasons.len(), 1);
        assert!(
            reasons[0].contains("requires the `rlm` feature"),
            "{reasons:?}"
        );
    }

    fn checkpoint_root(schema_version: u32, encodings: &[u32]) -> Vec<u8> {
        let components: serde_json::Map<String, serde_json::Value> = encodings
            .iter()
            .enumerate()
            .map(|(index, encoding)| {
                (
                    format!("component-{index}"),
                    serde_json::json!({"blob_ref": "sha256:abc", "encoding_version": encoding}),
                )
            })
            .collect();
        rmp_serde::to_vec_named(&serde_json::json!({
            "schema_version": schema_version,
            "turn_state": {"anything": [1, 2, 3]},
            "components": components,
        }))
        .expect("the fixture encodes")
    }

    #[tokio::test]
    async fn a_checkpoint_answers_for_its_manifest_and_every_component_encoding() {
        // One blob read decides two formats, because the component encodings
        // live in the manifest rather than in the component bodies.
        let extractions = extract(&item(
            DurableSurface::SessionCheckpoint,
            DurablePayload::MessagePack(checkpoint_root(2, &[2, 2, 3])),
        ))
        .await;
        assert_eq!(
            versions(&extractions, DurableFormat::SessionCheckpointManifest),
            vec![2]
        );
        assert_eq!(
            versions(&extractions, DurableFormat::CheckpointComponentEncoding),
            vec![2, 2, 3]
        );
    }

    #[tokio::test]
    async fn a_checkpoint_with_no_components_reports_nothing_rather_than_undecodable() {
        let bytes = rmp_serde::to_vec_named(&serde_json::json!({"schema_version": 2}))
            .expect("the fixture encodes");
        let extractions = extract(&item(
            DurableSurface::SessionCheckpoint,
            DurablePayload::MessagePack(bytes),
        ))
        .await;
        assert_eq!(
            versions(&extractions, DurableFormat::SessionCheckpointManifest),
            vec![2]
        );
        assert!(
            undecodable(&extractions, DurableFormat::CheckpointComponentEncoding).is_empty(),
            "a checkpoint that stores no components has nothing unreadable"
        );
    }

    /// C8 (FIG-3571): a started process carries its executable generation on
    /// its start record. The probe recomputes
    /// the generation this build runs its input as: a matching stamp is
    /// readable, a foreign or missing stamp is a decided refusal, and a
    /// process that has not started yields nothing.
    #[cfg(feature = "rlm")]
    #[tokio::test]
    async fn a_started_process_is_judged_by_its_start_stamp() {
        let hash = lash_vm::ContentHash::new("00ff");
        let input = lash_vm_runtime::LashVmProcessInput {
            module_ref: lash_vm::ModuleRef::new(&hash),
            process_ref: lash_vm::ProcessRef::new(hash.clone(), 0),
            host_requirements_ref: lash_vm::HostRequirementsRef::new(&hash),
            process_name: "worker".to_string(),
            args: serde_json::Map::new(),
        };
        let current = input.executable_generation();
        let record = |first_started: serde_json::Value| {
            item(
                DurableSurface::StartedProcess,
                DurablePayload::Json(
                    serde_json::json!({
                        "input": {
                            "type": "engine",
                            "kind": lash_vm_runtime::LASH_VM_ENGINE_KIND,
                            "payload": serde_json::to_value(&input).expect("the input serializes"),
                        },
                        "first_started": first_started,
                    })
                    .to_string(),
                ),
            )
        };
        let refused = |extractions: &[Extraction]| {
            extractions.iter().any(|extraction| {
                matches!(
                    extraction,
                    Extraction::IdentityMismatch {
                        format: DurableFormat::Bytecode,
                        ..
                    }
                )
            })
        };

        let stamped = extract(&record(
            serde_json::json!({ "generation": current.as_str() }),
        ))
        .await;
        assert!(
            matches!(
                stamped.as_slice(),
                [Extraction::IdentityMatch {
                    format: DurableFormat::Bytecode
                }]
            ),
            "a start stamped with the generation this build runs is readable"
        );
        assert!(
            refused(
                &extract(&record(
                    serde_json::json!({ "generation": "blake3:another-build" })
                ))
                .await
            ),
            "a start another build stamped is a decided refusal"
        );
        assert!(
            refused(&extract(&record(serde_json::json!({ "attempt": 1 }))).await),
            "a start written before the stamp existed is a decided refusal"
        );
        assert!(
            extract(&record(serde_json::Value::Null)).await.is_empty(),
            "a process that has not started carries no stamp"
        );
    }
}
