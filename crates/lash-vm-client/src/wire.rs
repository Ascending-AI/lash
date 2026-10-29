//! What a parent and its worker say to each other about one kernel run, in
//! the kernel's own JSON encoding.
//!
//! The protocol carries these as opaque payloads; the machine's interface
//! types are not serialisable themselves, so each crosses as the mirror
//! here. Numbers keep their tokens and objects their identities, so nothing
//! is reduced on the way.
//!
//! A document is not one of these: it crosses as its canonical token body
//! (`lash_kernel_doc::IdentifiedDocument::body`), never as JSON.

use lash_protocol_types::engine_fault::PayloadKind;
use std::borrow::Cow;

use lash_kernel_doc::heap::{PageGrant, PageId, PageRef};
use lash_kernel_doc::{Datum, ErrorDatum};

use lash_kernel_state::paged::SessionRoot;
use lash_kernel_vm::{
    Bound, BoundExceeded, EffectModes, EffectRequest, End, Finished, Outcome, Park, Request,
    RunError, Target, WaitId,
};
use lash_vm_broker::kernel::MachinePark;
use lash_vm_protocol::EncodedPayload;
use lash_vm_protocol::page::PageCapability;

use lash_kernel_value::names::FunctionId;
use lash_kernel_value::names::FunctionName;
use lash_kernel_value::names::Name;
use lash_kernel_value::sites::EffectId;
use lash_kernel_value::sites::TaskLabel;
use lash_kernel_value::types::Type;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use {lash_kernel_value::tokens::decode_body, lash_kernel_value::tokens::encode_body};

use crate::PoolError;

/// JSON text as a payload: one MessagePack binary, so the transport bounds
/// it as it bounds every payload, without reading the JSON.
///
/// # Errors
///
/// A payload breach when the bytes cannot be framed.
pub fn wrap(kind: PayloadKind, json: &[u8]) -> Result<EncodedPayload, PoolError> {
    rmp_serde::to_vec(serde_bytes::Bytes::new(json))
        .map(EncodedPayload)
        .map_err(|error| PoolError::payload(kind, error))
}

/// The JSON text a payload of `kind` carries, borrowed from binary or string
/// input. The accepted integer-array form owns its assembled bytes.
///
/// # Errors
///
/// A payload breach when the payload cannot decode to bytes.
pub fn unwrap(kind: PayloadKind, payload: &EncodedPayload) -> Result<Cow<'_, [u8]>, PoolError> {
    // Match ByteBuf's visitor, including its alternate string/sequence forms
    // and error expectation, while borrowing contiguous bytes from the frame.
    struct PayloadBytes;
    impl<'de> serde::de::Visitor<'de> for PayloadBytes {
        type Value = Cow<'de, [u8]>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("byte array")
        }

        fn visit_borrowed_bytes<E: serde::de::Error>(
            self,
            bytes: &'de [u8],
        ) -> Result<Self::Value, E> {
            Ok(Cow::Borrowed(bytes))
        }

        fn visit_borrowed_str<E: serde::de::Error>(self, text: &'de str) -> Result<Self::Value, E> {
            Ok(Cow::Borrowed(text.as_bytes()))
        }

        fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<Self::Value, E> {
            Ok(Cow::Owned(bytes.to_vec()))
        }

        fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Self::Value, E> {
            self.visit_bytes(text.as_bytes())
        }

        fn visit_byte_buf<E: serde::de::Error>(self, bytes: Vec<u8>) -> Result<Self::Value, E> {
            Ok(Cow::Owned(bytes))
        }

        fn visit_string<E: serde::de::Error>(self, text: String) -> Result<Self::Value, E> {
            Ok(Cow::Owned(text.into_bytes()))
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            sequence: A,
        ) -> Result<Self::Value, A::Error> {
            serde_bytes::ByteBuf::deserialize(serde::de::value::SeqAccessDeserializer::new(
                sequence,
            ))
            .map(|bytes| Cow::Owned(bytes.into_vec()))
        }
    }
    let mut decoder = rmp_serde::Deserializer::from_read_ref(&payload.0);
    serde::Deserializer::deserialize_byte_buf(&mut decoder, PayloadBytes)
        .map_err(|error| PoolError::payload(kind, error))
}

