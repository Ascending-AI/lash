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

use lash_core::durable_port::domain::checkpoint_graph::{
    EncodedNode as GraphNode, NodeId, NodeKind,
};
use lash_core::{DurableItem, DurablePayload, DurableSurface};

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
            SurfaceRelation::Walk { .. } | SurfaceRelation::Unwalkable(_) => None,
        })
        // A surface this build does not know is not a surface it can
        // attribute; the manifest row it lands on is the checkpoint manifest,
        // which is the one every backend has.
        .unwrap_or(DurableFormat::SessionCheckpointManifest)
}

/// Every format observation one item yields.
pub(super) fn extract(item: &DurableItem) -> Vec<Extraction> {
    let format = primary_format(item.surface);
    let payload = match &item.payload {
        DurablePayload::Json(text) => Payload::Json(text.as_str()),
        DurablePayload::GraphNode(bytes) => Payload::Graph(bytes.as_slice()),
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
        DurableSurface::SessionCheckpoint => session_checkpoint(payload),
        DurableSurface::SessionExecutionState => session_execution_state(payload),
        DurableSurface::ModuleArtifact => kernel_document(payload),
        _ => Vec::new(),
    }
}

/// A stored kernel document states the kernel version it is written in, in
/// its manifest: the module store holds nothing else. A text that is not a
/// document is undecidable, since it is not evidence of another build.
fn kernel_document(payload: Payload<'_>) -> Vec<Extraction> {
    let format = DurableFormat::KernelDocument;
    let document = match payload.json(format) {
        Ok(document) => document,
        Err(extraction) => return vec![extraction],
    };
    match document
        .pointer("/manifest/kernel")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
    {
        Some(version) => vec![Extraction::Found { format, version }],
        None => vec![Extraction::Undecodable {
            format,
            reason: "document manifest carries no readable `kernel` version".to_string(),
        }],
    }
}

/// The framings a walk yields, narrowed to borrowed bytes.
#[derive(Clone, Copy)]
enum Payload<'a> {
    Json(&'a str),
    Graph(&'a [u8]),
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
            Payload::Graph(_) => Err(Extraction::Undecodable {
                format,
                reason: format!("payload is {} where this format is JSON", self.framing()),
            }),
        }
    }

    /// The checkpoint graph node the payload's bytes encode, identified by
    /// their hash.
    fn graph(self, format: DurableFormat) -> Result<GraphNode, Extraction> {
        match self {
            Payload::Graph(bytes) => Ok(GraphNode {
                id: NodeId::of_envelope(bytes),
                bytes: bytes.into(),
            }),
            Payload::Json(_) => Err(Extraction::Undecodable {
                format,
                reason: format!(
                    "payload is {} where this format is a checkpoint graph node",
                    self.framing()
                ),
            }),
        }
    }

    fn framing(self) -> &'static str {
        match self {
            Payload::Json(_) => "JSON",
            Payload::Graph(_) => "a checkpoint graph node",
        }
    }
}

/// A checkpoint manifest answers for itself and for every component descriptor
/// it names — the component encodings are stored in the manifest, not in the
/// component bodies, so one blob read decides both formats.
fn session_checkpoint(payload: Payload<'_>) -> Vec<Extraction> {
    let format = DurableFormat::SessionCheckpointManifest;
    let node = match payload.graph(format) {
        Ok(node) => node,
        Err(extraction) => return vec![extraction],
    };
    let Some(manifest) = lash_core::store::SessionCheckpoint::named(&node) else {
        return vec![Extraction::Undecodable {
            format,
            reason: "checkpoint root is not a readable SessionManifest node".to_string(),
        }];
    };
    // A checkpoint with no components names none, which is nothing to
    // report rather than nothing readable.
    std::iter::once(Extraction::Found {
        format,
        version: manifest.schema_version,
    })
    .chain(
        manifest
            .components
            .into_iter()
            .map(|(_, version)| Extraction::Found {
                format: DurableFormat::CheckpointComponentEncoding,
                version,
            }),
    )
    .collect()
}

