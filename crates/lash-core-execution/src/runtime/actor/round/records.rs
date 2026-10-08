//! The run records' bodies and where they go (the layout is in the module
//! documentation of [`super`]).

use std::time::Duration;

use lash_core_store::tool_run::MaterialRef;
use lash_durable::domain::{Ordinal, OwnerKey, RunRecordKind, RunRecordWrite, RunSeq};
use lash_durable::{DomainWrite, DurableInstant};
use lash_sansio::{ExecutionLimit, ExecutionPolicy};
use serde::{Deserialize, Serialize};

use super::{ExecutionDraft, PinnedWait, SettledOutput};
use crate::runtime::actor::waits::{ParkDeadline, WaitDeadline, WaitId};
use crate::{ToolCallId, ToolId};

/// The format of a run record's body: the admission, start, outcome, retry
/// and presentation records below. Bodies carry no stamp of their own: the
/// record's actor carries this version in its format set, so only a node
/// that decodes them claims it (ADR 0106 §1).
///
/// version_guard(
///     shapes(
///         path = "crates/lash-core-execution/src/runtime/actor/round/records.rs",
///         cover(
///             AdmitBody, AdmittedMember, StartBody, OutcomeBody, RetryBody, PresentBody,
///             DecideBody,
///         ),
///     ),
/// )
/// version_surface = "drain"
/// format_manifest = "RunRecord"
pub const RUN_RECORD_FORMAT_VERSION: u32 = 1;

/// The admission's ordinal.
pub(super) const ADMIT_ORDINAL: Ordinal = Ordinal(0);

/// The ordinal of member `member`'s first `x_start`, committed with the
/// admission.
pub(super) fn first_start(member: u64) -> Ordinal {
    Ordinal(1 + member)
}

/// The admission record's body.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AdmitBody {
    pub(super) members: Vec<AdmittedMember>,
}

/// One member as its admission pinned it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AdmittedMember {
    call: ToolCallId,
    tool: String,
    request: MaterialRef,
    policy: ExecutionPolicy,
    limit_expires_at_ms: u64,
    limit_max_slice_ms: u64,
    wait_deadline_ms: Option<i64>,
    /// The completion wait pinned with the admission, by its id's hex.
    wait_id: Option<String>,
    /// The call's trace scope the admission retained (FIG-5382).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trace: Option<lash_trace::DurableTraceScope>,
}

impl AdmittedMember {
    fn of(draft: &ExecutionDraft) -> Self {
        Self {
            call: draft.call().clone(),
            tool: draft.tool().as_str().to_owned(),
            request: draft.request().clone(),
            policy: draft.policy(),
            limit_expires_at_ms: draft.limit().expires_at,
            limit_max_slice_ms: millis(draft.limit().max_slice),
            wait_deadline_ms: draft
                .park()
                .and_then(ParkDeadline::deadline)
                .map(|deadline| deadline.at().0),
            wait_id: draft.pinned_wait().map(|pinned| pinned.id.to_hex()),
            trace: draft.trace().cloned(),
        }
    }

    /// The draft the admission pinned, exactly: never refreshed. A member
    /// with a pinned wait and no deadline parks until its scope ends.
    ///
    /// # Errors
    ///
    /// A pinned wait whose id does not decode, or a park deadline without a
    /// pinned wait.
    pub(super) fn draft(&self) -> Result<ExecutionDraft, &'static str> {
        let pinned = self
            .wait_id
            .as_deref()
            .map(|id| {
                WaitId::parse_hex(id)
                    .map(|id| PinnedWait { id })
                    .ok_or("a pinned wait id is not a wait id")
            })
            .transpose()?;
        let deadline = self
            .wait_deadline_ms
            .map(|at| WaitDeadline::at_instant(DurableInstant(at)));
        let park = match (pinned, deadline) {
            (Some(_), Some(deadline)) => Some(ParkDeadline::At(deadline)),
            (Some(_), None) => Some(ParkDeadline::UntilScopeEnd),
            (None, None) => None,
            (None, Some(_)) => return Err("a park deadline has no pinned wait"),
        };
        Ok(ExecutionDraft::new(
            self.call.clone(),
            ToolId::new(self.tool.clone()),
            self.request.clone(),
            self.policy,
            ExecutionLimit {
                expires_at: self.limit_expires_at_ms,
                max_slice: Duration::from_millis(self.limit_max_slice_ms),
            },
            park,
        )
        .with_pinned_wait(pinned)
        .with_trace(self.trace.clone()))
    }
}

