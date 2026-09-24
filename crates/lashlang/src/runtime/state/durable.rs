//! A state's durable form, partitioned by root for incremental persistence.
//!
//! A host that persists a session after every cell cannot afford to rewrite
//! the whole state each time, so the durable form is a small header plus one
//! fragment per root binding, and a capture re-encodes only the fragments that
//! changed since the capture it is diffed against (FIG-1195, FIG-1257).
//!
//! The fragments carry the runtime roots and the heap, not the host view:
//! everything a later cell can observe survives a reload — a `Map`, a `Date`,
//! a `URL`, one object named by two bindings, an object's property order —
//! because the reload is the heap the live run had (FIG-3605, FIG-3606).
//!
//! * The header is the format version, for a heap-backed state the heap's
//!   counters, and the names of the globals a cell boundary dropped for
//!   holding a function. It changes with every allocation, so it is always
//!   written.
//! * A fragment is one root's value and the heap objects that root carries
//!   under the durable partition (`Heap::durable_partition`): each live object
//!   exactly once, owned by the first root, in name order, whose walk reaches
//!   it. A reference to an object another root carries is an id like any other.
//!
//! Change detection reads the heap's write stamps rather than re-encoding: a
//! fragment is rewritten when its root's value differs, when its set of
//! carried objects differs, or when one of those objects was written since the
//! baseline. That is sound under reference semantics, where a write through
//! any alias changes an object without assigning the root that carries it.
//!
//! A reader rebuilds the state from the header and every fragment, validates
//! it as a whole snapshot, and then re-encodes it: the header and every
//! fragment must come back byte-for-byte, so a wire whose partition, order or
//! encoding differs from the one this writer produces is refused rather than
//! normalized.

use std::collections::BTreeMap;

use super::*;
use crate::runtime::heap::DurablePartition;

/// A state's durable form as a capture produced it: see the module docs.
#[derive(Clone, Debug)]
pub struct DurableParts {
    /// Canonical bytes of the header: the format version and heap counters.
    pub header: Vec<u8>,
    /// Every binding the state holds, by name.
    pub fragments: BTreeMap<String, DurableFragment>,
    /// What this capture wrote, for the next capture to diff against.
    pub baseline: DurableBaseline,
}

/// One root's fragment in a capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DurableFragment {
    /// The fragment is byte-identical to the one the baseline recorded, so its
    /// previously persisted body still stands.
    Unchanged,
    /// The fragment's canonical bytes.
    Changed(Vec<u8>),
}

/// What a capture wrote, reduced to what the next capture must compare.
///
/// It stands for a set of fragment bodies the caller persisted; the caller
/// keeps the two together, and diffing against the default (empty) baseline
/// writes every fragment.
#[derive(Clone, Debug, Default)]
pub struct DurableBaseline {
    roots: BTreeMap<String, RootFingerprint>,
}

#[derive(Clone, Debug)]
struct RootFingerprint {
    value: Value,
    /// The carried objects, ascending by id, each with its write stamp.
    objects: Vec<(HeapId, u64)>,
}

