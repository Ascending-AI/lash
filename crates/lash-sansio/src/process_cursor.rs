//! The one process cursor (FIG-3571 §E): a single token that pages a process's
//! durable event history and resumes its live observation.
//!
//! `lashpc3:<epoch>:<process-reference>:<position>:<sequence>`
//!
//! - `epoch` names the live publisher route the cursor was minted under.
//! - `process-reference` is ONE opaque component naming the process: its
//!   minted, never-reused process id (FIG-3607). Hosts never read it.
//! - `position` is the live publisher position the holder has seen.
//! - `sequence` is the durable event high-water mark the holder has seen.
//!
//! The version stamp is explicit: a cursor from a retired version is refused
//! with a typed error that names the version it found, so under the current,
//! temporary clean-cutover policy an old cursor is never reinterpreted, and a
//! later migration could still identify it.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ProcessId;

/// The version stamp every current process cursor starts with.
pub const PROCESS_CURSOR_VERSION: &str = "lashpc3";

/// Cursor version stamps this build recognises only to refuse them.
///
/// `lashpc2` named a process by its reusable name and incarnation; its
/// component cannot name a minted process, so it is refused, never read.
const RETIRED_PROCESS_CURSOR_VERSIONS: &[&str] = &["lashpc1", "lashpc2"];

/// The epoch a cursor carries when no live publisher route existed for its
/// process when it was minted. It never equals a publisher epoch, so a
/// subscription from such a cursor always takes a snapshot.
pub const PROCESS_CURSOR_UNROUTED_EPOCH: &str = "unrouted";

/// The opaque process component of a [`ProcessCursor`].
///
/// It is the minted process id: an id is never reused, so the id alone names
/// the one process the cursor was minted against, and a cursor can never be
/// replayed against a successor.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProcessCursorReference(ProcessId);

impl ProcessCursorReference {
    /// The reference for one process. This is the single place the
    /// component's contents are decided.
    pub fn for_process(process_id: &ProcessId) -> Self {
        Self(process_id.clone())
    }

    /// Whether this reference names exactly this process.
    pub fn names(&self, process_id: &ProcessId) -> bool {
        self.0 == *process_id
    }

    /// The process this reference names.
    pub fn process_id(&self) -> &ProcessId {
        &self.0
    }

    fn parse(component: &str) -> Option<Self> {
        ProcessId::parse(component).ok().map(Self)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// Why a string is not a current process cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessCursorError {
    /// The cursor carries a retired version stamp. It is refused, never
    /// reinterpreted; `found` names the version so old state stays identifiable.
    RetiredVersion { found: String },
    /// The cursor is not a well-formed current cursor.
    Malformed,
}

impl fmt::Display for ProcessCursorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RetiredVersion { found } => write!(
                formatter,
                "process cursor version `{found}` is retired; this build reads only `{PROCESS_CURSOR_VERSION}` cursors"
            ),
            Self::Malformed => formatter.write_str("malformed process cursor"),
        }
    }
}

impl std::error::Error for ProcessCursorError {}

/// One position in a process's durable history and live observation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProcessCursor {
    epoch: String,
    reference: ProcessCursorReference,
    position: u64,
    sequence: u64,
}

impl ProcessCursor {
    /// Mint a cursor. `epoch` must be non-empty and free of `:`.
    pub fn new(
        epoch: impl Into<String>,
        reference: ProcessCursorReference,
        position: u64,
        sequence: u64,
    ) -> Result<Self, ProcessCursorError> {
        let epoch = epoch.into();
        if epoch.is_empty() || epoch.contains(':') {
            return Err(ProcessCursorError::Malformed);
        }
        Ok(Self {
            epoch,
            reference,
            position,
            sequence,
        })
    }

    /// Parse a wire cursor, refusing retired versions by name.
    pub fn parse(token: &str) -> Result<Self, ProcessCursorError> {
        let Some((version, rest)) = token.split_once(':') else {
            return Err(ProcessCursorError::Malformed);
        };
        if RETIRED_PROCESS_CURSOR_VERSIONS.contains(&version) {
            return Err(ProcessCursorError::RetiredVersion {
                found: version.to_string(),
            });
        }
        if version != PROCESS_CURSOR_VERSION {
            return Err(ProcessCursorError::Malformed);
        }
        let mut parts = rest.split(':');
        let (Some(epoch), Some(reference), Some(position), Some(sequence), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(ProcessCursorError::Malformed);
        };
        let reference =
            ProcessCursorReference::parse(reference).ok_or(ProcessCursorError::Malformed)?;
        let decimal = |value: &str| {
            (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| value.parse::<u64>().ok())
                .flatten()
                .ok_or(ProcessCursorError::Malformed)
        };
        Self::new(epoch, reference, decimal(position)?, decimal(sequence)?)
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    pub fn reference(&self) -> &ProcessCursorReference {
        &self.reference
    }

    pub fn position(&self) -> u64 {
        self.position
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// The same live position with a later durable high-water mark.
    #[must_use]
    pub fn with_sequence(&self, sequence: u64) -> Self {
        Self {
            sequence,
            ..self.clone()
        }
    }

    /// The same durable high-water mark at a later live position.
    #[must_use]
    pub fn with_position(&self, position: u64) -> Self {
        Self {
            position,
            ..self.clone()
        }
    }
}

impl fmt::Display for ProcessCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{PROCESS_CURSOR_VERSION}:{}:{}:{}:{}",
            self.epoch,
            self.reference.as_str(),
            self.position,
            self.sequence
        )
    }
}

