//! The group's own notifications (FIG-4344, ADR 0099 §2 and §5): what a
//! waiter subscribes to, how the index answers it from its record alone, and
//! how the handler whose state change makes a notice true completes its
//! subscribers.
//!
//! A waiter creates an awakeable in its own journal and subscribes it under
//! the notice it waits for. `subscribe` is exclusive on the group, the same
//! authority every transition goes through, so it either answers from the
//! record at once or records the subscriber before any later transition can
//! run: a notice that became true before the subscription and one that
//! becomes true after it both reach the waiter. The handler that makes a
//! notice true completes its subscribers' awakeables from its own journal,
//! after it stores the record: a completion is a command of that invocation,
//! never a call to another service. Every answer is monotonic under the
//! index's transitions, so a subscriber that arrives late — a redrive, a
//! successor — is answered from the record without having seen anything
//! earlier. A notification is a hint to re-read the authority, never a
//! permission of its own.

use super::*;

/// The state key the group's outstanding subscribers live under, apart from
/// the index record so the handlers that notify nobody never read it.
const SUBSCRIPTIONS_STATE_KEY: &str = "effect-group/v1/subscriptions";

/// What a subscriber waits for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupNotice {
    /// The group has left `Preparing`: the opener's readiness, and a child's
    /// admission, which it then decides with a fresh `admit_child`.
    Ready,
    /// Every rank up to `rank` has seated: the consuming await's next rank is
    /// inside the seated prefix.
    Rank { rank: u64 },
    /// The §5 barrier for `rank` has lifted: every committed sibling ranked
    /// below it has seated.
    Drained { rank: u64 },
    /// The child at `position` has a cancel fact: its cancel was decided, or
    /// its settlement seated, which no cancel can reach any more.
    ChildCancel { position: usize },
}

impl EffectGroupNotice {
    /// The idempotency key every attach from outside a handler to this
    /// notice of `group_key` carries: they all join one `await_notice`
    /// invocation, however often they reattach (FIG-4345).
    pub(crate) fn attachment(&self, group_key: &str) -> String {
        let notice = match self {
            Self::Ready => "ready".to_string(),
            Self::Rank { rank } => format!("rank:{rank}"),
            Self::Drained { rank } => format!("drained:{rank}"),
            Self::ChildCancel { position } => format!("child-cancel:{position}"),
        };
        format!("lash-group-notice:{group_key}:{notice}")
    }
}

/// A notice's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupNotification {
    /// The group left `Preparing` ready: admission is decidable.
    Ready,
    /// The group was refused before it became ready.
    Refused { reason: EffectGroupRefusal },
    /// The asked rank is inside the seated prefix.
    Rank,
    /// The asked barrier has lifted.
    Drained,
    /// The child's cancel was decided.
    Cancel,
    /// The child's settlement seated: no cancel can reach it.
    Settled,
    /// The group retired: every notice is answered, and nothing it answered
    /// before still permits anything.
    Retired,
    /// The index holds no record of this group.
    Absent,
}

impl EffectGroupNotification {
    /// Whether a child's cancel fact is a cancel: a decided cancel or a
    /// retirement. A seated child, or a group the index has no record of, is
    /// not cancelled.
    pub(crate) fn is_child_cancel(&self) -> bool {
        matches!(self, Self::Cancel | Self::Retired)
    }
}