/// `value` as a payload of `kind`.
///
/// # Errors
///
/// A payload breach when the value has no JSON encoding.
pub fn encode<T: Serialize>(kind: PayloadKind, value: &T) -> Result<EncodedPayload, PoolError> {
    let json = serde_json::to_vec(value).map_err(|error| PoolError::payload(kind, error))?;
    wrap(kind, &json)
}

/// `value` as a payload of `kind` of at most `bound` bytes, the effect value
/// bound (FIG-6788): a value whose encoding passes it is refused while it
/// is encoded, so the encoding never holds more than `bound` bytes of it.
/// The refusal names the payload's whole size.
///
/// # Errors
///
/// [`WorkerLimit::EffectValue`](lash_protocol_types::worker_limit::WorkerLimit::EffectValue)
/// past the bound, and a payload breach when the value has no JSON
/// encoding.
pub fn encode_within<T: Serialize>(
    kind: PayloadKind,
    value: &T,
    bound: u64,
) -> Result<EncodedPayload, PoolError> {
    let json = capped_json(kind, value, bound)?;
    let size = json.written.saturating_add(bin_header_bytes(json.written));
    if size > bound {
        return Err(
            lash_protocol_types::engine_fault::InfrastructureOutcome::WorkerLimitExceeded {
                limit: lash_protocol_types::worker_limit::WorkerLimit::EffectValue { size, bound },
            }
            .into(),
        );
    }
    wrap(kind, &json.bytes)
}

/// `value`'s JSON encoding, kept up to `bound` bytes and counted whole.
fn capped_json<T: Serialize>(
    kind: PayloadKind,
    value: &T,
    bound: u64,
) -> Result<Capped, PoolError> {
    let mut json = Capped::new(bound);
    serde_json::to_writer(&mut json, value).map_err(|error| PoolError::payload(kind, error))?;
    Ok(json)
}

/// The bytes of the MessagePack binary header [`wrap`] puts before `len`
/// bytes of JSON.
fn bin_header_bytes(len: u64) -> u64 {
    match len {
        0..256 => 2,
        256..65_536 => 3,
        _ => 5,
    }
}

/// Keeps what serialization writes up to `limit` bytes, never reserving
/// past it, and counts the rest without keeping it.
struct Capped {
    bytes: Vec<u8>,
    limit: usize,
    written: u64,
}

impl Capped {
    fn new(limit: u64) -> Self {
        Self {
            bytes: Vec::new(),
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
            written: 0,
        }
    }
}

impl std::io::Write for Capped {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.written = self.written.saturating_add(bytes.len() as u64);
        if self.written > self.limit as u64 {
            return Ok(bytes.len());
        }
        let needed = self.bytes.len() + bytes.len();
        if needed > self.bytes.capacity() {
            let capacity = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(needed)
                .min(self.limit);
            self.bytes.reserve_exact(capacity - self.bytes.len());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The value a payload of `kind` carries.
///
/// # Errors
///
/// A payload breach when the payload is not that value.
pub fn decode<T: DeserializeOwned>(
    kind: PayloadKind,
    payload: &EncodedPayload,
) -> Result<T, PoolError> {
    from_json(kind, &unwrap(kind, payload)?)
}

/// The value `json` encodes. Kernel values bound their own nesting, so the
/// decoder's fixed recursion limit is lifted.
///
/// # Errors
///
/// A payload breach when the text is not that value.
pub fn from_json<T: DeserializeOwned>(kind: PayloadKind, json: &[u8]) -> Result<T, PoolError> {
    let mut decoder = serde_json::Deserializer::from_slice(json);
    decoder.disable_recursion_limit();
    let value = T::deserialize(&mut decoder).map_err(|error| PoolError::payload(kind, error))?;
    decoder
        .end()
        .map_err(|error| PoolError::payload(kind, error))?;
    Ok(value)
}

/// The observations since the previous worker frame. `entered` remains
/// a current-position hint; `ran` is an exact additive delta.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sites {
    pub entered: Vec<lash_kernel_value::sites::EffectLocation>,
    pub ran: lash_kernel_vm::SiteCounts,
    /// The statement path since the previous frame, when the start asked
    /// for it: each task's consecutive starts, as document ordinals.
    #[serde(
        default,
        skip_serializing_if = "lash_kernel_vm::ExecutionPath::is_empty"
    )]
    pub path: lash_kernel_vm::ExecutionPath,
    /// The waits whose failed outcome a `catch` recovered from, carried by
    /// the frame of a park or an end (FIG-6817).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caught: Vec<lash_kernel_vm::WaitId>,
}

