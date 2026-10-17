//! Validated, SDK-independent W3C trace context shared by requests and telemetry.
use serde::{Deserialize, Serialize};

/// Most list members a W3C `tracestate` may hold.
pub const TRACESTATE_MEMBER_LIMIT: usize = 32;
/// Most characters the canonical `tracestate` may hold.
pub const TRACESTATE_CHAR_LIMIT: usize = 512;

const TRACEPARENT_LEN: usize = 55;

/// A parser's view of host-selected W3C tracestate bounds. Every value is
/// capped at the protocol ceiling, even for a custom implementation.
pub trait TraceStateBudget {
    fn members(&self) -> usize {
        32
    }
    fn chars(&self) -> usize {
        512
    }
    fn key_bytes(&self) -> usize {
        256
    }
    fn tenant_bytes(&self) -> usize {
        241
    }
    fn system_bytes(&self) -> usize {
        14
    }
    fn value_bytes(&self) -> usize {
        256
    }
}
impl<T: TraceStateBudget> TraceStateBudget for &T {
    fn members(&self) -> usize {
        T::members(self)
    }
    fn chars(&self) -> usize {
        T::chars(self)
    }
    fn key_bytes(&self) -> usize {
        T::key_bytes(self)
    }
    fn tenant_bytes(&self) -> usize {
        T::tenant_bytes(self)
    }
    fn system_bytes(&self) -> usize {
        T::system_bytes(self)
    }
    fn value_bytes(&self) -> usize {
        T::value_bytes(self)
    }
}
struct W3cCeilings;
impl TraceStateBudget for W3cCeilings {}

#[derive(Clone, Copy, PartialEq, Eq)]
struct StateLimits {
    members: usize,
    chars: usize,
    key: usize,
    tenant: usize,
    system: usize,
    value: usize,
}
impl StateLimits {
    fn of(limits: &impl TraceStateBudget) -> Self {
        Self {
            members: limits.members().min(32),
            chars: limits.chars().min(512),
            key: limits.key_bytes().min(256),
            tenant: limits.tenant_bytes().min(241),
            system: limits.system_bytes().min(14),
            value: limits.value_bytes().min(256),
        }
    }
    fn standard() -> Self {
        Self::of(&W3cCeilings)
    }
}

/// Why a W3C trace context was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InvalidTraceCarrier {
    #[error("traceparent is not `version-traceid-parentid-flags`")]
    TraceparentShape,
    #[error("traceparent holds a character that is not lowercase hex")]
    TraceparentEncoding,
    #[error("traceparent version `ff` is not a version")]
    TraceparentVersion,
    #[error("trace id is all zeroes")]
    ZeroTraceId,
    #[error("span id is all zeroes")]
    ZeroSpanId,
    #[error("tracestate holds {members} members, over the configured {limit}-member limit")]
    TracestateMembers { members: usize, limit: usize },
    #[error("tracestate holds {chars} characters, over the configured {limit}-character limit")]
    TracestateLength { chars: usize, limit: usize },
    #[error("tracestate {resource} is {actual} bytes, over the configured {limit}-byte limit")]
    TracestateBudget {
        resource: &'static str,
        actual: usize,
        limit: usize,
    },
    #[error("tracestate member {index} is not a valid `key=value`")]
    TracestateMember { index: usize },
    #[error("tracestate member {index} repeats an earlier key")]
    TracestateDuplicateKey { index: usize },
}

/// A nonzero W3C trace id.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct W3cTraceId([u8; 16]);

impl W3cTraceId {
    pub fn from_bytes(bytes: [u8; 16]) -> Result<Self, InvalidTraceCarrier> {
        if bytes == [0; 16] {
            return Err(InvalidTraceCarrier::ZeroTraceId);
        }
        Ok(Self(bytes))
    }

    pub fn to_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl std::fmt::Display for W3cTraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_hex(f, &self.0)
    }
}

impl std::fmt::Debug for W3cTraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "W3cTraceId({self})")
    }
}

/// A nonzero W3C span id.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct W3cSpanId([u8; 8]);

