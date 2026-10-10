//! A kernel run's admissions and effect membership as checkpoint graph
//! nodes (FIG-5887 §3, §5, §10).
//!
//! - **FrozenRequest** (kind 19): what an effect asked for, frozen once at
//!   admission. A tool call holds the guest's own argument vector, losslessly,
//!   with the identity of the projection that makes its tool JSON and the
//!   call's dispatch metadata; the tool JSON itself is never stored, and an
//!   admission whose projection is not exactly the call's arguments is
//!   refused before anything is dispatched. A sleep holds its duration and
//!   absolute deadline; a refusal its typed error.
//! - **Admission** (kind 18): one effect's admission under its execution:
//!   the execution key, the machine's effect identity and wait, the request,
//!   and how it was admitted, with an execution's pin. It is the one
//!   authority for the request: the broker's membership, the host's call
//!   summary and the call's outcome all refer to it, none copies it.
//! - **EffectMembership** (kind 17): the waits a parked state stands on and
//!   the released executions still open, each by its admission, with the
//!   number the next admitting park takes. No request bytes.
//!
//! A restore resolves each effect identity the kernel saved through the
//! membership to exactly one admission of the same execution
//! ([`resolve_admission`]), and only then builds the in-memory request.

use std::collections::BTreeMap;

use lash_core_execution::runtime::actor::round::Material;
use lash_core_execution::runtime::actor::round::graph::PinnedMember;
use lash_core_execution::tool_dispatch::CellCall;
use lash_core_store::checkpoint_graph::{
    GraphDecoder, GraphRefusal, GraphStaging, NodeId, NodeKind, TokenCodec, TokenError,
    TokenReader, TokenWriter,
};
use lash_core_store::effect_opener::EffectOpener;
use lash_core_store::tool_run::{MaterialOwner, MaterialRef, MaterialRole};
use lash_durable::domain::ExecKey;
use lash_durable::domain::graph::{
    AdmissionMember, EffectAdmissionWrite, EffectKey, GraphDomainWrite,
};
use lash_kernel_doc::graph_tokens::{check_ascending, encode_body};
use lash_kernel_doc::{Datum, EffectIdentity, EffectName, ErrorDatum};

use crate::snapshot::OperationId;

fn unsupported(variant: u32) -> TokenError {
    TokenError::UnsupportedVariant { variant }
}

fn invalid(message: impl Into<String>) -> TokenError {
    TokenError::Invalid(message.into())
}

fn body(kind: NodeKind, reason: &str) -> GraphRefusal {
    GraphRefusal::InvalidBody {
        kind: kind.0,
        reason: reason.to_owned(),
    }
}

/// The canonical binary key of `effect`: its token encoding, the
/// `effect_admissions` key.
#[must_use]
pub fn effect_key(effect: &EffectIdentity) -> EffectKey {
    EffectKey(encode_body(effect).into())
}

// --- Frozen requests -----------------------------------------------------

/// How a guest's arguments become a tool call's JSON: a pure function the
/// host names by an identity, so a request frozen under one projection is
/// never read through another.
pub trait ArgumentProjection {
    /// The projection's identity.
    fn id(&self) -> &str;

    /// The tool JSON of `args`.
    ///
    /// # Errors
    ///
    /// Arguments the projection cannot carry.
    fn project(&self, args: &[Datum]) -> Result<serde_json::Value, String>;
}

/// Why a call's request was not frozen: nothing is dispatched.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FreezeRefusal {
    /// The projection cannot carry the guest's arguments.
    #[error("the arguments do not project: {0}")]
    Unprojectable(String),
    /// The projection of the guest's arguments is not the call's arguments:
    /// freezing them would lose or change what the call was made with.
    #[error("the call's arguments are not the projection of the guest's")]
    Lossy,
}