/// How a carried session names one of its pages: under a capability the
/// receiver verifies, or as a page of the seal batch the same frame
/// carries, which the parent admits into this activation's staging before
/// it reads the session (FIG-6288).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PageLink {
    Granted(PageCapability),
    Carried([u8; 32]),
}

impl PageLink {
    /// The page this names.
    #[must_use]
    pub fn page(&self) -> [u8; 32] {
        match self {
            Self::Granted(capability) => capability.page,
            Self::Carried(page) => *page,
        }
    }
}

/// A carried session as it crosses (S8): its [`SessionRoot`] in the
/// kernel's token encoding, and a link for each page it names, in its
/// order (heap, bindings, functions). The receiver authorizes every page
/// by its link; a root naming a page with none is refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CarryWire {
    #[serde(with = "serde_bytes")]
    #[schemars(with = "Vec<u8>", extend("x-lash-binary-encoding" = "MessagePack bin"))]
    pub root: Vec<u8>,
    pub pages: Vec<PageLink>,
}

impl CarryWire {
    /// `session`, each of its pages carried by the link `issue` gives it.
    pub fn of(session: &SessionRoot, mut issue: impl FnMut(PageRef) -> PageLink) -> Self {
        Self {
            root: encode_body(session),
            pages: session.pages().into_iter().map(&mut issue).collect(),
        }
    }

    /// The session this carries, each page granted what `grant` makes of
    /// its link.
    ///
    /// # Errors
    ///
    /// A payload breach for a root that does not decode, or links that
    /// are not its pages'.
    pub fn session(
        &self,
        mut grant: impl FnMut(&PageLink) -> Result<PageGrant, PoolError>,
    ) -> Result<SessionRoot, PoolError> {
        let breach = |problem: &str| PoolError::payload(PayloadKind::Start, problem);
        let mut session: SessionRoot = decode_body(&self.root)
            .map_err(|error| PoolError::payload(PayloadKind::Start, error))?;
        let named = session.pages();
        if named.len() != self.pages.len()
            || named
                .iter()
                .zip(&self.pages)
                .any(|(page, link)| page.id.0 != link.page())
        {
            return Err(breach("a carried session's links are not its pages'"));
        }
        let mut grants = self.pages.iter();
        session.grant_pages(|_| {
            grants
                .next()
                .ok_or_else(|| breach("a carried session's links run out"))
                .and_then(&mut grant)
        })?;
        Ok(session)
    }
}

/// How a new run starts ([`lash_kernel_vm::CellStart`], less its owner
/// binding, which the protocol's `Start` carries), and the effects it
/// performs as reads ([`EffectModes`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartWire {
    /// The entry to run; `None` runs the document's `main`.
    pub target: Target,
    pub args: Vec<Datum>,
    /// The carried session a cell opens in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<CarryWire>,
    /// The effects the run performs as reads (`K-EFF-012`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<lash_kernel_value::names::OperationUse>,
    /// The host's read-only bindings a cell's `main` starts with.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host: Vec<(Name, Datum)>,
}