impl Serialize for ProcessCursor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ProcessCursor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let token = String::deserialize(deserializer)?;
        Self::parse(&token).map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for ProcessCursor {
    fn schema_name() -> String {
        "ProcessCursor".to_string()
    }

    fn json_schema(_generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        schemars::schema::SchemaObject {
            instance_type: Some(schemars::schema::InstanceType::String.into()),
            string: Some(Box::new(schemars::schema::StringValidation {
                pattern: Some("^lashpc3:[^:]+:p_[0-9a-f]{32}:[0-9]+:[0-9]+$".to_string()),
                ..Default::default()
            })),
            metadata: Some(Box::new(schemars::schema::Metadata {
                description: Some(
                    "Opaque process cursor: `lashpc3:<epoch>:<process-reference>:<position>:<sequence>`."
                        .to_string(),
                ),
                ..Default::default()
            })),
            ..Default::default()
        }
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(n: u128) -> ProcessId {
        ProcessId::from_minted(0x0000_0000_0000_7000_8000_0000_0000_0000 | n)
    }

    fn reference() -> ProcessCursorReference {
        ProcessCursorReference::for_process(&process(3))
    }

    #[test]
    fn a_cursor_round_trips_with_one_opaque_reference_component() {
        let cursor = ProcessCursor::new("epoch-a", reference(), 7, 11).expect("cursor");
        let wire = cursor.to_string();
        assert_eq!(
            wire,
            "lashpc3:epoch-a:p_00000000000070008000000000000003:7:11"
        );
        assert_eq!(ProcessCursor::parse(&wire), Ok(cursor.clone()));
        assert!(cursor.reference().names(&process(3)));
        assert!(!cursor.reference().names(&process(4)));
        assert_eq!(cursor.reference().process_id(), &process(3));
        let json = serde_json::to_string(&cursor).expect("serialize");
        assert_eq!(
            serde_json::from_str::<ProcessCursor>(&json).expect("decode"),
            cursor
        );
    }

    #[test]
    fn a_retired_cursor_is_refused_naming_its_version() {
        for (retired, wire) in [
            ("lashpc1", "lashpc1:epoch:1:1:process:wire"),
            // A `lashpc2` cursor named a reusable name and its incarnation.
            ("lashpc2", "lashpc2:epoch:r3.702d37:7:11"),
        ] {
            assert_eq!(
                ProcessCursor::parse(wire),
                Err(ProcessCursorError::RetiredVersion {
                    found: retired.to_string()
                })
            );
            assert!(
                serde_json::from_str::<ProcessCursor>(&format!("\"{wire}\""))
                    .expect_err("retired")
                    .to_string()
                    .contains(retired)
            );
        }
    }

    #[test]
    fn malformed_cursors_are_refused() {
        let good = ProcessCursor::new("e", reference(), 1, 2)
            .expect("cursor")
            .to_string();
        for bad in [
            String::new(),
            "invalid".to_string(),
            "lashpc4:e:p_00000000000070008000000000000003:1:2".to_string(),
            good.replace(":1:2", ":1"),
            format!("{good}:9"),
            good.replace(":1:2", ":-1:2"),
            good.replace(":1:2", ":+1:2"),
            "lashpc3::p_00000000000070008000000000000003:1:2".to_string(),
            "lashpc3:e:r1.61:1:2".to_string(),
            "lashpc3:e:p-7:1:2".to_string(),
            "lashpc3:e:p_0000000000000000000000000000003:1:2".to_string(),
            "lashpc3:e:process:1:2".to_string(),
        ] {
            assert_eq!(
                ProcessCursor::parse(&bad),
                Err(ProcessCursorError::Malformed),
                "{bad}"
            );
        }
        assert!(ProcessCursor::new("a:b", reference(), 0, 0).is_err());
    }
}