/// Kind 19: what one effect asked for, frozen at its admission.
#[derive(Clone, Debug, PartialEq)]
pub enum FrozenRequest {
    /// 0: a tool call. `call.args` is the projection of `args` under
    /// `projection`, and is not stored.
    Tool {
        call: CellCall,
        args: Vec<Datum>,
        projection: String,
    },
    /// 1: a sleep of `duration_ms`, over at `until_ms` on the store's clock.
    Sleep { duration_ms: u64, until_ms: i64 },
    /// 2: the parent refused the effect with `error`; nothing ran.
    Refused(ErrorDatum),
}

/// A frozen request as stored: a tool call's arguments not yet projected.
struct StoredRequest(FrozenRequest);

impl TokenCodec for StoredRequest {
    fn encode(&self, out: &mut TokenWriter) {
        match &self.0 {
            FrozenRequest::Tool {
                call,
                args,
                projection,
            } => {
                out.sum(0, 4);
                out.text(call.id.as_str());
                call.encode_dispatch(out);
                args.encode(out);
                out.text(projection);
            }
            FrozenRequest::Sleep {
                duration_ms,
                until_ms,
            } => {
                out.sum(1, 2);
                out.unsigned(*duration_ms);
                out.signed(*until_ms);
            }
            FrozenRequest::Refused(error) => {
                out.sum(2, 1);
                error.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        Ok(Self(match variant {
            0 => {
                TokenReader::expect_fields(fields, 4)?;
                let id = lash_sansio::ToolCallId::parse(input.text()?)
                    .map_err(|error| invalid(error.to_string()))?;
                let call = CellCall::decode_dispatch(input, id, serde_json::Value::Null)?;
                FrozenRequest::Tool {
                    call,
                    args: Vec::decode(input)?,
                    projection: input.text()?.to_owned(),
                }
            }
            1 => {
                TokenReader::expect_fields(fields, 2)?;
                FrozenRequest::Sleep {
                    duration_ms: input.unsigned()?,
                    until_ms: input.signed()?,
                }
            }
            2 => {
                TokenReader::expect_fields(fields, 1)?;
                FrozenRequest::Refused(ErrorDatum::decode(input)?)
            }
            other => return Err(unsupported(other)),
        }))
    }
}

impl FrozenRequest {
    /// The request of `call`, made by the guest with `args`: refused unless
    /// `projection` makes exactly the call's arguments of them.
    ///
    /// # Errors
    ///
    /// [`FreezeRefusal`].
    pub fn tool(
        call: CellCall,
        args: Vec<Datum>,
        projection: &dyn ArgumentProjection,
    ) -> Result<Self, FreezeRefusal> {
        let projected = projection
            .project(&args)
            .map_err(FreezeRefusal::Unprojectable)?;
        if projected != call.args {
            return Err(FreezeRefusal::Lossy);
        }
        Ok(Self::Tool {
            call,
            args,
            projection: projection.id().to_owned(),
        })
    }

    /// Stages this request and returns its node.
    ///
    /// # Errors
    ///
    /// Any refusal of the staging area.
    pub fn stage(&self, staging: &mut GraphStaging) -> Result<NodeId, GraphRefusal> {
        staging.encode_value(NodeKind::FROZEN_REQUEST, &StoredRequest(self.clone()))
    }

    /// Reads request `id`, a tool call's arguments projected again through
    /// `projection`, which must be the one it was frozen under.
    ///
    /// # Errors
    ///
    /// Any refusal of the node read, or [`GraphRefusal::InvalidBody`] for
    /// another projection or arguments it no longer carries.
    pub fn load(
        decoder: &mut GraphDecoder<'_>,
        id: NodeId,
        projection: &dyn ArgumentProjection,
    ) -> Result<Self, GraphRefusal> {
        let StoredRequest(mut request) = decoder.decode(id, NodeKind::FROZEN_REQUEST)?;
        if let Self::Tool {
            call,
            args,
            projection: frozen,
        } = &mut request
        {
            if frozen != projection.id() {
                return Err(body(
                    NodeKind::FROZEN_REQUEST,
                    "a request frozen under another projection",
                ));
            }
            call.args = projection
                .project(args)
                .map_err(|reason| body(NodeKind::FROZEN_REQUEST, &reason))?;
        }
        Ok(request)
    }
}

/// The request material of a call whose frozen request is `request`, owned
/// by `opener`'s run: minted over the node's identity, so the admission's
/// reference names exactly that node.
#[must_use]
pub fn request_material(opener: &EffectOpener, request: NodeId) -> MaterialRef {
    Material::journal_local(
        MaterialOwner::Run {
            opener: opener.clone(),
        },
        MaterialRole::PreparedRequest,
        request.to_hex(),
    )
    .reference()
    .clone()
}

// --- Admissions ----------------------------------------------------------

/// How an effect was admitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// 0: as an admitted execution of the park that minted `operation`,
    /// pinned as `pin` says.
    Execution {
        operation: OperationId,
        pin: Box<PinnedMember>,
    },
    /// 1: as a sleep.
    Sleep,
    /// 2: not at all.
    Refused,
}

/// Kind 18: one effect's admission under its execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admission {
    pub exec: ExecKey,
    pub effect: EffectIdentity,
    /// The machine's number for the wait its outcome is delivered to.
    pub wait: u64,
    /// The effect performed; none for a sleep.
    pub effect_name: Option<EffectName>,
    /// Its FrozenRequest.
    pub request: NodeId,
    pub disposition: Disposition,
}