impl W3cSpanId {
    pub fn from_bytes(bytes: [u8; 8]) -> Result<Self, InvalidTraceCarrier> {
        if bytes == [0; 8] {
            return Err(InvalidTraceCarrier::ZeroSpanId);
        }
        Ok(Self(bytes))
    }

    pub fn to_bytes(self) -> [u8; 8] {
        self.0
    }
}

impl std::fmt::Display for W3cSpanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_hex(f, &self.0)
    }
}

/// Serialized as its 16 lowercase hex digits.
impl Serialize for W3cSpanId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for W3cSpanId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let mut bytes = [0_u8; 8];
        if text.len() != 16
            || !text
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(serde::de::Error::custom(
                InvalidTraceCarrier::TraceparentEncoding,
            ));
        }
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
                .map_err(serde::de::Error::custom)?;
        }
        Self::from_bytes(bytes).map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for W3cSpanId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "W3cSpanId".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "A nonzero W3C span id: 16 lowercase hex digits.",
            "type": "string",
            "pattern": "^[0-9a-f]{16}$",
        })
    }
}

impl std::fmt::Debug for W3cSpanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "W3cSpanId({self})")
    }
}

/// The W3C trace-flags byte, kept whole so an unsampled context stays
/// distinguishable from an absent one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct W3cTraceFlags(u8);

impl W3cTraceFlags {
    pub const SAMPLED: u8 = 0x01;

    pub fn from_byte(byte: u8) -> Self {
        Self(byte)
    }

    pub fn to_byte(self) -> u8 {
        self.0
    }

    pub fn is_sampled(self) -> bool {
        self.0 & Self::SAMPLED != 0
    }
}

/// A validated W3C `tracestate`, in the order its members arrived.
///
/// The canonical form is the members joined by `,` with optional whitespace
/// and empty members removed. An empty state is the absent header.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct W3cTraceState(String);

impl W3cTraceState {
    /// Parses one `tracestate` value. A transport that received several
    /// `tracestate` headers joins them with `,` in arrival order first.
    pub fn parse(header: &str) -> Result<Self, InvalidTraceCarrier> {
        Self::parse_with_limits(header, W3cCeilings)
    }
    /// Parse under deployment policy without exceeding the W3C protocol maxima.
    pub fn parse_with_limits(
        header: &str,
        limits: impl TraceStateBudget,
    ) -> Result<Self, InvalidTraceCarrier> {
        let limits = StateLimits::of(&limits);
        let mut keys: Vec<&str> = Vec::new();
        let mut canonical = String::new();
        for member in header.split(',') {
            let member = member.trim_matches([' ', '\t']);
            if member.is_empty() {
                continue;
            }
            let index = keys.len();
            if index == limits.members {
                return Err(InvalidTraceCarrier::TracestateMembers {
                    members: header
                        .split(',')
                        .filter(|member| !member.trim_matches([' ', '\t']).is_empty())
                        .count(),
                    limit: limits.members,
                });
            }
            let Some((key, value)) = member.split_once('=') else {
                return Err(InvalidTraceCarrier::TracestateMember { index });
            };
            let key_parts = match key.split_once('@') {
                Some((tenant, system)) => [
                    ("tenant key", tenant.len(), limits.tenant),
                    ("system key", system.len(), limits.system),
                ],
                None => [("key", key.len(), limits.key), ("key", 0, limits.key)],
            };
            for (resource, actual, limit) in
                key_parts
                    .into_iter()
                    .chain(std::iter::once(("value", value.len(), limits.value)))
            {
                if actual > limit && limits != StateLimits::standard() {
                    return Err(InvalidTraceCarrier::TracestateBudget {
                        resource,
                        actual,
                        limit,
                    });
                }
            }
            if !valid_tracestate_key(key) || !valid_tracestate_value(value) {
                return Err(InvalidTraceCarrier::TracestateMember { index });
            }
            if keys.contains(&key) {
                return Err(InvalidTraceCarrier::TracestateDuplicateKey { index });
            }
            keys.push(key);
            if !canonical.is_empty() {
                canonical.push(',');
            }
            canonical.push_str(member);
        }
        if canonical.len() > limits.chars {
            return Err(InvalidTraceCarrier::TracestateLength {
                chars: canonical.len(),
                limit: limits.chars,
            });
        }
        Ok(Self(canonical))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The `(key, value)` members in order.
    pub fn members(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .split(',')
            .filter_map(|member| member.split_once('='))
    }
}

/// One W3C trace context: the identity of a span some operation can name as
/// its parent or link.
///
/// A carrier never claims its span is local. A span is local only while its
/// SDK handle is alive in this process; every carrier read from transport or
/// from storage is a remote span context.
///
/// The serialized form is the W3C text itself (`traceparent`, and `tracestate`
/// when it is not empty). Decoding validates it: invalid stored data is a
/// decode error, never a reason to mint replacement ancestry. Baggage is not
/// carried.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "TraceCarrierWire", into = "TraceCarrierWire")]
pub struct TraceCarrier {
    trace_id: W3cTraceId,
    span_id: W3cSpanId,
    flags: W3cTraceFlags,
    tracestate: W3cTraceState,
}

impl TraceCarrier {
    pub fn new(
        trace_id: W3cTraceId,
        span_id: W3cSpanId,
        flags: W3cTraceFlags,
        tracestate: W3cTraceState,
    ) -> Self {
        Self {
            trace_id,
            span_id,
            flags,
            tracestate,
        }
    }