/// What a session cell's end needs to save the functions its carry would
/// drop (`K-SES-004`), beside the dialect's function values the worker
/// holds: the controls its effects declare and what its source wrote of
/// each function it makes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveWire {
    #[serde(default, with = "saved_written")]
    pub saved_written:
        std::collections::BTreeMap<lash_kernel_doc::heap::EntityId, lash_kernel_dialect::Written>,
    #[serde(with = "lash_vocabulary::ordered_pairs")]
    pub controls: std::collections::BTreeMap<
        lash_kernel_value::names::OperationUse,
        std::collections::BTreeSet<lash_kernel_dialect::EffectControl>,
    >,
    /// What the source wrote of each function the cell's document makes,
    /// by the site of its closure ([`lash_kernel_dialect::Lowered::written`]).
    #[serde(with = "lash_kernel_dialect::written_table")]
    pub written:
        std::collections::BTreeMap<lash_kernel_value::sites::Site, lash_kernel_dialect::Written>,
}

/// Heap identities use their family and ordinal on the worker wire. They
/// remain internal kernel types rather than acquiring a general serde form.
mod saved_written {
    use std::collections::BTreeMap;

    use lash_kernel_dialect::Written;
    use lash_kernel_doc::heap::{EntityId, FamilyId};
    use serde::{Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        origins: &BTreeMap<EntityId, Written>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        origins
            .iter()
            .map(|(id, origin)| ((id.family.0, id.ordinal), origin))
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<EntityId, Written>, D::Error> {
        let rows: BTreeMap<(u64, u64), Written> =
            lash_vocabulary::ordered_pairs::deserialize(deserializer)?;
        Ok(rows
            .into_iter()
            .map(|((family, ordinal), origin)| {
                (
                    EntityId {
                        family: FamilyId(family),
                        ordinal,
                    },
                    origin,
                )
            })
            .collect())
    }
}

impl StartWire {
    /// A run of `target` over `args`, opening `session`, performing its
    /// effects in the modes `modes` binds.
    #[must_use]
    pub fn of(
        target: Target,
        args: Vec<Datum>,
        session: Option<CarryWire>,
        modes: &EffectModes,
        host: Vec<(Name, Datum)>,
    ) -> Self {
        Self {
            target,
            args,
            session,
            reads: modes
                .iter()
                .filter_map(|(use_, mode)| {
                    (*mode == lash_kernel_value::package::OperationMode::Read).then_some(*use_)
                })
                .collect(),
            host,
        }
    }

    /// The target this wire runs.
    #[must_use]
    pub fn target(&self) -> Target {
        self.target.clone()
    }

    /// The effect modes this wire carries.
    #[must_use]
    pub fn modes(&self) -> EffectModes {
        self.reads
            .iter()
            .map(|use_| (*use_, lash_kernel_value::package::OperationMode::Read))
            .collect()
    }
}

/// One read-bound `perform`, as the worker asks its parent to answer it
/// (`K-EFF-012`): the effect, its arguments copied out of the run, the
/// site of the `perform` and the authored statement it stands under.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectRead {
    pub use_: lash_kernel_value::names::OperationUse,
    pub args: Vec<Datum>,
    pub site: lash_kernel_value::sites::PositionLabel,
    pub origin: lash_kernel_value::sites::Site,
}

/// The parent's answer to an [`EffectRead`]: the effect's result, or the
/// error the guest raises at the `perform`.
pub type EffectAnswer = Result<Datum, ErrorDatum>;

/// One wait a park requests ([`Request`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestWire {
    Effect {
        wait: WaitId,
        identity: EffectId,
        use_: lash_kernel_value::names::OperationUse,
        args: Vec<Datum>,
        result: Type,
        origin: lash_kernel_value::sites::Site,
    },
}