impl TokenCodec for Admission {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(6);
        out.text(&self.exec.stored());
        self.effect.encode(out);
        out.unsigned(self.wait);
        self.effect_name.encode(out);
        self.request.encode(out);
        match &self.disposition {
            Disposition::Execution { operation, pin } => {
                out.sum(0, 3);
                out.unsigned(operation.run);
                out.unsigned(operation.ordinal);
                pin.encode(out);
            }
            Disposition::Sleep => out.sum(1, 0),
            Disposition::Refused => out.sum(2, 0),
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(6)?;
        let exec = ExecKey::parse(input.text()?).map_err(|error| invalid(error.to_string()))?;
        let effect = EffectIdentity::decode(input)?;
        let wait = input.unsigned()?;
        let effect_name = Option::decode(input)?;
        let request = NodeId::decode(input)?;
        let (variant, fields) = input.sum()?;
        let disposition = match variant {
            0 => {
                TokenReader::expect_fields(fields, 3)?;
                Disposition::Execution {
                    operation: OperationId {
                        run: input.unsigned()?,
                        ordinal: input.unsigned()?,
                    },
                    pin: Box::new(PinnedMember::decode(input)?),
                }
            }
            1 => {
                TokenReader::expect_fields(fields, 0)?;
                Disposition::Sleep
            }
            2 => {
                TokenReader::expect_fields(fields, 0)?;
                Disposition::Refused
            }
            other => return Err(unsupported(other)),
        };
        Ok(Self {
            exec,
            effect,
            wait,
            effect_name,
            request,
            disposition,
        })
    }
}

/// An admission read back with its request.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedAdmission {
    pub node: NodeId,
    pub admission: Admission,
    pub request: FrozenRequest,
}

impl Admission {
    /// Stages this admission and returns its node.
    ///
    /// # Errors
    ///
    /// Any refusal of the staging area.
    pub fn stage(&self, staging: &mut GraphStaging) -> Result<NodeId, GraphRefusal> {
        staging.encode_value(NodeKind::ADMISSION, self)
    }

    /// The `effect_admissions` row this admission is published as: under
    /// its execution and effect key, with the round member it is when it
    /// was admitted as an execution.
    #[must_use]
    pub fn write(&self, node: NodeId) -> GraphDomainWrite {
        let member = match &self.disposition {
            Disposition::Execution { operation, .. } => Some(AdmissionMember {
                owner: self.exec.owner(),
                run: lash_durable::domain::RunSeq(operation.run),
                member: operation.ordinal,
            }),
            Disposition::Sleep | Disposition::Refused => None,
        };
        GraphDomainWrite::EffectAdmission(EffectAdmissionWrite::Put {
            exec: self.exec.clone(),
            effect: effect_key(&self.effect),
            member,
            admission: node,
        })
    }