    /// Strict parse of an explicitly supplied context: any invalid field is
    /// refused with its reason.
    pub fn parse_w3c(
        traceparent: &str,
        tracestate: Option<&str>,
    ) -> Result<Self, InvalidTraceCarrier> {
        Self::parse_w3c_with(traceparent, tracestate, &W3cCeilings)
    }

    /// [`parse_w3c`](Self::parse_w3c) under a host's narrower `tracestate`
    /// admission: a `tracestate` over `limits` is refused with its reason.
    pub fn parse_w3c_with(
        traceparent: &str,
        tracestate: Option<&str>,
        limits: &impl TraceStateBudget,
    ) -> Result<Self, InvalidTraceCarrier> {
        let (trace_id, span_id, flags) = parse_traceparent(traceparent)?;
        let tracestate = match tracestate {
            Some(tracestate) => W3cTraceState::parse_with_limits(tracestate, limits)?,
            None => W3cTraceState::default(),
        };
        Ok(Self::new(trace_id, span_id, flags, tracestate))
    }

    /// Tolerant extraction from transport headers: an absent or invalid
    /// `traceparent` is no context, and an invalid `tracestate` is dropped
    /// while a valid `traceparent` survives. A malformed telemetry header
    /// never refuses the request that carried it.
    pub fn extract_w3c(traceparent: Option<&str>, tracestate: Option<&str>) -> Option<Self> {
        let (trace_id, span_id, flags) = parse_traceparent(traceparent?).ok()?;
        let tracestate = tracestate
            .and_then(|tracestate| W3cTraceState::parse(tracestate).ok())
            .unwrap_or_default();
        Some(Self::new(trace_id, span_id, flags, tracestate))
    }

    pub fn trace_id(&self) -> W3cTraceId {
        self.trace_id
    }

    pub fn span_id(&self) -> W3cSpanId {
        self.span_id
    }

    pub fn flags(&self) -> W3cTraceFlags {
        self.flags
    }

    pub fn tracestate(&self) -> &W3cTraceState {
        &self.tracestate
    }

    /// The version-00 `traceparent` header value.
    pub fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-{:02x}",
            self.trace_id,
            self.span_id,
            self.flags.to_byte()
        )
    }

    /// Whether both carriers name the same span, whatever their flags or
    /// state.
    pub fn same_span(&self, other: &Self) -> bool {
        self.trace_id == other.trace_id && self.span_id == other.span_id
    }
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "TraceCarrier")]
struct TraceCarrierWire {
    /// W3C `traceparent`, version 00.
    traceparent: String,
    /// W3C `tracestate`, absent when empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tracestate: Option<String>,
}

impl TryFrom<TraceCarrierWire> for TraceCarrier {
    type Error = InvalidTraceCarrier;