/// What a park hands the parent ([`Park`]): its task table stays in the
/// machine's heap, and only its counts cross.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParkWire {
    pub requests: Vec<RequestWire>,
    pub cancel_requests: Vec<WaitId>,
    pub outstanding: u64,
    pub live: u64,
}

impl From<Park> for ParkWire {
    fn from(park: Park) -> Self {
        Self {
            requests: park
                .requests
                .into_iter()
                .map(|request| match request {
                    Request::Effect(request) => RequestWire::Effect {
                        wait: request.wait,
                        identity: request.identity,
                        use_: request.use_,
                        args: request.args,
                        result: request.result,
                        origin: request.origin,
                    },
                })
                .collect(),
            cancel_requests: park.cancel_requests,
            outstanding: park.outstanding,
            live: park.live,
        }
    }
}

impl TryFrom<ParkWire> for MachinePark {
    type Error = PoolError;

    fn try_from(wire: ParkWire) -> Result<Self, Self::Error> {
        Ok(Self {
            requests: wire
                .requests
                .into_iter()
                .map(|request| match request {
                    RequestWire::Effect {
                        wait,
                        identity,
                        use_,
                        args,
                        result,
                        origin,
                    } => Ok(Request::Effect(EffectRequest {
                        wait,
                        identity,
                        use_,
                        args,
                        result,
                        origin,
                    })),
                })
                .collect::<Result<_, PoolError>>()?,
            cancel_requests: wire.cancel_requests,
            outstanding: wire.outstanding,
            live: wire.live,
        })
    }
}

/// How a wait ended ([`Outcome`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OutcomeWire {
    Completed(Datum),
    Failed(ErrorDatum),
}

impl From<Outcome> for OutcomeWire {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Completed(value) => Self::Completed(value),
            Outcome::Failed(error) => Self::Failed(error),
        }
    }
}

impl From<OutcomeWire> for Outcome {
    fn from(wire: OutcomeWire) -> Self {
        match wire {
            OutcomeWire::Completed(value) => Self::Completed(value),
            OutcomeWire::Failed(error) => Self::Failed(error),
        }
    }
}

/// Which bound a run passed ([`Bound`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum BoundWire {
    Charge,
    Memory,
    CallDepth,
    LiveTasks,
    RequestsPerPark,
    JoinMembers,
    Guard { function: FunctionId },
    Effects,
    CheckpointBytes,
}

/// How a run ended ([`End`]). A finished cell's carry crosses as the
/// session root it sealed, with a link for each page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum EndWire {
    Finished {
        result: Datum,
        finish: bool,
        carry: Option<CarryWire>,
        not_carried: Vec<(Name, String)>,
    },
    Failed(Datum),
    Uncaught {
        error: Datum,
        site: Option<lash_kernel_value::sites::Site>,
    },
    TasksOutstanding {
        unfinished: Vec<TaskLabel>,
        unobserved: Vec<TaskLabel>,
        sources: Vec<(TaskLabel, lash_kernel_value::sites::PositionLabel)>,
    },
    Deadlock {
        waiting: Vec<TaskLabel>,
    },
    Bound {
        bound: BoundWire,
        limit: u64,
        function: Option<FunctionName>,
        site: Option<lash_kernel_value::sites::Site>,
    },
    Cancelled,
}

