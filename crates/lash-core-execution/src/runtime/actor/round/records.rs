//! The run records' bodies and where they go (the layout is in the module
//! documentation of [`super`]).

use std::time::Duration;

use lash_core_store::tool_run::{AttemptOutcome, MaterialRef};
use lash_durable::domain::{Ordinal, OwnerKey, RunRecordKind, RunRecordWrite, RunSeq};
use lash_durable::{DomainWrite, DurableInstant};
use lash_sansio::{ExecutionLimit, ExecutionPolicy};
use serde::{Deserialize, Serialize};

use super::ExecutionDraft;
use crate::runtime::actor::waits::WaitDeadline;
use crate::{ToolCallId, ToolId};

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
            wait_deadline_ms: draft.wait().map(|wait| wait.at().0),
        }
    }

    /// The draft the admission pinned, exactly: never refreshed.
    pub(super) fn draft(&self) -> ExecutionDraft {
        ExecutionDraft::new(
            self.call.clone(),
            ToolId::new(self.tool.clone()),
            self.request.clone(),
            self.policy,
            ExecutionLimit {
                expires_at: self.limit_expires_at_ms,
                max_slice: Duration::from_millis(self.limit_max_slice_ms),
            },
            self.wait_deadline_ms
                .map(|at| WaitDeadline::at_instant(DurableInstant(at))),
        )
    }
}

/// An `x_start` record's body: which member, and which attempt of its call.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StartBody {
    pub(super) member: u64,
    pub(super) attempt: u32,
}

/// An `x_outcome` record's body: the call's final outcome, settling the
/// attempt started at `start`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OutcomeBody {
    pub(super) start: u64,
    pub(super) outcome: AttemptOutcome,
    pub(super) material: Option<String>,
}

/// A `retry` record's body: the attempt started at `start` failed in a way
/// its pinned `Repeatable` contract repeats, and the next attempt is due at
/// `due_at_ms`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RetryBody {
    pub(super) start: u64,
    pub(super) outcome: AttemptOutcome,
    pub(super) material: Option<String>,
    pub(super) due_at_ms: i64,
}

/// A `present` record's body: the calls presented, in declared order.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PresentBody {
    pub(super) calls: Vec<ToolCallId>,
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
