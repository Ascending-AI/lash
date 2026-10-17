//! Bounded, execution-owned observations outside recorded attempt streams.
use crate::{ExecutionOwner, ToolCallId};
use futures_util::{StreamExt, stream::BoxStream};
use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

mod memory;
mod policy;
mod publisher;
pub use memory::ToolLiveMemoryWindow;
pub use policy::{
    TOOL_LIVE_ENVELOPE_BYTES, ToolLivePolicy, ToolLivePolicyBound, ToolLivePolicyError,
    ToolLivePolicyField, ToolLivePolicyRelation, ToolLivePolicySnapshot, ToolLivePolicyValue,
    ToolLiveRetention,
};
pub use publisher::{ToolLiveDiagnostics, ToolLivePublisher};

/// Immutable address of one admitted attempt. Routing is never inferred from
/// a plugin name, an observer, or a server notification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallAddress {
    pub owner: ExecutionOwner,
    pub call_id: ToolCallId,
    pub attempt: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolLiveLogLevel {
    Debug,
    Info,
    Notice,
    Warning,
    Error,
    Critical,
    Alert,
    Emergency,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolLiveFact {
    Progress {
        progress: f64,
        total: Option<f64>,
        message: Option<String>,
    },
    Log {
        level: ToolLiveLogLevel,
        logger: Option<String>,
        data: serde_json::Value,
    },
}

/// Why facts were counted instead of delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolLiveOmission {
    /// A newer progress value replaced one no write had carried yet.
    Coalesced,
    /// A log found the owner's log budget full.
    OwnerBudget,
    /// A fact over `fact_bytes`, or a progress record over `progress_bytes`.
    Oversized,
    /// Publication after the call's completion fence.
    AfterCompletion,
    /// Logs a failed or timed-out store write lost. Counted when the write
    /// fails and delivered with the owner's next successful write.
    StoreFailed,
    /// Call-scoped: the MCP plugin had no correlation route for the call.
    /// Owner-scoped: a call of this owner got no publisher slot, so nothing
    /// it publishes is delivered.
    RoutesFull,
    /// A progress or total that is NaN or infinite; never stored.
    NonFinite,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolLivePayload {
    Fact { fact: ToolLiveFact },
    Omitted { cause: ToolLiveOmission, count: u64 },
}

/// Emission identity is independent of the call address and replay position.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolLiveEmissionId {
    pub producer_epoch: String,
    pub ordinal: u64,
}

/// One fact, or one count of omitted facts, about one admitted call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolLiveEmission {
    pub address: CallAddress,
    pub id: ToolLiveEmissionId,
    pub payload: ToolLivePayload,
}

/// Omissions no admitted call's address can carry: calls refused a
/// publisher slot (`RoutesFull`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolLiveOwnerOmission {
    pub owner: ExecutionOwner,
    pub id: ToolLiveEmissionId,
    pub cause: ToolLiveOmission,
    pub count: u64,
}