    /// Reads admission `id` with its request, checking the request is of
    /// the kind its disposition says, a call's pin names that request and
    /// that call, and a sleep's deadline is its request's.
    ///
    /// # Errors
    ///
    /// Any refusal of a node read, [`GraphRefusal::WrongChildKind`], or
    /// [`GraphRefusal::AdmissionMismatch`] for a disposition its request
    /// does not hold.
    pub fn load(
        decoder: &mut GraphDecoder<'_>,
        id: NodeId,
        projection: &dyn ArgumentProjection,
    ) -> Result<ResolvedAdmission, GraphRefusal> {
        let admission: Self = decoder.decode(id, NodeKind::ADMISSION)?;
        decoder.child_kind(id, admission.request, &[NodeKind::FROZEN_REQUEST])?;
        let request = FrozenRequest::load(decoder, admission.request, projection)?;
        let matches = match (&admission.disposition, &request) {
            (Disposition::Execution { pin, .. }, FrozenRequest::Tool { call, .. }) => {
                pin.call == call.id
                    && Material::new(pin.request.clone(), admission.request.to_hex()).is_ok()
            }
            (Disposition::Sleep, FrozenRequest::Sleep { .. })
            | (Disposition::Refused, FrozenRequest::Refused(_)) => true,
            _ => false,
        };
        if !matches {
            return Err(GraphRefusal::AdmissionMismatch(Box::new(
                admission.effect.clone(),
            )));
        }
        Ok(ResolvedAdmission {
            node: id,
            admission,
            request,
        })
    }
}

// --- Effect membership ---------------------------------------------------

/// Whether a member is a wait the state stands on (0) or a released
/// execution still open (1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberStatus {
    Pending,
    Released,
}

/// One effect the membership names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberEntry {
    pub effect: EffectIdentity,
    pub wait: u64,
    /// Its Admission.
    pub admission: NodeId,
    pub status: MemberStatus,
}

/// Kind 17: the effects a parked state stands on or has released.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectMembership {
    pub next_park: u64,
    /// Sorted by effect identity, each once.
    pub entries: Vec<MemberEntry>,
}

impl TokenCodec for EffectMembership {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        out.unsigned(self.next_park);
        out.seq(self.entries.len() as u64);
        for entry in &self.entries {
            out.seq(4);
            entry.effect.encode(out);
            out.unsigned(entry.wait);
            entry.admission.encode(out);
            out.sum(
                match entry.status {
                    MemberStatus::Pending => 0,
                    MemberStatus::Released => 1,
                },
                0,
            );
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let next_park = input.unsigned()?;
        let count = input.seq()?;
        let mut entries = Vec::new();
        for _ in 0..count {
            input.expect_struct(4)?;
            let effect = EffectIdentity::decode(input)?;
            let wait = input.unsigned()?;
            let admission = NodeId::decode(input)?;
            let (variant, fields) = input.sum()?;
            TokenReader::expect_fields(fields, 0)?;
            let status = match variant {
                0 => MemberStatus::Pending,
                1 => MemberStatus::Released,
                other => return Err(unsupported(other)),
            };
            entries.push(MemberEntry {
                effect,
                wait,
                admission,
                status,
            });
        }
        let effects: Vec<&EffectIdentity> = entries.iter().map(|entry| &entry.effect).collect();
        check_ascending(&effects)?;
        Ok(Self { next_park, entries })
    }
}

impl EffectMembership {
    /// The membership of `entries`, sorted by effect identity.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::DuplicateEffect`] for an effect named twice.
    pub fn new(next_park: u64, mut entries: Vec<MemberEntry>) -> Result<Self, GraphRefusal> {
        entries.sort_by(|left, right| left.effect.cmp(&right.effect));
        if let Some(pair) = entries
            .windows(2)
            .find(|pair| pair[0].effect == pair[1].effect)
        {
            return Err(GraphRefusal::DuplicateEffect(Box::new(
                pair[0].effect.clone(),
            )));
        }
        Ok(Self { next_park, entries })
    }