/// A session's execution state is a session descriptor page (ADR 0145
/// §9), which carries no version of its own: the 1.0 node registry is
/// frozen, and a later shape is a new kind. A root of the registered kind
/// is this build's surface version; any other node is not readable as one.
fn session_execution_state(payload: Payload<'_>) -> Vec<Extraction> {
    let format = DurableFormat::CodeModeSnapshotEnvelope;
    let node = match payload.graph(format) {
        Ok(node) => node,
        Err(extraction) => return vec![extraction],
    };
    match node.envelope() {
        Ok(envelope) if envelope.kind == NodeKind::SESSION_ROOT => execution_surface(format),
        Ok(envelope) => vec![Extraction::Undecodable {
            format,
            reason: format!(
                "execution-state root is a node of kind {}, not a SessionRoot page",
                envelope.kind.0
            ),
        }],
        Err(refusal) => vec![Extraction::Undecodable {
            format,
            reason: format!("execution-state root is not a readable graph node: {refusal}"),
        }],
    }
}

#[cfg(feature = "rlm")]
fn execution_surface(format: DurableFormat) -> Vec<Extraction> {
    vec![Extraction::Found {
        format,
        version: crate::formats::CODEMODE_SNAPSHOT_VERSION,
    }]
}

/// A build without a code mode executor reads no session execution state.
#[cfg(not(feature = "rlm"))]
fn execution_surface(format: DurableFormat) -> Vec<Extraction> {
    vec![Extraction::Undecodable {
        format,
        reason: "this build holds no code mode executor to read session execution state"
            .to_string(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_protocol_types::ProcessId;
    use lash_protocol_types::SessionId;

    async fn extract(item: &DurableItem) -> Vec<Extraction> {
        super::extract(item)
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

    /// A stored document is judged by the kernel version its manifest
    /// states; a text with none is undecidable, never another build's.
    #[tokio::test]
    async fn a_stored_document_reports_the_kernel_version_it_is_written_in() {
        let stored = |text: &str| {
            item(
                DurableSurface::ModuleArtifact,
                DurablePayload::Json(text.to_string()),
            )
        };
        let extractions = extract(&stored(r#"{"manifest":{"kernel":7,"numbers":"exact"}}"#)).await;
        assert_eq!(
            versions(&extractions, DurableFormat::KernelDocument),
            vec![7]
        );
        let extractions = extract(&stored("{}")).await;
        let reasons = undecodable(&extractions, DurableFormat::KernelDocument);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("`kernel` version"), "{reasons:?}");
    }

    /// A SessionManifest node's bytes stamped `schema_version`, naming one
    /// component per encoding version in `encodings`.
    fn checkpoint_root(schema_version: u32, encodings: &[u32]) -> Vec<u8> {
        let blob = NodeId::of_envelope(b"component body").to_hex();
        let manifest = lash_core::store::SessionCheckpoint {
            schema_version,
            turn_state: lash_core::PersistedTurnState::default(),
            components: encodings
                .iter()
                .enumerate()
                .map(|(index, encoding)| {
                    (
                        format!("component-{index}"),
                        lash_core::store::CheckpointComponentDescriptor {
                            blob_ref: lash_core::store::BlobRef(blob.clone()).into(),
                            encoding_version: *encoding,
                        },
                    )
                })
                .collect(),
            execution_ref: None,
        };
        let (root, nodes) = manifest.to_graph().expect("the fixture stages");
        nodes[&root].bytes.to_vec()
    }

    #[tokio::test]
    async fn a_checkpoint_answers_for_its_manifest_and_every_component_encoding() {
        // One node read decides two formats, because the component encodings
        // live in the manifest rather than in the component bodies.
        let extractions = extract(&item(
            DurableSurface::SessionCheckpoint,
            DurablePayload::GraphNode(checkpoint_root(2, &[2, 2, 3])),
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
        let extractions = extract(&item(
            DurableSurface::SessionCheckpoint,
            DurablePayload::GraphNode(checkpoint_root(2, &[])),
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
}