impl RootFingerprint {
    fn matches(&self, other: &Self) -> bool {
        self.objects == other.objects && durably_identical(&self.value, &other.value)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CanonicalDurableHeader {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    heap: Option<CanonicalHeapCounters>,
    /// [`State::expired_functions`], strictly sorted; absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    expired_functions: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CanonicalHeapCounters {
    reference_semantics: bool,
    next_id: u64,
    allocation_counter: u64,
    live_logical_bytes: u64,
    size_schedule_version: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CanonicalFragment {
    value: CanonicalValue,
    objects: Vec<CanonicalHeapEntry>,
}

impl State {
    /// Every binding the state holds, in the order the owning record holds
    /// them. Existence is decided here, never by the host view, which omits
    /// bindings it cannot carry (ADR 0076).
    pub fn binding_names(&self) -> impl Iterator<Item = &str> {
        let record = match &self.mode {
            StateMode::Plain(globals) => globals.as_ref(),
            StateMode::HeapBacked(backed) => &backed.runtime_globals,
        };
        record.keys()
    }

    /// Captures the durable form, re-encoding only the fragments that differ
    /// from `since`.
    pub fn durable_parts(
        &self,
        since: &DurableBaseline,
    ) -> Result<DurableParts, ContinuationError> {
        let (record, heap) = match &self.mode {
            StateMode::Plain(globals) => (globals.as_ref(), None),
            StateMode::HeapBacked(backed) => (&backed.runtime_globals, Some(&backed.heap)),
        };
        let mut roots = record.iter().collect::<Vec<_>>();
        roots.sort_unstable_by_key(|(name, _)| *name);

        let partition = heap
            .map(|heap| {
                heap.durable_partition(roots.iter().map(|(_, value)| *value))
                    .map_err(|reason| ContinuationError::UnserializableValue {
                        location: format!("snapshot heap: {reason}"),
                        variant: "shared heap object",
                    })
            })
            .transpose()?;
        let header = CanonicalDurableHeader {
            version: LASHLANG_SNAPSHOT_VERSION,
            heap: heap
                .zip(partition.as_ref())
                .map(|(heap, partition)| CanonicalHeapCounters {
                    reference_semantics: partition.reference_semantics,
                    next_id: heap.next_id,
                    allocation_counter: heap.allocations(),
                    live_logical_bytes: partition.live_logical_bytes,
                    size_schedule_version: heap.schedule_version(),
                }),
            expired_functions: self.expired_functions.iter().cloned().collect(),
        };

        let mut fragments = BTreeMap::new();
        let mut baseline = DurableBaseline::default();
        for (index, (name, value)) in roots.into_iter().enumerate() {
            let carried = partition
                .as_ref()
                .map_or(&[][..], |partition: &DurablePartition| {
                    partition.owned[index].as_slice()
                });
            let fingerprint = RootFingerprint {
                value: value.clone(),
                objects: heap.map_or_else(Vec::new, |heap| {
                    carried.iter().map(|id| (*id, heap.revision(*id))).collect()
                }),
            };
            let unchanged = since
                .roots
                .get(name)
                .is_some_and(|prior| prior.matches(&fingerprint));
            let fragment = if unchanged {
                DurableFragment::Unchanged
            } else {
                DurableFragment::Changed(encode_fragment(name, value, heap, carried)?)
            };
            fragments.insert(name.to_string(), fragment);
            baseline.roots.insert(name.to_string(), fingerprint);
        }
        Ok(DurableParts {
            header: encode_canonical(&header, "snapshot header")?,
            fragments,
            baseline,
        })
    }

    /// Rebuilds a state from a header and every fragment, refusing anything
    /// this writer would not have produced byte-for-byte. Returns the state
    /// and the baseline the fragments stand for, so the next capture diffs
    /// against exactly what was read.
    pub fn from_durable_parts<'a>(
        header: &[u8],
        fragments: impl IntoIterator<Item = (&'a str, &'a [u8])>,
    ) -> Result<(Self, DurableBaseline), SnapshotDecodeError> {
        let found = probe_header_version(header)?;
        if found != LASHLANG_SNAPSHOT_VERSION {
            return Err(SnapshotDecodeError::VersionMismatch {
                expected: LASHLANG_SNAPSHOT_VERSION,
                found,
            });
        }
        let decoded_header: CanonicalDurableHeader = decode_canonical(header, "snapshot header")?;
        let fragments = fragments.into_iter().collect::<BTreeMap<_, _>>();
        let mut roots = Vec::with_capacity(fragments.len());
        let mut objects = Vec::new();
        for (name, bytes) in &fragments {
            let location = child_location("roots", name);
            let fragment: CanonicalFragment = decode_canonical(bytes, &location)?;
            roots.push(CanonicalBinding {
                name: (*name).to_string(),
                value: fragment.value,
            });
            objects.extend(fragment.objects);
        }
        // Each fragment lists its objects in id order; the whole snapshot
        // lists them all in id order. A duplicate id survives the sort and is
        // refused by the heap's strict ordering check.
        objects.sort_by_key(|entry| entry.id);
        let whole = match decoded_header.heap {
            None => {
                if !objects.is_empty() {
                    return Err(SnapshotDecodeError::InvalidEncoding(
                        "a heapless snapshot's fragments cannot carry heap objects".to_string(),
                    ));
                }
                CanonicalSnapshot {
                    version: decoded_header.version,
                    globals: Some(roots),
                    heap: None,
                    expired_functions: decoded_header.expired_functions,
                }
            }
            Some(counters) => CanonicalSnapshot {
                version: decoded_header.version,
                globals: None,
                heap: Some(CanonicalHeap {
                    reference_semantics: counters.reference_semantics,
                    next_id: counters.next_id,
                    allocation_counter: counters.allocation_counter,
                    live_logical_bytes: counters.live_logical_bytes,
                    size_schedule_version: counters.size_schedule_version,
                    roots,
                    objects,
                }),
                expired_functions: decoded_header.expired_functions,
            },
        };
        let state = State::from_snapshot(Snapshot::try_from(whole)?);

        // The fixed point: this writer, run over what was read, must produce
        // exactly the bytes it was given. That one comparison refuses a
        // non-canonical scalar, a reordered or duplicated key, an object
        // carried by the wrong root, and counters that disagree with the heap.
        let parts = state
            .durable_parts(&DurableBaseline::default())
            .map_err(|error| SnapshotDecodeError::InvalidEncoding(error.to_string()))?;
        if parts.header != header {
            return Err(non_fixed_point("snapshot header"));
        }
        if parts.fragments.len() != fragments.len() {
            return Err(non_fixed_point("roots"));
        }
        for (name, bytes) in &fragments {
            match parts.fragments.get(*name) {
                Some(DurableFragment::Changed(encoded)) if encoded.as_slice() == *bytes => {}
                _ => return Err(non_fixed_point(&child_location("roots", name))),
            }
        }
        Ok((state, parts.baseline))
    }
}

fn encode_fragment(
    name: &str,
    value: &Value,
    heap: Option<&Heap>,
    carried: &[HeapId],
) -> Result<Vec<u8>, ContinuationError> {
    let location = child_location("roots", name);
    let value = match heap {
        Some(_) => CanonicalValue::from_runtime(value, &location, 0)?,
        None => CanonicalValue::from_heapless_runtime(value, &location, 0)?,
    };
    let objects = match heap {
        Some(heap) => carried
            .iter()
            .map(|id| {
                let object = heap
                    .get(*id)
                    .map_err(|_| ContinuationError::UnserializableValue {
                        location: format!("{location}.objects"),
                        variant: "dangling heap reference",
                    })?;
                Ok(CanonicalHeapEntry {
                    id: *id,
                    object: CanonicalHeapObject::from_runtime(object, *id)?,
                })
            })
            .collect::<Result<_, ContinuationError>>()?,
        None => Vec::new(),
    };
    encode_canonical(&CanonicalFragment { value, objects }, &location)
}

fn encode_canonical(wire: &impl Serialize, location: &str) -> Result<Vec<u8>, ContinuationError> {
    rmp_serde::to_vec_named(wire).map_err(|_| ContinuationError::UnserializableValue {
        location: location.to_string(),
        variant: "canonical encoding",
    })
}

/// Deserializes one part after the shared structural pass has bounded its
/// nesting, so an over-deep wire is refused before serde recurses into it.
/// Order and encoding are left to the fixed point, which states them once.
fn decode_canonical<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    location: &str,
) -> Result<T, SnapshotDecodeError> {
    validate_canonical_messagepack_structure(
        bytes,
        location,
        MAX_SNAPSHOT_MESSAGEPACK_DEPTH,
        |_| CanonicalMapOrder::Unordered,
        |_| false,
    )?;
    rmp_serde::from_slice(bytes)
        .map_err(|error| SnapshotDecodeError::InvalidEncoding(format!("at `{location}`: {error}")))
}

/// Reads the header's version before anything else, so a header from another
/// format version is refused as a version boundary rather than as a shape it
/// happens not to match.
fn probe_header_version(bytes: &[u8]) -> Result<u32, SnapshotDecodeError> {
    let mut cursor = 0;
    let fields = take_map_length(bytes, &mut cursor, "snapshot header", "snapshot header")?;
    if fields == 0 {
        return Err(non_canonical(
            "snapshot header",
            "snapshot header must begin with its version",
        ));
    }
    expect_key(bytes, &mut cursor, "version", "snapshot header")?;
    let marker = take_byte(bytes, &mut cursor)?;
    let version = take_canonical_integer(bytes, &mut cursor, "snapshot header.version", marker)?;
    u32::try_from(version).map_err(|_| {
        non_canonical(
            "snapshot header.version",
            "snapshot version must be a non-negative 32-bit integer",
        )
    })
}

fn non_fixed_point(location: &str) -> SnapshotDecodeError {
    SnapshotDecodeError::NonCanonicalEncoding {
        location: location.to_string(),
        reason: "wire is not a byte-for-byte canonical fixed point".to_string(),
    }
}

/// Whether two root values encode to the same bytes, without encoding them:
/// numbers by their canonical bits (so `-0` and `+0` differ), records in
/// property order, projections by the identity the wire carries.
fn durably_identical(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => {
            canonical_bits(*left) == canonical_bits(*right)
        }
        (Value::Tuple(left), Value::Tuple(right)) | (Value::List(left), Value::List(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right.iter())
                    .all(|(left, right)| durably_identical(left, right))
        }
        (Value::Record(left), Value::Record(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right.iter())
                    .all(|((left_name, left), (right_name, right))| {
                        left_name == right_name && durably_identical(left, right)
                    })
        }
        (Value::Projected(left), Value::Projected(right)) => {
            left.name() == right.name()
                && left.value_type_name() == right.value_type_name()
                && left.projection_ref() == right.projection_ref()
        }
        (Value::Null, Value::Null) | (Value::Undefined, Value::Undefined) => true,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Image(left), Value::Image(right)) => left == right,
        (Value::Resource(left), Value::Resource(right)) => left == right,
        (Value::Ref(left), Value::Ref(right)) => left == right,
        _ => false,
    }
}

fn canonical_bits(value: f64) -> u64 {
    if value.is_nan() {
        CANONICAL_NAN_BITS
    } else {
        value.to_bits()
    }
}

#[cfg(test)]
#[path = "durable_tests.rs"]
mod tests;