/// The unit a publisher writes and a replay store retains.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum ToolLiveRecord {
    Call(ToolLiveEmission),
    Owner(ToolLiveOwnerOmission),
}
impl ToolLiveRecord {
    pub fn id(&self) -> &ToolLiveEmissionId {
        match self {
            Self::Call(emission) => &emission.id,
            Self::Owner(omission) => &omission.id,
        }
    }
    /// The subscription item at `cursor`.
    pub fn into_item(self, cursor: ToolLiveCursor) -> ToolLiveItem {
        match self {
            Self::Call(emission) => ToolLiveItem::Emission { cursor, emission },
            Self::Owner(omission) => ToolLiveItem::OwnerOmitted { cursor, omission },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolLiveCursor {
    pub owner: ExecutionOwner,
    pub incarnation: String,
    pub position: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolLiveGapReason {
    Trimmed,
    Unavailable,
}

/// A retained record the subscription could not read. The feed continues
/// after it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolLiveReadFault {
    /// The stored payload does not decode as a record.
    CorruptPayload { detail: String },
}

/// One subscription item. Call filters receive their call's emissions and
/// every owner-level item (owner omissions, faults and gaps).
///
/// Its Serde JSON is a stable host wire contract: internally tagged by `kind`,
/// with snake_case variant names and typed payloads. Hosts may receive this
/// readable JSON directly, paired with [`ToolLiveFilter`] requests.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolLiveItem {
    Emission {
        cursor: ToolLiveCursor,
        emission: ToolLiveEmission,
    },
    OwnerOmitted {
        cursor: ToolLiveCursor,
        omission: ToolLiveOwnerOmission,
    },
    Fault {
        cursor: ToolLiveCursor,
        fault: ToolLiveReadFault,
    },
    Gap {
        cursor: ToolLiveCursor,
        reason: ToolLiveGapReason,
    },
}
impl ToolLiveItem {
    pub fn cursor(&self) -> &ToolLiveCursor {
        match self {
            Self::Emission { cursor, .. }
            | Self::OwnerOmitted { cursor, .. }
            | Self::Fault { cursor, .. }
            | Self::Gap { cursor, .. } => cursor,
        }
    }
}

/// What a subscription reads: every item of one execution owner, or one
/// call's emissions with the owner-level items.
///
/// Its Serde JSON is a stable host wire contract: internally tagged by `kind`
/// (`"owner"` or `"call"`), with typed payloads. Hosts may send this readable
/// JSON directly and receive [`ToolLiveItem`] without a wire mirror.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolLiveFilter {
    Owner(ExecutionOwner),
    Call(CallAddress),
}
impl ToolLiveFilter {
    pub fn owner(&self) -> &ExecutionOwner {
        match self {
            Self::Owner(owner) => owner,
            Self::Call(call) => &call.owner,
        }
    }
    fn admits(&self, item: &ToolLiveItem) -> bool {
        match (self, item) {
            (Self::Call(call), ToolLiveItem::Emission { emission, .. }) => {
                call == &emission.address
            }
            _ => true,
        }
    }
}

/// Boxed source of a transport fault.
pub type ToolLiveFaultSource = Box<dyn std::error::Error + Send + Sync>;

/// Why a subscription could not start or ended. Loss inside a working
/// subscription is an item instead: a `Gap`, a `Fault`, or an `Omitted` count.
#[derive(Debug, thiserror::Error)]
pub enum ToolLiveError {
    /// No transport is configured for the owner's kind.
    #[error("tool-live transport is unavailable")]
    Unavailable,
    #[error("tool-live cursor belongs to another owner")]
    WrongOwner,
    /// The store refused or could not be reached. Reconnect with
    /// `subscribe_after` from the last delivered cursor.
    #[error("tool-live store is unavailable")]
    StoreUnavailable(#[source] ToolLiveFaultSource),
    /// A record could not be encoded for the store.
    #[error("tool-live payload is corrupt")]
    CorruptPayload(#[source] ToolLiveFaultSource),
    /// The subscription's change notifications ended (the listener stopped,
    /// or the owner's window was reclaimed). Reconnect from the last cursor;
    /// the class reports a `Gap` if what it held is gone.
    #[error("tool-live notifications were lost")]
    NotificationLost,
}
impl ToolLiveError {
    pub fn store(source: impl Into<ToolLiveFaultSource>) -> Self {
        Self::StoreUnavailable(source.into())
    }
    pub fn corrupt(source: impl Into<ToolLiveFaultSource>) -> Self {
        Self::CorruptPayload(source.into())
    }
}

pub type ToolLiveSubscription = BoxStream<'static, Result<ToolLiveItem, ToolLiveError>>;

/// Separate provisional event class implemented by the session and process
/// replay stores. Publication has no journal or lifecycle writer.
#[async_trait::async_trait]
pub trait ToolLiveReplay: Send + Sync {
    async fn publish(
        &self,
        owner: &ExecutionOwner,
        records: Vec<ToolLiveRecord>,
    ) -> Result<(), ToolLiveError>;
    async fn subscribe(
        &self,
        owner: &ExecutionOwner,
        after: Option<ToolLiveCursor>,
    ) -> Result<ToolLiveSubscription, ToolLiveError>;
}

/// Core's host seam, with transport through the owning replay store.
pub struct ToolLive {
    session: Option<Arc<dyn ToolLiveReplay>>,
    process: Option<Arc<dyn ToolLiveReplay>>,
    policy: ToolLivePolicy,
    owners: Mutex<HashMap<String, Weak<publisher::Owner>>>,
}
impl Default for ToolLive {
    fn default() -> Self {
        Self::new(None, None, ToolLivePolicy::default())
    }
}
impl ToolLive {
    pub fn new(
        session: Option<Arc<dyn ToolLiveReplay>>,
        process: Option<Arc<dyn ToolLiveReplay>>,
        policy: ToolLivePolicy,
    ) -> Self {
        Self {
            session,
            process,
            policy,
            owners: Mutex::default(),
        }
    }
    fn transport(&self, owner: &ExecutionOwner) -> Option<Arc<dyn ToolLiveReplay>> {
        match owner {
            ExecutionOwner::SessionFrame { .. } => self.session.clone(),
            ExecutionOwner::Process { .. } => self.process.clone(),
        }
    }
    pub async fn subscribe(
        &self,
        filter: ToolLiveFilter,
    ) -> Result<ToolLiveSubscription, ToolLiveError> {
        self.subscribe_after(filter, None).await
    }
    pub async fn subscribe_after(
        &self,
        filter: ToolLiveFilter,
        after: Option<ToolLiveCursor>,
    ) -> Result<ToolLiveSubscription, ToolLiveError> {
        if after
            .as_ref()
            .is_some_and(|cursor| &cursor.owner != filter.owner())
        {
            return Err(ToolLiveError::WrongOwner);
        }
        let stream = self
            .transport(filter.owner())
            .ok_or(ToolLiveError::Unavailable)?
            .subscribe(filter.owner(), after)
            .await?;
        Ok(stream
            .filter(move |item| {
                std::future::ready(item.as_ref().map_or(true, |item| filter.admits(item)))
            })
            .boxed())
    }
    #[cfg(any(test, feature = "testing"))]
    pub fn publisher_for_test(&self, address: CallAddress) -> ToolLivePublisher {
        self.bind(address)
    }
    pub fn policy(&self) -> &ToolLivePolicy {
        &self.policy
    }
    /// The owner's publisher on this node, while any call of it holds a
    /// publisher or its writes are still draining; `None` otherwise.
    pub fn diagnostics(&self, owner: &ExecutionOwner) -> Option<ToolLiveDiagnostics> {
        let key = serde_json::to_string(owner).unwrap_or_default();
        let owner = self
            .owners
            .lock_recover()
            .get(&key)
            .and_then(Weak::upgrade)?;
        Some(owner.diagnostics())
    }

    /// Runtime admission binds the address once; tools receive only this
    /// sealed handle, which cannot publish for another call.
    pub(crate) fn bind(&self, address: CallAddress) -> ToolLivePublisher {
        let key = serde_json::to_string(&address.owner).unwrap_or_default();
        let mut owners = self.owners.lock_recover();
        owners.retain(|_, owner| owner.strong_count() != 0);
        let owner = owners.get(&key).and_then(Weak::upgrade).unwrap_or_else(|| {
            let owner = publisher::Owner::new(
                address.owner.clone(),
                self.transport(&address.owner),
                self.policy,
            );
            owners.insert(key, Arc::downgrade(&owner));
            owner
        });
        owner.bind(address)
    }
}

#[cfg(test)]
#[path = "tool_live/wire_laws.rs"]
mod wire_laws;