/// An `x_start` record's body: which member, and which attempt of its call.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StartBody {
    pub(super) member: u64,
    pub(super) attempt: u32,
}

/// An `x_outcome` or `x_wait` record's body: the call's final output, or
/// its park, settling the attempt started at `start`. It decodes only with
/// the payload its outcome names. A completion's plugin-state resolutions
/// commit in it (ADR 0132 §5); a record that carries none omits them.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OutcomeBody {
    pub(super) start: u64,
    pub(super) output: SettledOutput,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) state: Vec<lash_core_store::tool_run::StateResolution>,
}

/// A `retry` record's body: the attempt started at `start` failed in a way
/// its pinned `Repeatable` contract repeats, and the next attempt is due at
/// `due_at_ms`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RetryBody {
    pub(super) start: u64,
    pub(super) output: SettledOutput,
    pub(super) due_at_ms: i64,
}

/// A `present` record's body: the calls presented, in declared order.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PresentBody {
    pub(super) calls: Vec<ToolCallId>,
}

/// A `decide` record's body: a decision of the round's coordinator, which
/// no member's recovery depends on.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub(super) enum DecideBody {
    /// The trace admissions the round's admission retained are exported:
    /// the admission's export obligation is discharged (FIG-5452).
    TraceExported,
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(super) fn encode<T: Serialize>(body: &T) -> String {
    #[expect(
        clippy::expect_used,
        reason = "the record bodies are plain data whose encoding cannot fail"
    )]
    serde_json::to_string(body).expect("a run record body encodes")
}

/// Append one record of `owner`'s run `run` at `ordinal`.
pub(super) fn append(
    owner: &OwnerKey,
    run: RunSeq,
    ordinal: Ordinal,
    kind: RunRecordKind,
    call: Option<&ToolCallId>,
    record_json: String,
) -> DomainWrite {
    DomainWrite::RunRecord(RunRecordWrite::Append {
        owner: owner.clone(),
        run,
        ordinal,
        kind,
        call: call.cloned(),
        record_json,
    })
}

/// The admission record of `drafts`.
pub(super) fn admit_record(
    owner: &OwnerKey,
    run: RunSeq,
    drafts: &[ExecutionDraft],
) -> DomainWrite {
    append(
        owner,
        run,
        ADMIT_ORDINAL,
        RunRecordKind::Admit,
        None,
        encode(&AdmitBody {
            members: drafts.iter().map(AdmittedMember::of).collect(),
        }),
    )
}

/// The `x_start` of `draft`, member `member`, for `attempt`.
pub(super) fn start_record(
    owner: &OwnerKey,
    run: RunSeq,
    ordinal: Ordinal,
    draft: &ExecutionDraft,
    member: u64,
    attempt: u32,
) -> DomainWrite {
    append(
        owner,
        run,
        ordinal,
        RunRecordKind::XStart,
        Some(draft.call()),
        encode(&StartBody { member, attempt }),
    )
}

/// The `decide` record of `decision`, at `ordinal` of `owner`'s run `run`.
pub(super) fn decide_record(
    owner: &OwnerKey,
    run: RunSeq,
    ordinal: Ordinal,
    decision: &DecideBody,
) -> DomainWrite {
    append(
        owner,
        run,
        ordinal,
        RunRecordKind::Decide,
        None,
        encode(decision),
    )
}