    /// Stages this membership and returns its node.
    ///
    /// # Errors
    ///
    /// Any refusal of the staging area.
    pub fn stage(&self, staging: &mut GraphStaging) -> Result<NodeId, GraphRefusal> {
        staging.encode_value(NodeKind::EFFECT_MEMBERSHIP, self)
    }

    /// Reads membership `id` of `exec` with every admission it names,
    /// checking each is an admission of this execution, of its entry's
    /// effect and wait, minted by a park this membership has issued, and
    /// that no two waits the state stands on share a number.
    ///
    /// # Errors
    ///
    /// Any refusal of a node read, [`GraphRefusal::WrongChildKind`], or
    /// [`GraphRefusal::AdmissionMismatch`].
    pub fn load(
        decoder: &mut GraphDecoder<'_>,
        id: NodeId,
        exec: &ExecKey,
        projection: &dyn ArgumentProjection,
    ) -> Result<(Self, BTreeMap<EffectIdentity, ResolvedAdmission>), GraphRefusal> {
        let membership: Self = decoder.decode(id, NodeKind::EFFECT_MEMBERSHIP)?;
        let mut resolved = BTreeMap::new();
        let mut waits = std::collections::BTreeSet::new();
        for entry in &membership.entries {
            decoder.child_kind(id, entry.admission, &[NodeKind::ADMISSION])?;
            let admission = Admission::load(decoder, entry.admission, projection)?;
            let mismatch = || GraphRefusal::AdmissionMismatch(Box::new(entry.effect.clone()));
            let issued = match &admission.admission.disposition {
                Disposition::Execution { operation, .. } => operation.run < membership.next_park,
                Disposition::Sleep | Disposition::Refused => entry.status == MemberStatus::Pending,
            };
            if &admission.admission.exec != exec
                || admission.admission.effect != entry.effect
                || admission.admission.wait != entry.wait
                || !issued
                || (entry.status == MemberStatus::Pending && !waits.insert(entry.wait))
            {
                return Err(mismatch());
            }
            resolved.insert(entry.effect.clone(), admission);
        }
        Ok((membership, resolved))
    }
}

/// The admission `exec`'s saved state names for `effect`, through its
/// membership `membership`: exactly one admission of the same execution, or
/// a typed refusal before any request is built.
///
/// # Errors
///
/// [`GraphRefusal::AdmissionMissing`] when the membership does not name
/// it; any refusal of [`EffectMembership::load`].
pub fn resolve_admission(
    decoder: &mut GraphDecoder<'_>,
    membership: NodeId,
    exec: &ExecKey,
    effect: &EffectIdentity,
    projection: &dyn ArgumentProjection,
) -> Result<ResolvedAdmission, GraphRefusal> {
    let (_, mut resolved) = EffectMembership::load(decoder, membership, exec, projection)?;
    resolved
        .remove(effect)
        .ok_or_else(|| GraphRefusal::AdmissionMissing(Box::new(effect.clone())))
}

// --- A park's graph -----------------------------------------------------

/// The admissions of one execution this activation knows, by effect: what
/// a park's membership names and a restore resolved. It holds no request
/// copy beyond the frozen request each admission was staged from.
#[derive(Clone, Debug, Default)]
pub struct AdmissionBook {
    admitted: BTreeMap<EffectIdentity, (NodeId, Admission)>,
}

/// One wait a park admits, frozen before its commit opens.
#[derive(Clone, Debug, PartialEq)]
pub struct FrozenAdmission {
    pub effect: EffectIdentity,
    pub wait: u64,
    /// The effect performed; none for a sleep.
    pub effect_name: Option<EffectName>,
    pub request: FrozenRequest,
    /// The execution it was admitted as, with its pin; none for a sleep or
    /// a refusal.
    pub execution: Option<(OperationId, PinnedMember)>,
}