impl EndWire {
    /// `end`, its carry's pages carried by the link `issue` gives
    /// each.
    pub fn of(end: End, issue: impl FnMut(PageRef) -> PageLink) -> Self {
        match end {
            End::Finished(finished) => {
                let Finished {
                    result,
                    finish,
                    carry,
                    not_carried,
                } = *finished;
                Self::Finished {
                    result,
                    finish,
                    carry: carry.map(|session| CarryWire::of(&session, issue)),
                    not_carried,
                }
            }
            End::Failed(reason) => Self::Failed(reason),
            End::Error(RunError::Uncaught { error, site }) => Self::Uncaught { error, site },
            End::Error(RunError::TasksOutstanding {
                unfinished,
                unobserved,
                sources,
            }) => Self::TasksOutstanding {
                unfinished,
                unobserved,
                sources,
            },
            End::Error(RunError::Deadlock { waiting }) => Self::Deadlock { waiting },
            End::Error(RunError::Bound(exceeded)) => Self::Bound {
                bound: match exceeded.bound {
                    Bound::Charge => BoundWire::Charge,
                    Bound::Memory => BoundWire::Memory,
                    Bound::CallDepth => BoundWire::CallDepth,
                    Bound::LiveTasks => BoundWire::LiveTasks,
                    Bound::RequestsPerPark => BoundWire::RequestsPerPark,
                    Bound::JoinMembers => BoundWire::JoinMembers,
                    Bound::Guard { function } => BoundWire::Guard { function },
                    Bound::Effects => BoundWire::Effects,
                    Bound::CheckpointBytes => BoundWire::CheckpointBytes,
                },
                limit: exceeded.limit,
                function: exceeded.function,
                site: exceeded.site.map(|site| *site),
            },
            End::Cancelled => Self::Cancelled,
        }
    }

    /// The end this carries, its carry's pages granted what `grant` makes
    /// of each link.
    ///
    /// # Errors
    ///
    /// As [`CarryWire::session`].
    pub fn into_end(
        self,
        grant: impl FnMut(&PageLink) -> Result<PageGrant, PoolError>,
    ) -> Result<End, PoolError> {
        Ok(match self {
            Self::Finished {
                result,
                finish,
                carry,
                not_carried,
            } => End::Finished(Box::new(Finished {
                result,
                finish,
                carry: carry.map(|carry| carry.session(grant)).transpose()?,
                not_carried,
            })),
            Self::Failed(reason) => End::Failed(reason),
            Self::Uncaught { error, site } => End::Error(RunError::Uncaught { error, site }),
            Self::TasksOutstanding {
                unfinished,
                unobserved,
                sources,
            } => End::Error(RunError::TasksOutstanding {
                unfinished,
                unobserved,
                sources,
            }),
            Self::Deadlock { waiting } => End::Error(RunError::Deadlock { waiting }),
            Self::Bound {
                bound,
                limit,
                function,
                site,
            } => End::Error(RunError::Bound(BoundExceeded {
                bound: match bound {
                    BoundWire::Charge => Bound::Charge,
                    BoundWire::Memory => Bound::Memory,
                    BoundWire::CallDepth => Bound::CallDepth,
                    BoundWire::LiveTasks => Bound::LiveTasks,
                    BoundWire::RequestsPerPark => Bound::RequestsPerPark,
                    BoundWire::JoinMembers => Bound::JoinMembers,
                    BoundWire::Guard { function } => Bound::Guard { function },
                    BoundWire::Effects => Bound::Effects,
                    BoundWire::CheckpointBytes => Bound::CheckpointBytes,
                },
                limit,
                function,
                site: site.map(Box::new),
            })),
            Self::Cancelled => End::Cancelled,
        })
    }
}