/// A subscription: the notice and the subscriber's awakeable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupSubscribeRequest {
    pub notice: EffectGroupNotice,
    pub awakeable_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupSubscribeResponse {
    /// The notice is already true; nothing was recorded and the awakeable
    /// will not be completed.
    Notified {
        notification: EffectGroupNotification,
    },
    /// The subscriber is recorded; its awakeable is completed with the answer
    /// once the notice is true.
    Subscribed,
    /// The group already holds `outstanding` subscribers, its ceiling:
    /// nothing was recorded.
    Refused { outstanding: usize },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupUnsubscribeRequest {
    pub awakeable_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupChildCancelRequest {
    pub position: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct EffectGroupSubscriber {
    notice: EffectGroupNotice,
    awakeable_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct EffectGroupSubscriptions {
    entries: Vec<EffectGroupSubscriber>,
}

/// The most subscribers one group holds at once. A waiter subscribes once
/// per wait and a lost race arm unsubscribes, so a group's own traffic stays
/// far below it; past it a subscription is refused typed rather than grown.
pub(crate) fn subscription_ceiling(children: usize) -> usize {
    64 + 16 * children
}

/// The answer `notice` has in `record`, or `None` while it is not true.
pub(crate) fn notice_answer(
    record: Option<&EffectGroupStateRecord>,
    notice: &EffectGroupNotice,
) -> Option<EffectGroupNotification> {
    let Some(record) = record else {
        return Some(EffectGroupNotification::Absent);
    };
    let live = match &record.lifecycle {
        EffectGroupLifecycle::Retired { .. } => return Some(EffectGroupNotification::Retired),
        EffectGroupLifecycle::Preparing { live, .. }
        | EffectGroupLifecycle::Ready { live, .. }
        | EffectGroupLifecycle::Closed { live, .. } => live,
    };
    match notice {
        EffectGroupNotice::Ready => match &record.lifecycle {
            EffectGroupLifecycle::Preparing { .. } => None,
            EffectGroupLifecycle::Closed {
                effective: EffectGroupCloseOutcome::Refused { reason },
                ..
            } => Some(EffectGroupNotification::Refused {
                reason: reason.clone(),
            }),
            EffectGroupLifecycle::Ready { .. }
            | EffectGroupLifecycle::Closed { .. }
            | EffectGroupLifecycle::Retired { .. } => Some(EffectGroupNotification::Ready),
        },
        EffectGroupNotice::Rank { rank } => {
            (*rank <= live.seated_prefix()).then_some(EffectGroupNotification::Rank)
        }
        EffectGroupNotice::Drained { rank } => blocking_positions(live, *rank)
            .is_empty()
            .then_some(EffectGroupNotification::Drained),
        EffectGroupNotice::ChildCancel { position } => match live.commit_states.get(position) {
            Some(EffectGroupChildCommitState::CancelDecided) => {
                Some(EffectGroupNotification::Cancel)
            }
            _ if live.settled_positions.contains_key(position) => {
                Some(EffectGroupNotification::Settled)
            }
            _ => None,
        },
    }
}

async fn load_subscriptions(
    ctx: &ObjectContext<'_>,
) -> Result<EffectGroupSubscriptions, TerminalError> {
    Ok(object_state::get_stamped(
        ctx,
        SUBSCRIPTIONS_STATE_KEY,
        &protocol::EFFECT_GROUP_STATE_FORMATS,
    )
    .await?
    .unwrap_or_default())
}

fn store_subscriptions(
    ctx: &ObjectContext<'_>,
    writer: object_state::StoredValueWriter,
    subscriptions: EffectGroupSubscriptions,
) {
    if subscriptions.entries.is_empty() {
        ctx.clear(SUBSCRIPTIONS_STATE_KEY);
    } else {
        object_state::set_stamped(ctx, SUBSCRIPTIONS_STATE_KEY, writer, subscriptions);
    }
}

/// Completes every subscriber whose notice `record` now answers, and keeps
/// the rest. The last step of every handler that can make a notice true, run
/// after it stored `record`: the kept list is written before any awakeable is
/// completed, and a replay of the handler reads the same journaled list, so a
/// crash between the write and the completions, or between two completions,
/// emits the same completions on the retry.
pub(super) async fn notify_satisfied(
    ctx: &ObjectContext<'_>,
    writer: object_state::StoredValueWriter,
    record: &EffectGroupStateRecord,
) -> Result<(), TerminalError> {
    let subscriptions = load_subscriptions(ctx).await?;
    if subscriptions.entries.is_empty() {
        return Ok(());
    }
    let mut kept = Vec::with_capacity(subscriptions.entries.len());
    let mut satisfied = Vec::new();
    for subscriber in subscriptions.entries {
        match notice_answer(Some(record), &subscriber.notice) {
            Some(notification) => satisfied.push((subscriber.awakeable_id, notification)),
            None => kept.push(subscriber),
        }
    }
    if satisfied.is_empty() {
        return Ok(());
    }
    store_subscriptions(ctx, writer, EffectGroupSubscriptions { entries: kept });
    for (awakeable_id, notification) in satisfied {
        ctx.resolve_awakeable(&awakeable_id, Json(notification));
    }
    Ok(())
}

/// `subscribe`'s body: answer from the record, or record the subscriber.
/// Idempotent by awakeable: a retried subscription records nothing twice.
pub(super) async fn subscribe(
    ctx: &ObjectContext<'_>,
    writer: object_state::StoredValueWriter,
    request: EffectGroupSubscribeRequest,
) -> Result<EffectGroupSubscribeResponse, TerminalError> {
    let record = load_index(ctx).await?;
    if let Some(notification) = notice_answer(record.as_ref(), &request.notice) {
        return Ok(EffectGroupSubscribeResponse::Notified { notification });
    }
    let children = record.as_ref().map_or(Ok(0), |record| {
        record.live().map(|live| live.shape.children())
    })?;
    let mut subscriptions = load_subscriptions(ctx).await?;
    if subscriptions
        .entries
        .iter()
        .any(|entry| entry.awakeable_id == request.awakeable_id)
    {
        return Ok(EffectGroupSubscribeResponse::Subscribed);
    }
    let outstanding = subscriptions.entries.len();
    if outstanding >= subscription_ceiling(children) {
        return Ok(EffectGroupSubscribeResponse::Refused { outstanding });
    }
    subscriptions.entries.push(EffectGroupSubscriber {
        notice: request.notice,
        awakeable_id: request.awakeable_id,
    });
    store_subscriptions(ctx, writer, subscriptions);
    Ok(EffectGroupSubscribeResponse::Subscribed)
}

/// `unsubscribe`'s body: drop the subscriber whose race the other arm won.
pub(super) async fn unsubscribe(
    ctx: &ObjectContext<'_>,
    writer: object_state::StoredValueWriter,
    request: EffectGroupUnsubscribeRequest,
) -> Result<(), TerminalError> {
    let mut subscriptions = load_subscriptions(ctx).await?;
    let before = subscriptions.entries.len();
    subscriptions
        .entries
        .retain(|entry| entry.awakeable_id != request.awakeable_id);
    if subscriptions.entries.len() != before {
        store_subscriptions(ctx, writer, subscriptions);
    }
    Ok(())
}

/// `retire`'s notification: every subscriber is answered `Retired` and the
/// list is emptied. Later subscribers are answered from the retired record.
pub(super) async fn notify_retired(
    ctx: &ObjectContext<'_>,
    writer: object_state::StoredValueWriter,
) -> Result<(), TerminalError> {
    let subscriptions = load_subscriptions(ctx).await?;
    if subscriptions.entries.is_empty() {
        return Ok(());
    }
    store_subscriptions(ctx, writer, EffectGroupSubscriptions::default());
    for subscriber in subscriptions.entries {
        ctx.resolve_awakeable(
            &subscriber.awakeable_id,
            Json(EffectGroupNotification::Retired),
        );
    }
    Ok(())
}

/// Awaits `notice` of `group_key` from a handler: an awakeable of the
/// caller's own journal, subscribed at the group index, which completes it
/// once the notice is true. One journaled call; an already-true notice is
/// answered by it, and the awakeable is then never completed. A macro rather
/// than a function so each handler context keeps its own concrete lifetimes.
macro_rules! await_group_notice {
    ($ctx:expr, $namespace:expr, $group_key:expr, $notice:expr) => {{
        let group_key: &str = $group_key;
        let (awakeable_id, awakeable) = restate_sdk::context::ContextAwakeables::awakeable::<
            restate_sdk::serde::Json<$crate::effect_group::EffectGroupNotification>,
        >($ctx);
        let subscribed = $namespace
            .effect_group_state($ctx, group_key.to_string())
            .subscribe($crate::effect_group::EffectGroupSubscribeRequest {
                notice: $notice,
                awakeable_id,
            })
            .call()
            .await
            .map($crate::compat::Reply::into_body);
        match subscribed {
            Ok($crate::effect_group::EffectGroupSubscribeResponse::Notified { notification }) => {
                Ok(notification)
            }
            Ok($crate::effect_group::EffectGroupSubscribeResponse::Subscribed) => {
                awakeable.await.map(restate_sdk::serde::Json::into_inner)
            }
            Ok($crate::effect_group::EffectGroupSubscribeResponse::Refused { outstanding }) => Err(
                $crate::effect_group::subscription_refused(group_key, outstanding),
            ),
            Err(error) => Err(error),
        }
    }};
}
pub(crate) use await_group_notice;

/// The typed refusal of a subscription past the group's ceiling.
pub(crate) fn subscription_refused(group_key: &str, outstanding: usize) -> TerminalError {
    TerminalError::new(format!(
        "effect group {group_key} refused a subscription: it already holds {outstanding} \
         outstanding subscribers, its ceiling"
    ))
}

/// Awaits `notice` of `group_key` from outside any handler: through the
/// index's shared `await_notice`, which holds the awakeable, under the
/// notice's idempotency key, so every attach joins one server-side waiter. An
/// attach-ceiling timeout reattaches to the same waiter; any other ingress
/// failure is the caller's.
#[allow(
    clippy::result_large_err,
    reason = "the attach answers the ingress client's own RestateHttpError"
)]
pub(crate) async fn await_group_notice_via_ingress(
    ingress: &RestateIngressClient,
    index_service: &str,
    group_key: &str,
    notice: &EffectGroupNotice,
) -> Result<EffectGroupNotification, crate::RestateHttpError> {
    loop {
        match ingress
            .call_lash_object_idempotent::<_, EffectGroupNotification>(
                index_service,
                group_key,
                "await_notice",
                notice,
                &notice.attachment(group_key),
            )
            .await
        {
            Ok(notification) => return Ok(notification),
            Err(error)
                if error.is_timeout()
                    && error.classification() == crate::RestateHttpErrorClass::Transient => {}
            Err(error) => return Err(error),
        }
    }
}