impl AdmissionBook {
    /// The book of the admissions a restore resolved.
    #[must_use]
    pub fn restored(resolved: BTreeMap<EffectIdentity, ResolvedAdmission>) -> Self {
        Self {
            admitted: resolved
                .into_iter()
                .map(|(effect, resolved)| (effect, (resolved.node, resolved.admission)))
                .collect(),
        }
    }

    /// The admission node of `effect`.
    #[must_use]
    pub fn get(&self, effect: &EffectIdentity) -> Option<NodeId> {
        self.admitted.get(effect).map(|(node, _)| *node)
    }

    /// Stages `frozen`'s request and admission under `exec`, records it, and
    /// returns the `effect_admissions` row that publishes it.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::DuplicateEffect`] for an effect already admitted, or
    /// any refusal of the staging area.
    pub fn admit(
        &mut self,
        staging: &mut GraphStaging,
        exec: &ExecKey,
        frozen: FrozenAdmission,
    ) -> Result<GraphDomainWrite, GraphRefusal> {
        if self.admitted.contains_key(&frozen.effect) {
            return Err(GraphRefusal::DuplicateEffect(Box::new(frozen.effect)));
        }
        let request = frozen.request.stage(staging)?;
        let disposition = match (frozen.execution, &frozen.request) {
            (Some((operation, pin)), FrozenRequest::Tool { .. }) => Disposition::Execution {
                operation,
                pin: Box::new(pin),
            },
            (None, FrozenRequest::Sleep { .. }) => Disposition::Sleep,
            (None, FrozenRequest::Refused(_)) => Disposition::Refused,
            _ => {
                return Err(GraphRefusal::AdmissionMismatch(Box::new(frozen.effect)));
            }
        };
        let admission = Admission {
            exec: exec.clone(),
            effect: frozen.effect.clone(),
            wait: frozen.wait,
            effect_name: frozen.effect_name,
            request,
            disposition,
        };
        let node = admission.stage(staging)?;
        let write = admission.write(node);
        self.admitted.insert(frozen.effect, (node, admission));
        Ok(write)
    }

    /// The membership `ledger` stands on: each wait by its admission, and
    /// each released execution by the admission that minted it.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::AdmissionMissing`] for a wait no admission here
    /// names, [`GraphRefusal::InvalidBody`] for such a released execution, [`GraphRefusal::AdmissionMismatch`] for one on
    /// another wait.
    pub fn membership(
        &self,
        ledger: &super::EffectLedger,
    ) -> Result<EffectMembership, GraphRefusal> {
        let mut entries = Vec::new();
        for (effect, pending) in ledger.pending() {
            let (node, admission) = self
                .admitted
                .get(effect)
                .ok_or_else(|| GraphRefusal::AdmissionMissing(Box::new(effect.clone())))?;
            if admission.wait != pending.wait {
                return Err(GraphRefusal::AdmissionMismatch(Box::new(effect.clone())));
            }
            entries.push(MemberEntry {
                effect: effect.clone(),
                wait: pending.wait,
                admission: *node,
                status: MemberStatus::Pending,
            });
        }
        for released in ledger.released() {
            let (effect, (node, admission)) = self
                .admitted
                .iter()
                .find(|(_, (_, admission))| {
                    matches!(&admission.disposition,
                        Disposition::Execution { operation, .. } if *operation == released.operation)
                })
                .ok_or_else(|| {
                    body(
                        NodeKind::EFFECT_MEMBERSHIP,
                        "a released execution no admission minted",
                    )
                })?;
            entries.push(MemberEntry {
                effect: effect.clone(),
                wait: admission.wait,
                admission: *node,
                status: MemberStatus::Released,
            });
        }
        EffectMembership::new(ledger.next_park(), entries)
    }