/// The page a capability names.
#[must_use]
pub fn page_of(capability: &PageCapability) -> PageId {
    PageId(capability.page)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BorrowedPayloadDecodePreservesAcceptance: the MessagePack adapter keeps
    /// the owned decoder's accepted forms and failures, borrowing binary input.
    #[test]
    fn borrowed_payload_decode_preserves_acceptance() {
        let kind = PayloadKind::Outcome;
        let json = r#"{"number":123456789012345678901234567890,"text":"é"}"#.as_bytes();
        let mut cases = vec![wrap(kind, json).unwrap().0];
        for header in [
            vec![0xc5, 0, json.len() as u8],
            vec![0xc6, 0, 0, 0, json.len() as u8],
        ] {
            let mut bytes = header;
            bytes.extend_from_slice(json);
            cases.push(bytes);
        }
        let binaries = cases.len();
        let number = Datum::Number(lash_kernel_doc::NumberToken::new("1.2300e+40").unwrap());
        let mut nested = number;
        for _ in 0..160 {
            nested = Datum::List(vec![nested]);
        }
        let nested_payload = encode(kind, &nested).unwrap();
        assert_eq!(decode::<Datum>(kind, &nested_payload).unwrap(), nested);
        cases.extend([
            rmp_serde::to_vec(&std::str::from_utf8(json).unwrap()).unwrap(),
            rmp_serde::to_vec(&json.to_vec()).unwrap(),
            vec![],
            vec![0xc4],
            vec![0xc4, 2, b'1'],
            vec![0xc1],
            vec![0xc0],
            vec![0x81, 1, 2],
            vec![0x91, 0xcd, 1, 0],
            vec![0xa1, 0xff],
            wrap(kind, b"1 2").unwrap().0,
            wrap(kind, b"{").unwrap().0,
        ]);
        let mut trailing = wrap(kind, json).unwrap().0;
        trailing.push(0xc0);
        cases.push(trailing);
        for (index, bytes) in cases.into_iter().enumerate() {
            let payload = EncodedPayload(bytes);
            let owned = rmp_serde::from_slice::<serde_bytes::ByteBuf>(&payload.0);
            let borrowed = unwrap(kind, &payload);
            assert_eq!(owned.is_ok(), borrowed.is_ok(), "case {index}");
            if let Err(error) = &owned {
                assert_eq!(
                    borrowed.as_ref().unwrap_err(),
                    &PoolError::payload(kind, error),
                    "case {index}"
                );
            }
            if let Ok(owned) = owned {
                let borrowed = borrowed.unwrap();
                let actual_bytes: &[u8] = borrowed.as_ref();
                assert_eq!(actual_bytes, owned.as_ref(), "case {index}");
                let expected = from_json::<serde_json::Value>(kind, &owned);
                let actual = decode::<serde_json::Value>(kind, &payload);
                if index < binaries {
                    assert!(expected.is_ok());
                }
                assert_eq!(actual, expected, "case {index}");
                if index < binaries {
                    assert!(borrowed.as_ptr() >= payload.0.as_ptr());
                    assert!(borrowed.as_ptr_range().end <= payload.0.as_ptr_range().end);
                }
            }
        }
    }

    /// FIG-6788: an outcome past the effect value bound is refused while it
    /// is encoded, naming its whole size, and its encoding never holds more
    /// than the bound; one at the bound is the payload `encode` makes.
    #[test]
    fn an_outcome_past_the_effect_bound_is_refused_while_it_is_encoded() {
        let outcome = OutcomeWire::from(Outcome::Completed(Datum::Text("x".repeat(1 << 20))));
        let whole = encode(PayloadKind::Outcome, &outcome).unwrap();
        let size = whole.0.len() as u64;
        let bound = 4096;
        let json = capped_json(PayloadKind::Outcome, &outcome, bound).unwrap();
        assert!(
            json.bytes.capacity() as u64 <= bound,
            "the encoding held {} bytes past a {bound}-byte bound",
            json.bytes.capacity()
        );
        for bound in [bound, size - 1] {
            let refused = encode_within(PayloadKind::Outcome, &outcome, bound);
            assert!(
                matches!(
                    refused,
                    Err(PoolError::Infrastructure(
                        lash_protocol_types::engine_fault::InfrastructureOutcome::WorkerLimitExceeded {
                            limit: lash_protocol_types::worker_limit::WorkerLimit::EffectValue { size: named, bound: stated },
                        }
                    )) if named == size && stated == bound
                ),
                "{refused:?}"
            );
        }
        assert_eq!(
            encode_within(PayloadKind::Outcome, &outcome, size).unwrap(),
            whole
        );
    }
}
