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

use std::time::Duration;

use lash_kernel_doc::graph_tokens::{decode_body, encode_body};
use lash_kernel_doc::heap::{PageGrant, PageId, PageRef};
use lash_kernel_doc::{
    Datum, EffectIdentity, EffectName, ErrorDatum, FunctionId, FunctionName, Handle, Name,
    TaskIdentity, Type,
};
use lash_kernel_state::paged::SessionRoot;
use lash_kernel_vm::{
    Bound, BoundExceeded, EffectModes, EffectRequest, End, Finished, Outcome, Park, Request,
    RunError, SleepRequest, Target, WaitId,
};
use lash_vm_broker::kernel::MachinePark;
use lash_vm_protocol::page::PageCapability;
use lash_vm_protocol::{EncodedPayload, PayloadKind};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

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

/// The JSON text a payload of `kind` carries.
///
/// # Errors
///
/// A payload breach when the payload is not one binary.
pub fn unwrap(kind: PayloadKind, payload: &EncodedPayload) -> Result<Vec<u8>, PoolError> {
    rmp_serde::from_slice::<serde_bytes::ByteBuf>(&payload.0)
        .map(serde_bytes::ByteBuf::into_vec)
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
    pub entered: Vec<EffectIdentity>,
    pub ran: lash_kernel_vm::RanDelta,
    /// The statement path since the previous frame, when the start asked
    /// for it: each task's consecutive starts, as document ordinals.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: OrdinalPath,
}

/// A statement path ([`lash_kernel_vm::StatementPath`]) whose sites are
/// [`lash_kernel_doc::Document::site_ordinals`] ordinals.
pub type OrdinalPath = Vec<(lash_kernel_doc::TaskIdentity, Vec<u32>)>;

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
            pages: session_pages(session).into_iter().map(&mut issue).collect(),
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
        let named = session_pages(&session);
        if named.len() != self.pages.len()
            || named
                .iter()
                .zip(&self.pages)
                .any(|(page, link)| page.id.0 != link.page())
        {
            return Err(breach("a carried session's links are not its pages'"));
        }
        let mut grants = self.pages.iter();
        let mut next = || -> Result<PageGrant, PoolError> {
            grants
                .next()
                .ok_or_else(|| breach("a carried session's links run out"))
                .and_then(&mut grant)
        };
        session.heap.grant = next()?;
        if let Some(page) = session.bindings.page_mut() {
            page.grant = next()?;
        }
        if let Some(page) = session.functions.page_mut() {
            page.grant = next()?;
        }
        Ok(session)
    }
}

/// The pages `session` names, in its order.
fn session_pages(session: &SessionRoot) -> Vec<PageRef> {
    let mut pages = vec![session.heap];
    pages.extend(session.bindings.page());
    pages.extend(session.functions.page());
    pages
}

/// How a new run starts ([`lash_kernel_vm::CellStart`], less its owner
/// binding, which the protocol's `Start` carries), and the effects it
/// performs as reads ([`EffectModes`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartWire {
    /// The entry to run; `None` runs the document's `main`.
    pub entry: Option<Name>,
    pub args: Vec<Datum>,
    /// The carried session a cell opens in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<CarryWire>,
    /// The effects the run performs as reads (`K-EFF-012`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<EffectName>,
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
    pub controls: std::collections::BTreeMap<
        EffectName,
        std::collections::BTreeSet<lash_kernel_dialect::EffectControl>,
    >,
    /// What the source wrote of each function the cell's document makes,
    /// by the site of its closure ([`lash_kernel_dialect::Lowered::written`]).
    #[serde(with = "lash_kernel_dialect::written_table")]
    pub written: std::collections::BTreeMap<lash_kernel_doc::Site, lash_kernel_dialect::Written>,
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
            entry: match target {
                Target::Main => None,
                Target::Entry(name) => Some(name),
            },
            args,
            session,
            reads: modes.reads().iter().cloned().collect(),
            host,
        }
    }

    /// The target this wire runs.
    #[must_use]
    pub fn target(&self) -> Target {
        self.entry.clone().map_or(Target::Main, Target::Entry)
    }

    /// The effect modes this wire carries.
    #[must_use]
    pub fn modes(&self) -> EffectModes {
        EffectModes::reading(self.reads.clone())
    }
}

/// One read-bound `perform`, as the worker asks its parent to answer it
/// (`K-EFF-012`): the effect, its arguments copied out of the run, and the
/// site of the `perform`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectRead {
    pub effect: EffectName,
    pub args: Vec<Datum>,
    pub site: lash_kernel_doc::Site,
}

/// The parent's answer to an [`EffectRead`]: the effect's result, or the
/// error the guest raises at the `perform`.
pub type EffectAnswer = Result<Datum, ErrorDatum>;

/// Canonical nanoseconds within one second, checked at payload admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct SubsecondNanos(u32);

impl TryFrom<u32> for SubsecondNanos {
    type Error = &'static str;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if value < 1_000_000_000 {
            Ok(Self(value))
        } else {
            Err("sleep nanoseconds must be below one second")
        }
    }
}

impl From<SubsecondNanos> for u32 {
    fn from(value: SubsecondNanos) -> Self {
        value.0
    }
}

