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

use super::msgpack;
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
    /// Available when this build carries the optional Lashlang verifier.
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
    match surface {
        DurableSurface::ParkedSegment => DurableFormat::LashlangSegmentHandover,
        DurableSurface::PendingWake => DurableFormat::ProcessWakeDelivery,
        DurableSurface::SessionCheckpoint => DurableFormat::SessionCheckpointManifest,
        DurableSurface::SessionExecutionState => DurableFormat::RlmSnapshotEnvelope,
        DurableSurface::ModuleArtifact => DurableFormat::ModuleArtifact,
        // A surface this build does not know is not a surface it can attribute;
        // the manifest row it lands on is the checkpoint manifest, which is the
        // one every backend has.
        _ => DurableFormat::SessionCheckpointManifest,
    }
}

/// Every format observation one item yields.
pub(super) fn extract(item: &DurableItem) -> Vec<Extraction> {
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
        DurableSurface::ParkedSegment => parked_segment(payload, item.owner_record.as_deref()),
        DurableSurface::PendingWake => pending_wake(payload),
        DurableSurface::SessionCheckpoint => session_checkpoint(payload),
        DurableSurface::SessionExecutionState => session_execution_state(payload),
        DurableSurface::ModuleArtifact => module_artifact(payload),
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
fn module_artifact(payload: Payload<'_>) -> Vec<Extraction> {
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
        match lashlang::ModuleArtifact::from_store_bytes(bytes) {
            Ok(_) => vec![Extraction::IdentityMatch { format }],
            Err(lashlang::ModuleArtifactError::Codec(reason)) => vec![Extraction::Undecodable {
                format,
                reason: format!("module artifact is not readable JSON: {reason}"),
            }],
            Err(error) => vec![Extraction::IdentityMismatch {
                format,
                detail: error.to_string(),
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

fn as_u32(value: Option<&serde_json::Value>) -> Option<u32> {
    value
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
}

/// A parked segment handover carries three of the enumerated formats at once:
/// its own envelope version, the VM continuation nested inside it, and the
/// program identity the process resumes against.
fn parked_segment(payload: Payload<'_>, owner_record: Option<&str>) -> Vec<Extraction> {
    let handover = DurableFormat::LashlangSegmentHandover;
    let root = match payload.json(handover) {
        Ok(root) => root,
        Err(extraction) => return vec![extraction],
    };
    let mut found = Vec::new();

    // `engine_state` is the engine-private continuation, stored as a byte
    // sequence rather than as nested JSON, so the outer envelope stays engine
    // agnostic. Reading it is one un-nesting, not a decode: the bytes are UTF-8
    // JSON whose first field is the version this build compares.
    let engine_state = root
        .get("handover")
        .and_then(|handover| handover.get("engine_state"))
        .and_then(serde_json::Value::as_array)
        .map(|bytes| {
            bytes
                .iter()
                .filter_map(serde_json::Value::as_u64)
                .filter_map(|byte| u8::try_from(byte).ok())
                .collect::<Vec<u8>>()
        });
    match engine_state {
        None => found.push(Extraction::Undecodable {
            format: handover,
            reason: "segment handover carries no `handover.engine_state` byte sequence".to_string(),
        }),
        Some(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Err(error) => found.push(Extraction::Undecodable {
                format: handover,
                reason: format!("segment engine state is not JSON: {error}"),
            }),
            Ok(state) => {
                match as_u32(state.get("version")) {
                    Some(version) => found.push(Extraction::Found {
                        format: handover,
                        version,
                    }),
                    // The engine's own decoder reads an absent version as
                    // generation zero and refuses it; reporting it as
                    // undecodable would be gentler than the boundary is.
                    None => found.push(Extraction::Found {
                        format: handover,
                        version: 0,
                    }),
                }
                let continuation = DurableFormat::VmContinuation;
                match as_u32(state.get("vm").and_then(|vm| vm.get("format_version"))) {
                    Some(version) => found.push(Extraction::Found {
                        format: continuation,
                        version,
                    }),
                    None => found.push(Extraction::Undecodable {
                        format: continuation,
                        reason: "segment engine state carries no `vm.format_version`".to_string(),
                    }),
                }
            }
        },
    }

    if let Some(extraction) = program_identity(&root, owner_record) {
        found.push(extraction);
    }
    found
}

/// The bytecode identity check, which is a recompute rather than a comparison.
///
/// No stored bytes say "this was compiled by bytecode version N": the process
/// records a hash whose preimage includes the bytecode format version and the
/// module, process and host-requirement references it was built from. The only
/// honest check is to recompute the identity this build would mint for the same
/// inputs and compare — which is why the walk carries the owner's record at all.
#[cfg(feature = "rlm")]
fn program_identity(root: &serde_json::Value, owner_record: Option<&str>) -> Option<Extraction> {
    let format = DurableFormat::Bytecode;
    let persisted = root.get("handover")?.get("program_hash")?.as_str()?;
    let record: serde_json::Value = serde_json::from_str(owner_record?).ok()?;
    let input = record.get("input")?;
    // Only Lashlang engine processes carry a bytecode identity; a tool-call or
    // session-turn process has nothing to recompute and is not a gap.
    if input.get("type").and_then(serde_json::Value::as_str) != Some("engine")
        || input.get("kind").and_then(serde_json::Value::as_str)
            != Some(lash_lashlang_runtime::LASHLANG_ENGINE_KIND)
    {
        return None;
    }
    let payload = input.get("payload")?;
    let parsed: lash_lashlang_runtime::LashlangProcessInput =
        serde_json::from_value(payload.clone()).ok()?;
    let current = lash_lashlang_runtime::lashlang_program_hash(&parsed);
    if current == persisted {
        Some(Extraction::Found {
            format,
            version: crate::formats::BYTECODE_FORMAT_VERSION,
        })
    } else {
        Some(Extraction::IdentityMismatch {
            format,
            detail: format!(
                "program identity {persisted} was minted by another build; \
                 bytecode v{} mints {current}",
                crate::formats::BYTECODE_FORMAT_VERSION
            ),
        })
    }
}

/// A build without the language has no bytecode identity to recompute, and
/// claiming one would be reporting a comparison this build cannot make.
#[cfg(not(feature = "rlm"))]
fn program_identity(_root: &serde_json::Value, _owner_record: Option<&str>) -> Option<Extraction> {
    None
}

fn pending_wake(payload: Payload<'_>) -> Vec<Extraction> {
    let format = DurableFormat::ProcessWakeDelivery;
    let root = match payload.json(format) {
        Ok(root) => root,
        Err(extraction) => return vec![extraction],
    };
    match as_u32(root.get("version")) {
        Some(version) => vec![Extraction::Found { format, version }],
        // The wake payload's decoder defaults an absent version to this
        // build's, so an unstamped row genuinely opens. Reporting it as a
        // refusal would flag every row written before the field existed as a
        // drain blocker it is not.
        None => vec![Extraction::Found {
            format,
            version: crate::formats::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        }],
    }
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

    fn item(surface: DurableSurface, payload: DurablePayload) -> DurableItem {
        DurableItem {
            surface,
            cursor: "c".to_string(),
            process_id: Some("p-1".to_string()),
            session_id: Some("s-1".to_string()),
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

    fn segment_handover(segment_version: u32, continuation_version: u32) -> String {
        let engine_state = serde_json::json!({
            "version": segment_version,
            "vm": {"format_version": continuation_version},
        });
        let bytes = serde_json::to_vec(&engine_state).expect("the fixture encodes");
        serde_json::json!({
            "segment_ordinal": 3,
            "handover": {
                "reason": "await",
                "program_hash": "sha256:abc",
                "engine_state": bytes,
            },
        })
        .to_string()
    }

    #[test]
    fn a_parked_segment_yields_both_of_its_nested_format_versions() {
        // One payload, two boundaries: the envelope this build wrote and the VM
        // continuation nested a level inside it.
        let extractions = extract(&item(
            DurableSurface::ParkedSegment,
            DurablePayload::Json(segment_handover(3, 8)),
        ));
        assert_eq!(
            versions(&extractions, DurableFormat::LashlangSegmentHandover),
            vec![3]
        );
        assert_eq!(
            versions(&extractions, DurableFormat::VmContinuation),
            vec![8]
        );
    }

    #[test]
    fn an_unstamped_segment_reads_as_generation_zero_not_as_readable() {
        // The engine's decoder reads an absent version as zero and refuses it;
        // a probe that reported "no version, cannot say" would be gentler than
        // the boundary the host will actually hit.
        let engine_state = serde_json::to_vec(&serde_json::json!({"vm": {"format_version": 8}}))
            .expect("the fixture encodes");
        let payload = serde_json::json!({
            "handover": {
                "program_hash": "sha256:abc",
                "engine_state": engine_state,
            },
        })
        .to_string();
        let extractions = extract(&item(
            DurableSurface::ParkedSegment,
            DurablePayload::Json(payload),
        ));
        assert_eq!(
            versions(&extractions, DurableFormat::LashlangSegmentHandover),
            vec![0]
        );
    }

    #[test]
    fn a_segment_whose_engine_state_is_junk_is_undecodable_rather_than_fatal() {
        let payload = serde_json::json!({
            "handover": {"engine_state": vec![0xffu8, 0xfe, 0xfd]},
        })
        .to_string();
        let extractions = extract(&item(
            DurableSurface::ParkedSegment,
            DurablePayload::Json(payload),
        ));
        assert_eq!(
            undecodable(&extractions, DurableFormat::LashlangSegmentHandover).len(),
            1
        );
        assert!(versions(&extractions, DurableFormat::VmContinuation).is_empty());
    }

    #[test]
    fn a_payload_that_is_not_json_at_all_is_undecodable_rather_than_fatal() {
        for payload in [
            DurablePayload::Json("}{ not json".to_string()),
            DurablePayload::MessagePack(vec![0xc1, 0xc1]),
        ] {
            let extractions = extract(&item(DurableSurface::ParkedSegment, payload));
            assert_eq!(
                undecodable(&extractions, DurableFormat::LashlangSegmentHandover).len(),
                1
            );
        }
    }

    #[test]
    fn a_payload_that_could_not_be_fetched_is_attributed_to_its_surface() {
        let extractions = extract(&item(
            DurableSurface::SessionCheckpoint,
            DurablePayload::Missing {
                reason: "blob sha256:abc is absent".to_string(),
            },
        ));
        let reasons = undecodable(&extractions, DurableFormat::SessionCheckpointManifest);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("sha256:abc"), "{reasons:?}");
    }

    #[test]
    #[cfg(feature = "rlm")]
    fn a_frozen_sha256_module_artifact_is_an_identity_refusal() {
        let extractions = extract(&item(
            DurableSurface::ModuleArtifact,
            DurablePayload::Json(
                include_str!("../../../lashlang/tests/fixtures/module-artifact-old.json")
                    .to_string(),
            ),
        ));
        let detail = extractions
            .iter()
            .find_map(|extraction| match extraction {
                Extraction::IdentityMismatch {
                    format: DurableFormat::ModuleArtifact,
                    detail,
                } => Some(detail.as_str()),
                _ => None,
            })
            .expect("the SHA-256 artifact should be refused by its identity fence");
        assert!(detail.contains("lashlang:v2:blake3:"), "{detail}");
    }

    #[test]
    #[cfg(feature = "rlm")]
    fn a_future_module_artifact_is_a_legible_identity_refusal() {
        let mut raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../../lashlang/tests/fixtures/module-artifact-old.json"
        ))
        .expect("frozen fixture should be JSON");
        raw["compilation_dialect"] = serde_json::json!("future_dialect");
        raw["canonical_ir"]["main"] = serde_json::json!({"FutureExpr": null});
        let extractions = extract(&item(
            DurableSurface::ModuleArtifact,
            DurablePayload::Json(
                serde_json::to_string(&raw).expect("future fixture should encode"),
            ),
        ));
        let detail = extractions
            .iter()
            .find_map(|extraction| match extraction {
                Extraction::IdentityMismatch {
                    format: DurableFormat::ModuleArtifact,
                    detail,
                } => Some(detail.as_str()),
                _ => None,
            })
            .expect("future artifact shape should refuse as an identity mismatch");
        assert!(detail.contains("recompile and republish"), "{detail}");
        assert!(!detail.contains("unknown variant"), "{detail}");
    }

    #[test]
    #[cfg(not(feature = "rlm"))]
    fn a_module_artifact_is_undecidable_without_the_identity_verifier() {
        let extractions = extract(&item(
            DurableSurface::ModuleArtifact,
            DurablePayload::Json("{}".to_string()),
        ));
        let reasons = undecodable(&extractions, DurableFormat::ModuleArtifact);
        assert_eq!(reasons.len(), 1);
        assert!(
            reasons[0].contains("requires the `rlm` feature"),
            "{reasons:?}"
        );
    }

    #[test]
    fn an_unstamped_wake_reads_as_this_builds_version() {
        // The opposite call from the segment case, and for a stated reason: the
        // wake payload's own decoder defaults an absent version to this
        // build's, so the row genuinely opens.
        let extractions = extract(&item(
            DurableSurface::PendingWake,
            DurablePayload::Json(serde_json::json!({"wake_id": "w-1"}).to_string()),
        ));
        assert_eq!(
            versions(&extractions, DurableFormat::ProcessWakeDelivery),
            vec![crate::formats::PROCESS_WAKE_DELIVERY_FORMAT_VERSION]
        );

        let stamped = extract(&item(
            DurableSurface::PendingWake,
            DurablePayload::Json(serde_json::json!({"version": 9}).to_string()),
        ));
        assert_eq!(
            versions(&stamped, DurableFormat::ProcessWakeDelivery),
            vec![9]
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

    #[test]
    fn a_checkpoint_answers_for_its_manifest_and_every_component_encoding() {
        // One blob read decides two formats, because the component encodings
        // live in the manifest rather than in the component bodies.
        let extractions = extract(&item(
            DurableSurface::SessionCheckpoint,
            DurablePayload::MessagePack(checkpoint_root(2, &[2, 2, 3])),
        ));
        assert_eq!(
            versions(&extractions, DurableFormat::SessionCheckpointManifest),
            vec![2]
        );
        assert_eq!(
            versions(&extractions, DurableFormat::CheckpointComponentEncoding),
            vec![2, 2, 3]
        );
    }

    #[test]
    fn a_checkpoint_with_no_components_reports_nothing_rather_than_undecodable() {
        let bytes = rmp_serde::to_vec_named(&serde_json::json!({"schema_version": 2}))
            .expect("the fixture encodes");
        let extractions = extract(&item(
            DurableSurface::SessionCheckpoint,
            DurablePayload::MessagePack(bytes),
        ));
        assert_eq!(
            versions(&extractions, DurableFormat::SessionCheckpointManifest),
            vec![2]
        );
        assert!(
            undecodable(&extractions, DurableFormat::CheckpointComponentEncoding).is_empty(),
            "a checkpoint that stores no components has nothing unreadable"
        );
    }

    #[test]
    fn an_execution_state_root_yields_the_envelope_version() {
        let bytes = rmp_serde::to_vec_named(&serde_json::json!({
            "version": 13,
            "engine": "rlm",
        }))
        .expect("the fixture encodes");
        let extractions = extract(&item(
            DurableSurface::SessionExecutionState,
            DurablePayload::MessagePack(bytes),
        ));
        assert_eq!(
            versions(&extractions, DurableFormat::RlmSnapshotEnvelope),
            vec![13]
        );
    }

    #[cfg(feature = "rlm")]
    #[test]
    fn a_new_writer_handover_exposes_its_nested_program_identity() {
        let hash = lashlang::ContentHash::new("00ff");
        let input = lash_lashlang_runtime::LashlangProcessInput {
            module_ref: lashlang::ModuleRef::new(&hash),
            process_ref: lashlang::ProcessRef::new(hash.clone(), 0),
            host_requirements_ref: lashlang::HostRequirementsRef::new(&hash),
            process_name: "worker".to_string(),
            args: serde_json::Map::new(),
        };
        let current = lash_lashlang_runtime::lashlang_program_hash(&input);
        let record = serde_json::json!({
            "input": {
                "type": "engine",
                "kind": lash_lashlang_runtime::LASHLANG_ENGINE_KIND,
                "payload": serde_json::to_value(&input).expect("the input serializes"),
            }
        })
        .to_string();
        let payload = serde_json::to_string(&lash_core::PersistedSegmentHandover {
            segment_ordinal: 1,
            handover: lash_core::SegmentHandover {
                reason: lash_core::BoundaryReason::JournalBudget,
                program_hash: current,
                engine_state: serde_json::to_vec(&serde_json::json!({
                    "version": crate::formats::LASHLANG_SEGMENT_STATE_VERSION,
                    "vm": {"format_version": crate::formats::VM_CONTINUATION_FORMAT_VERSION},
                }))
                .expect("the engine state serializes"),
            },
        })
        .expect("the new writer handover serializes");
        let mut parked = item(DurableSurface::ParkedSegment, DurablePayload::Json(payload));
        parked.owner_record = Some(record);

        assert_eq!(
            versions(&extract(&parked), DurableFormat::Bytecode),
            vec![crate::formats::BYTECODE_FORMAT_VERSION]
        );
    }

    #[cfg(feature = "rlm")]
    #[test]
    fn a_program_identity_from_another_build_is_a_refusal_with_no_version_to_name() {
        let hash = lashlang::ContentHash::new("00ff");
        let input = lash_lashlang_runtime::LashlangProcessInput {
            module_ref: lashlang::ModuleRef::new(&hash),
            process_ref: lashlang::ProcessRef::new(hash.clone(), 0),
            host_requirements_ref: lashlang::HostRequirementsRef::new(&hash),
            process_name: "worker".to_string(),
            args: serde_json::Map::new(),
        };
        let current = lash_lashlang_runtime::lashlang_program_hash(&input);
        let record = serde_json::json!({
            "input": {
                "type": "engine",
                "kind": lash_lashlang_runtime::LASHLANG_ENGINE_KIND,
                "payload": serde_json::to_value(&input).expect("the input serializes"),
            }
        })
        .to_string();

        let mut matching = item(
            DurableSurface::ParkedSegment,
            DurablePayload::Json(
                serde_json::json!({
                    "handover": {
                        "program_hash": current,
                        "engine_state": Vec::<u8>::new(),
                    },
                })
                .to_string(),
            ),
        );
        matching.owner_record = Some(record.clone());
        assert_eq!(
            versions(&extract(&matching), DurableFormat::Bytecode),
            vec![crate::formats::BYTECODE_FORMAT_VERSION],
            "an identity this build mints is the only evidence of readability there is"
        );

        let mut stale = matching.clone();
        stale.payload = DurablePayload::Json(
            serde_json::json!({
                "handover": {
                    "program_hash": "sha256:from-another-build",
                    "engine_state": Vec::<u8>::new(),
                },
            })
            .to_string(),
        );
        let extractions = extract(&stale);
        assert!(
            extractions.iter().any(|extraction| matches!(
                extraction,
                Extraction::IdentityMismatch {
                    format: DurableFormat::Bytecode,
                    ..
                }
            )),
            "a stale identity is a decided refusal, not an undecodable item"
        );
    }
}
