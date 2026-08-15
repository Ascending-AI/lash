use super::*;

/// Two heaps are equal when they hold the same live objects under the same IDs
/// and the same meters.
///
/// Storage layout — which slot an object occupies, which slots are vacant, and
/// the free list — is a private allocation detail that a decode/encode round
/// trip legitimately compacts, so it is deliberately excluded. Including it made
/// `decode(encode(state)) == state` fail for any program that ever allocated a
/// temporary.
impl PartialEq for Heap {
    fn eq(&self, other: &Self) -> bool {
        self.next_id == other.next_id
            && self.allocations == other.allocations
            && self.live_logical_bytes == other.live_logical_bytes
            && self.schedule_version == other.schedule_version
            && self.id_to_slot.len() == other.id_to_slot.len()
            && self.objects_in_id_order().eq(other.objects_in_id_order())
    }
}

pub(super) fn compound_identity(value: &Value) -> Option<(u8, usize)> {
    match value {
        Value::Tuple(values) => Some((0, values.identity())),
        Value::List(values) => Some((1, values.identity())),
        Value::Record(record) => Some((2, std::sync::Arc::as_ptr(record) as usize)),
        _ => None,
    }
}

impl HeapObject {
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            Self::Tuple(_) => "tuple",
            Self::List(_) => "list",
            Self::Record(_) => "record",
            Self::Closure { .. } => "function",
            Self::RegExp(_) => "RegExp",
            Self::RegExpMatch(_) => "RegExp match array",
            Self::Map(_) => "Map",
            Self::Set(_) => "Set",
            Self::Date(_) => "Date",
            Self::Error(error) => error.kind.name(),
            Self::Url(_) => "URL",
            Self::UrlSearchParams(_) => "URLSearchParams",
        }
    }

    pub(crate) fn logical_bytes(&self) -> u64 {
        let payload = match self {
            Self::Tuple(values) | Self::List(values) => values
                .iter()
                .map(value_logical_bytes)
                .fold(0_u64, u64::saturating_add),
            Self::Record(record) => record.iter().fold(0_u64, |total, (name, value)| {
                total
                    .saturating_add(RECORD_FIELD_BYTES)
                    .saturating_add(name.len() as u64)
                    .saturating_add(value_logical_bytes(value))
            }),
            Self::Closure { captures, .. } => 4_u64.saturating_add(
                captures
                    .iter()
                    .map(value_logical_bytes)
                    .fold(0_u64, u64::saturating_add),
            ),
            Self::RegExp(regexp) => (regexp.pattern.len() as u64)
                .saturating_add(regexp.flags.len() as u64)
                .saturating_add(VALUE_SLOT_BYTES.saturating_mul(3))
                .saturating_add(8),
            Self::RegExpMatch(result) => result
                .items
                .iter()
                .chain([&result.index, &result.input, &result.groups])
                .map(value_logical_bytes)
                .fold(RECORD_FIELD_BYTES.saturating_mul(3), u64::saturating_add),
            Self::Map(map) => map.entries.iter().fold(0_u64, |total, (key, value)| {
                total
                    .saturating_add(COLLECTION_ENTRY_BYTES)
                    .saturating_add(value_logical_bytes(key))
                    .saturating_add(value_logical_bytes(value))
            }),
            Self::Set(set) => set.values.iter().fold(0_u64, |total, value| {
                total
                    .saturating_add(COLLECTION_ENTRY_BYTES)
                    .saturating_add(value_logical_bytes(value))
            }),
            Self::Date(_) => VALUE_SLOT_BYTES.saturating_add(8),
            Self::Error(error) => (error.message.len() as u64)
                .saturating_add(VALUE_SLOT_BYTES)
                .saturating_add(error.cause.as_ref().map_or(0, value_logical_bytes))
                .saturating_add(error.errors.as_ref().map_or(0, value_logical_bytes)),
            Self::Url(url) => (url.href.len() as u64).saturating_add(VALUE_SLOT_BYTES),
            Self::UrlSearchParams(params) => {
                params.entries.iter().fold(0_u64, |total, (name, value)| {
                    total
                        .saturating_add(COLLECTION_ENTRY_BYTES)
                        .saturating_add(name.len() as u64)
                        .saturating_add(value.len() as u64)
                })
            }
        };
        OBJECT_HEADER_BYTES.saturating_add(payload)
    }

    /// The single source of truth for child discovery.
    ///
    /// Every consumer — allocation bookkeeping, reverse parent edges, mark and
    /// sweep, wire validation, and root traversal — resolves children through
    /// this one recursive enumerator, so no caller can accidentally see a
    /// shallower answer than another. Members are normally scalars or
    /// references (`Heap::from_wire` rejects anything else, and every in-process
    /// insertion path imports compounds into their own objects), but the
    /// enumerator still descends into inline compounds so a future member shape
    /// cannot silently hide a reference.
    pub(crate) fn child_refs(&self) -> Vec<HeapId> {
        let mut refs = Vec::new();
        for value in self.values() {
            collect_value_refs(value, &mut refs);
        }
        refs
    }

    pub(super) fn values(&self) -> Box<dyn Iterator<Item = &Value> + '_> {
        match self {
            Self::Tuple(values) | Self::List(values) => Box::new(values.iter()),
            Self::Record(record) => Box::new(record.values()),
            Self::Closure { captures, .. } => Box::new(captures.iter()),
            Self::RegExp(_) | Self::Date(_) | Self::UrlSearchParams(_) => {
                Box::new(std::iter::empty())
            }
            Self::RegExpMatch(result) => {
                Box::new(
                    result
                        .items
                        .iter()
                        .chain([&result.index, &result.input, &result.groups]),
                )
            }
            Self::Map(map) => Box::new(map.entries.iter().flat_map(|(key, value)| [key, value])),
            Self::Set(set) => Box::new(set.values.iter()),
            Self::Error(error) => Box::new(error.cause.iter().chain(error.errors.iter())),
            Self::Url(url) => Box::new(std::iter::once(&url.search_params)),
        }
    }
}