    /// Forgets every admission `membership` no longer names, and returns
    /// the rows that remove their lookups: no live wait or released
    /// execution needs them. Their outcomes and summaries keep the
    /// historical request through their own references.
    pub fn retire_unnamed(
        &mut self,
        exec: &ExecKey,
        membership: &EffectMembership,
    ) -> Vec<GraphDomainWrite> {
        let named: std::collections::BTreeSet<&EffectIdentity> = membership
            .entries
            .iter()
            .map(|entry| &entry.effect)
            .collect();
        let retired: Vec<EffectIdentity> = self
            .admitted
            .keys()
            .filter(|effect| !named.contains(effect))
            .cloned()
            .collect();
        retired
            .into_iter()
            .filter_map(|effect| {
                let (node, _) = self.admitted.remove(&effect)?;
                Some(GraphDomainWrite::EffectAdmission(
                    EffectAdmissionWrite::Remove {
                        exec: exec.clone(),
                        effect: effect_key(&effect),
                        expected: node,
                    },
                ))
            })
            .collect()
    }
}

/// The execution root's parts a park or an end publishes beside its
/// membership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootParts {
    /// A KernelWhole while parked; a CellEnd once ended.
    pub state: NodeId,
    pub status: lash_core_store::checkpoint_graph::bodies::ExecStatus,
    /// The document the run executes.
    pub executable_identity: String,
    pub format: u32,
    /// A HostRoot.
    pub host: Option<NodeId>,
    /// A CellEnvelope.
    pub envelope: Option<NodeId>,
}

/// The owner binding of `exec`.
#[must_use]
pub fn owner_binding(exec: &ExecKey) -> lash_core_store::checkpoint_graph::bodies::OwnerBinding {
    use lash_core_store::checkpoint_graph::bodies::OwnerBinding;
    match exec {
        ExecKey::Cell(session, turn, cell) => OwnerBinding::Cell {
            session: session.as_str().to_owned(),
            turn: turn.as_str().to_owned(),
            cell: cell.as_str().to_owned(),
        },
        ExecKey::Process(process) => OwnerBinding::Process {
            process: process.as_str().to_owned(),
        },
    }
}

/// Stages `exec`'s complete execution root over `parts` and `membership`,
/// and returns it with the row that publishes it in place of `expected`.
/// The root depends on no earlier root.
///
/// # Errors
///
/// Any refusal of the staging area, [`GraphRefusal::RootTooLarge`]
/// included.
pub fn stage_exec_root(
    staging: &mut GraphStaging,
    exec: &ExecKey,
    membership: Option<&EffectMembership>,
    parts: RootParts,
    expected: (Option<lash_durable::domain::SnapshotRev>, Option<NodeId>),
    commit_id: lash_core_store::checkpoint_graph::CommitId,
) -> Result<(NodeId, GraphDomainWrite), GraphRefusal> {
    use lash_core_store::checkpoint_graph::bodies::{ExecRoot, ExecRootKind};
    let owner = staging.encode_value(NodeKind::OWNER_BINDING, &owner_binding(exec))?;
    let effect_membership = membership
        .map(|membership| membership.stage(staging))
        .transpose()?;
    let root = staging.encode_value(
        NodeKind::EXEC_ROOT,
        &ExecRoot {
            exec: match exec {
                ExecKey::Cell(..) => ExecRootKind::Cell,
                ExecKey::Process(_) => ExecRootKind::Process,
            },
            owner,
            executable_identity: parts.executable_identity,
            format: parts.format,
            status: parts.status,
            state: parts.state,
            effect_membership,
            host: parts.host,
            envelope: parts.envelope,
        },
    )?;
    let write = GraphDomainWrite::ExecRoot(lash_durable::domain::graph::ExecRootWrite::Put {
        exec: exec.clone(),
        expected: expected.0,
        expected_root: expected.1,
        root,
        commit_id,
    });
    Ok((root, write))
}

#[cfg(test)]
#[path = "graph_tests.rs"]
mod tests;