    fn try_from(wire: TraceCarrierWire) -> Result<Self, Self::Error> {
        Self::parse_w3c(&wire.traceparent, wire.tracestate.as_deref())
    }
}

impl From<TraceCarrier> for TraceCarrierWire {
    fn from(carrier: TraceCarrier) -> Self {
        Self {
            traceparent: carrier.traceparent(),
            tracestate: (!carrier.tracestate.is_empty()).then_some(carrier.tracestate.0),
        }
    }
}

impl schemars::JsonSchema for TraceCarrier {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        TraceCarrierWire::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        TraceCarrierWire::json_schema(generator)
    }
}

fn parse_traceparent(
    header: &str,
) -> Result<(W3cTraceId, W3cSpanId, W3cTraceFlags), InvalidTraceCarrier> {
    let header = header.trim_matches([' ', '\t']);
    let bytes = header.as_bytes();
    if bytes.len() < TRACEPARENT_LEN || bytes[2] != b'-' || bytes[35] != b'-' || bytes[52] != b'-' {
        return Err(InvalidTraceCarrier::TraceparentShape);
    }
    let mut version = [0_u8; 1];
    decode_hex(&bytes[0..2], &mut version)?;
    match version[0] {
        0xff => return Err(InvalidTraceCarrier::TraceparentVersion),
        // Version 00 is exactly four fields.
        0 if bytes.len() != TRACEPARENT_LEN => return Err(InvalidTraceCarrier::TraceparentShape),
        // A later version may append fields after another delimiter.
        _ if bytes.len() > TRACEPARENT_LEN && bytes[TRACEPARENT_LEN] != b'-' => {
            return Err(InvalidTraceCarrier::TraceparentShape);
        }
        _ => {}
    }
    let mut trace_id = [0_u8; 16];
    decode_hex(&bytes[3..35], &mut trace_id)?;
    let mut span_id = [0_u8; 8];
    decode_hex(&bytes[36..52], &mut span_id)?;
    let mut flags = [0_u8; 1];
    decode_hex(&bytes[53..55], &mut flags)?;
    Ok((
        W3cTraceId::from_bytes(trace_id)?,
        W3cSpanId::from_bytes(span_id)?,
        W3cTraceFlags::from_byte(flags[0]),
    ))
}

fn decode_hex(text: &[u8], out: &mut [u8]) -> Result<(), InvalidTraceCarrier> {
    let (pairs, _) = text.as_chunks::<2>();
    for (byte, [high, low]) in out.iter_mut().zip(pairs) {
        *byte = (hex_digit(*high)? << 4) | hex_digit(*low)?;
    }
    Ok(())
}

fn hex_digit(digit: u8) -> Result<u8, InvalidTraceCarrier> {
    match digit {
        b'0'..=b'9' => Ok(digit - b'0'),
        b'a'..=b'f' => Ok(digit - b'a' + 10),
        _ => Err(InvalidTraceCarrier::TraceparentEncoding),
    }
}

fn write_hex(f: &mut std::fmt::Formatter<'_>, bytes: &[u8]) -> std::fmt::Result {
    for byte in bytes {
        write!(f, "{byte:02x}")?;
    }
    Ok(())
}

fn tracestate_key_char(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'*' | b'/')
}

fn tracestate_key_part(part: &str, limit: usize, digit_start: bool) -> bool {
    let bytes = part.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    let first_ok = first.is_ascii_lowercase() || (digit_start && first.is_ascii_digit());
    first_ok && bytes.len() <= limit && bytes.iter().copied().all(tracestate_key_char)
}

fn valid_tracestate_key(key: &str) -> bool {
    match key.split_once('@') {
        Some((tenant, system)) => {
            tracestate_key_part(tenant, 241, true) && tracestate_key_part(system, 14, false)
        }
        None => tracestate_key_part(key, 256, true),
    }
}

fn valid_tracestate_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 256
        && bytes.last() != Some(&b' ')
        && bytes
            .iter()
            .all(|byte| matches!(byte, 0x20..=0x7e) && !matches!(byte, b',' | b'='))
}
