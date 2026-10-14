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
mod publisher;
pub use memory::ToolLiveMemoryWindow;
pub use publisher::ToolLivePublisher;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolLiveOmission {
    Coalesced,
    OwnerBudget,
    Oversized,
    AfterCompletion,
    StoreFailed,
    RoutesFull,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolLiveEmission {
    pub address: CallAddress,
    pub id: ToolLiveEmissionId,
    pub payload: ToolLivePayload,
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

#[derive(Clone, Debug, PartialEq)]
pub enum ToolLiveItem {
    Emission {
        cursor: ToolLiveCursor,
        emission: ToolLiveEmission,
    },
    Gap {
        cursor: ToolLiveCursor,
        reason: ToolLiveGapReason,
    },
}
impl ToolLiveItem {
    pub fn cursor(&self) -> &ToolLiveCursor {
        match self {
            Self::Emission { cursor, .. } | Self::Gap { cursor, .. } => cursor,
        }
    }
}

#[derive(Clone, Debug)]
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

#[derive(Debug, thiserror::Error)]
pub enum ToolLiveError {
    #[error("tool-live transport is unavailable")]
    Unavailable,
    #[error("tool-live cursor belongs to another owner")]
    WrongOwner,
    #[error("tool-live transport: {0}")]
    Transport(String),
}

pub type ToolLiveSubscription = BoxStream<'static, Result<ToolLiveItem, ToolLiveError>>;

/// Separate provisional event class implemented by the session and process
/// replay stores. Publication has no journal or lifecycle writer.
#[async_trait::async_trait]
pub trait ToolLiveReplay: Send + Sync {
    async fn publish(
        &self,
        owner: &ExecutionOwner,
        emissions: Vec<ToolLiveEmission>,
    ) -> Result<(), ToolLiveError>;
    async fn subscribe(
        &self,
        owner: &ExecutionOwner,
        after: Option<ToolLiveCursor>,
    ) -> Result<ToolLiveSubscription, ToolLiveError>;
}

/// Per-owner publisher admission, including the batch held by a stalled write.
/// Defaults: 64 events/100 ms, 16 logs/interval, 256 KiB pending, 64 KiB/fact,
/// 128 calls/owner. Hard maxima are 4096 events, 1024 logs, 16 MiB pending,
/// 1 MiB/fact and 4096 calls; interval is 10 ms..60 s. Writes time out at 5 s.
#[derive(Clone, Copy, Debug)]
pub struct ToolLivePolicy {
    pub(super) events: usize,
    pub(super) logs: usize,
    pub(super) bytes: usize,
    pub(super) fact_bytes: usize,
    pub(super) calls: usize,
    pub(super) interval: Duration,
}
#[derive(Debug, thiserror::Error)]
#[error("tool-live policy is outside its bounds")]
pub struct ToolLivePolicyError;
impl Default for ToolLivePolicy {
    fn default() -> Self {
        Self {
            events: 64,
            logs: 16,
            bytes: 262144,
            fact_bytes: 65536,
            calls: 128,
            interval: Duration::from_millis(100),
        }
    }
}
impl ToolLivePolicy {
    pub fn new(
        events: usize,
        logs: usize,
        bytes: usize,
        fact_bytes: usize,
        calls: usize,
        interval: Duration,
    ) -> Result<Self, ToolLivePolicyError> {
        if !(2..=4096).contains(&events)
            || logs > 1024
            || logs >= events
            || !(4096..=16777216).contains(&bytes)
            || !(1024..=1048576).contains(&fact_bytes)
            || fact_bytes > bytes / 2
            || !(1..=4096).contains(&calls)
            || !(Duration::from_millis(10)..=Duration::from_secs(60)).contains(&interval)
        {
            return Err(ToolLivePolicyError);
        }
        Ok(Self {
            events,
            logs,
            bytes,
            fact_bytes,
            calls,
            interval,
        })
    }
}

/// The tool class's retention does not consume either owner history window.
/// Defaults: 2048 events, 8 MiB and 120 s per owner, 4096 resident owners.
#[derive(Clone, Copy, Debug)]
pub struct ToolLiveRetention {
    pub(super) events: usize,
    pub(super) bytes: usize,
    pub(super) age: Duration,
    pub(super) owners: usize,
}
impl Default for ToolLiveRetention {
    fn default() -> Self {
        Self {
            events: 2048,
            bytes: 8388608,
            age: Duration::from_secs(120),
            owners: 4096,
        }
    }
}
impl ToolLiveRetention {
    pub fn events(&self) -> usize {
        self.events
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn age(&self) -> Duration {
        self.age
    }
    pub fn owners(&self) -> usize {
        self.owners
    }
    pub fn new(
        events: usize,
        bytes: usize,
        age: Duration,
        owners: usize,
    ) -> Result<Self, ToolLivePolicyError> {
        if !(1..=65536).contains(&events)
            || !(4096..=67108864).contains(&bytes)
            || !(Duration::from_millis(10)..=Duration::from_secs(3600)).contains(&age)
            || !(1..=65536).contains(&owners)
        {
            return Err(ToolLivePolicyError);
        }
        Ok(Self {
            events,
            bytes,
            age,
            owners,
        })
    }
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