/// One wait a park requests ([`Request`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestWire {
    Effect {
        wait: WaitId,
        identity: EffectIdentity,
        effect: EffectName,
        args: Vec<Datum>,
        result: Type,
    },
    Sleep {
        wait: WaitId,
        identity: EffectIdentity,
        seconds: u64,
        nanoseconds: SubsecondNanos,
    },
}

/// What a park hands the parent ([`Park`]): its task table stays in the
/// machine's heap, and only its counts cross.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParkWire {
    pub requests: Vec<RequestWire>,
    pub withdrawn: Vec<WaitId>,
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
                        effect: request.effect,
                        args: request.args,
                        result: request.result,
                    },
                    Request::Sleep(sleep) => RequestWire::Sleep {
                        wait: sleep.wait,
                        identity: sleep.identity,
                        seconds: sleep.duration.as_secs(),
                        nanoseconds: SubsecondNanos(sleep.duration.subsec_nanos()),
                    },
                })
                .collect(),
            withdrawn: park.withdrawn,
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
                        effect,
                        args,
                        result,
                    } => Ok(Request::Effect(EffectRequest {
                        wait,
                        identity,
                        effect,
                        args,
                        result,
                    })),
                    RequestWire::Sleep {
                        wait,
                        identity,
                        seconds,
                        nanoseconds,
                    } => Ok(Request::Sleep(SleepRequest {
                        wait,
                        identity,
                        duration: Duration::from_secs(seconds)
                            .checked_add(Duration::from_nanos(u64::from(nanoseconds.0)))
                            .ok_or_else(|| {
                                PoolError::payload(PayloadKind::Park, "sleep duration overflows")
                            })?,
                    })),
                })
                .collect::<Result<_, PoolError>>()?,
            withdrawn: wire.withdrawn,
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
    Elapsed,
}

impl From<Outcome> for OutcomeWire {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Completed(value) => Self::Completed(value),
            Outcome::Failed(error) => Self::Failed(error),
            Outcome::Elapsed => Self::Elapsed,
        }
    }
}

impl From<OutcomeWire> for Outcome {
    fn from(wire: OutcomeWire) -> Self {
        match wire {
            OutcomeWire::Completed(value) => Self::Completed(value),
            OutcomeWire::Failed(error) => Self::Failed(error),
            OutcomeWire::Elapsed => Self::Elapsed,
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
    Uncaught(Datum),
    TasksOutstanding {
        unfinished: Vec<TaskIdentity>,
        unobserved: Vec<TaskIdentity>,
        sources: Vec<(TaskIdentity, lash_kernel_doc::Site)>,
    },
    Deadlock {
        waiting: Vec<TaskIdentity>,
    },
    Bound {
        bound: BoundWire,
        limit: u64,
        function: Option<FunctionName>,
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
            End::Error(RunError::Uncaught(error)) => Self::Uncaught(error),
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
                },
                limit: exceeded.limit,
                function: exceeded.function,
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
            Self::Uncaught(error) => End::Error(RunError::Uncaught(error)),
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
            } => End::Error(RunError::Bound(BoundExceeded {
                bound: match bound {
                    BoundWire::Charge => Bound::Charge,
                    BoundWire::Memory => Bound::Memory,
                    BoundWire::CallDepth => Bound::CallDepth,
                    BoundWire::LiveTasks => Bound::LiveTasks,
                    BoundWire::RequestsPerPark => Bound::RequestsPerPark,
                    BoundWire::JoinMembers => Bound::JoinMembers,
                    BoundWire::Guard { function } => Bound::Guard { function },
                },
                limit,
                function,
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

/// A read through a projection handle. The answer is
/// [`ProjectionAnswer`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionRead {
    pub handle: Handle,
    pub request: Datum,
}

/// Kernel data, or the error the read raises in the guest.
pub type ProjectionAnswer = Result<Datum, ErrorDatum>;

#[cfg(test)]
mod tests {
    use super::*;

    /// V11: decoding refuses normalization and overflow before park conversion.
    #[test]
    fn park_payloads_refuse_noncanonical_sleep_durations() {
        let identity = serde_json::json!({"task": "main", "site": {"unit": "main", "path": [0, 0]}, "occurrence": 0, "loops": []});
        for seconds in [0, u64::MAX] {
            for nanoseconds in [0, 999_999_999, 1_000_000_000, u32::MAX] {
                let json = serde_json::json!({"requests": [{"sleep": {"wait": 0, "identity": identity, "seconds": seconds, "nanoseconds": nanoseconds}}], "withdrawn": [], "outstanding": 0, "live": 0});
                let payload = encode(PayloadKind::Park, &json).unwrap();
                let decoded = decode::<ParkWire>(PayloadKind::Park, &payload);
                if nanoseconds < 1_000_000_000 {
                    let park = MachinePark::try_from(decoded.unwrap()).unwrap();
                    let Request::Sleep(sleep) = &park.requests[0] else {
                        panic!("sleep")
                    };
                    assert_eq!(sleep.duration.as_secs(), seconds);
                    assert_eq!(sleep.duration.subsec_nanos(), nanoseconds);
                } else {
                    assert!(decoded.is_err());
                    assert!(SubsecondNanos::try_from(nanoseconds).is_err());
                }
            }
        }
    }
}
