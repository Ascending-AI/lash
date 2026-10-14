//! Node-local MCP facts. No journal, serialization, replay or store writes.
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

use crate::{SessionId, ToolCallId};
use futures_util::Stream;
use lash_sansio::sync::MutexExt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpCallIdentity {
    pub call_id: ToolCallId,
    pub attempt: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpLogLevel {
    Debug,
    Info,
    Notice,
    Warning,
    Error,
    Critical,
    Alert,
    Emergency,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpList {
    Tools,
    Resources,
    Prompts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpObservationKind {
    Progress,
    Log,
    ListChanged,
}

#[derive(Clone, Debug, PartialEq)]
pub enum McpObservation {
    McpProgress {
        server: String,
        call: McpCallIdentity,
        progress: f64,
        total: Option<f64>,
        message: Option<String>,
    },
    McpLog {
        server: String,
        call: Option<McpCallIdentity>,
        level: McpLogLevel,
        logger: Option<String>,
        data: serde_json::Value,
    },
    McpListChanged {
        server: String,
        list: McpList,
    },
    McpObservationsDropped {
        server: String,
        kind: McpObservationKind,
        count: u64,
    },
}

impl McpObservation {
    fn server(&self) -> &str {
        match self {
            Self::McpProgress { server, .. }
            | Self::McpLog { server, .. }
            | Self::McpListChanged { server, .. }
            | Self::McpObservationsDropped { server, .. } => server,
        }
    }
    fn kind(&self) -> McpObservationKind {
        match self {
            Self::McpProgress { .. } => McpObservationKind::Progress,
            Self::McpLog { .. } => McpObservationKind::Log,
            Self::McpListChanged { .. } => McpObservationKind::ListChanged,
            Self::McpObservationsDropped { kind, .. } => *kind,
        }
    }
    fn bytes(&self) -> usize {
        self.server().len()
            + match self {
                Self::McpProgress { call, message, .. } => {
                    call.call_id.as_str().len() + message.as_ref().map_or(0, String::len)
                }
                Self::McpLog {
                    call, logger, data, ..
                } => {
                    call.as_ref().map_or(0, |call| call.call_id.as_str().len())
                        + logger.as_ref().map_or(0, String::len)
                        + data.to_string().len()
                }
                _ => 0,
            }
    }
}

/// Bounds each observer's pending facts per server. Default: 128 logs,
/// 64 progress calls, 64 servers, 64 KiB per fact. Hard maxima: 4096 logs,
/// 1024 calls, 256 servers and 1 MiB. Excess facts produce dropped counts;
/// facts are never truncated. These limits cannot affect a call's outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct McpObservationPolicy {
    logs: usize,
    calls: usize,
    servers: usize,
    bytes: usize,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error(
    "MCP observation limits must be nonzero and at most 4096 logs, 1024 calls, 256 servers and 1048576 bytes"
)]
pub struct McpObservationPolicyError;

impl Default for McpObservationPolicy {
    fn default() -> Self {
        Self {
            logs: 128,
            calls: 64,
            servers: 64,
            bytes: 65536,
        }
    }
}
impl McpObservationPolicy {
    /// Pending progress identities and recent request tokens per server.
    pub fn progress_calls(self) -> usize {
        self.calls
    }
    pub fn new(
        logs: usize,
        calls: usize,
        servers: usize,
        bytes: usize,
    ) -> Result<Self, McpObservationPolicyError> {
        if logs == 0
            || logs > 4096
            || calls == 0
            || calls > 1024
            || servers == 0
            || servers > 256
            || bytes == 0
            || bytes > 1048576
        {
            return Err(McpObservationPolicyError);
        }
        Ok(Self {
            logs,
            calls,
            servers,
            bytes,
        })
    }
}

/// A live-only span cannot be recovered, even when its replay cursor survives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpObservationGap {
    Unavailable,
}

#[derive(Clone, Debug, PartialEq)]
pub enum McpLiveItem {
    Observation(McpObservation),
    Gap(McpObservationGap),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Address {
    Session(SessionId),
    Server(String),
}

/// Local side of the existing observation feed, kept apart from its replay
/// cache. Subscriptions own every payload: the hub keeps only weak handles.
#[derive(Debug, Default)]
pub struct McpObservationHub {
    observers: Mutex<Vec<Weak<Queue>>>,
}

#[derive(Debug)]
struct Queue {
    address: Address,
    state: Mutex<State>,
}
#[derive(Debug, Default)]
struct State {
    items: VecDeque<McpLiveItem>,
    waker: Option<Waker>,
}

#[derive(Clone)]
pub struct McpLivePublisher {
    hub: Arc<McpObservationHub>,
    address: Address,
}

impl McpObservationHub {
    pub fn session(self: &Arc<Self>, session: SessionId) -> McpLivePublisher {
        McpLivePublisher {
            hub: Arc::clone(self),
            address: Address::Session(session),
        }
    }
    pub fn server(self: &Arc<Self>, server: String) -> McpLivePublisher {
        McpLivePublisher {
            hub: Arc::clone(self),
            address: Address::Server(server),
        }
    }
    pub fn subscribe_session(self: &Arc<Self>, session: SessionId) -> McpLiveSubscription {
        self.subscribe(Address::Session(session))
    }
    pub fn subscribe_server(self: &Arc<Self>, server: String) -> McpLiveSubscription {
        self.subscribe(Address::Server(server))
    }
    fn subscribe(&self, address: Address) -> McpLiveSubscription {
        let queue = Arc::new(Queue {
            address,
            state: Mutex::new(State {
                items: VecDeque::from([McpLiveItem::Gap(McpObservationGap::Unavailable)]),
                waker: None,
            }),
        });
        let mut observers = self.observers.lock_recover();
        observers.retain(|observer| observer.strong_count() != 0);
        observers.push(Arc::downgrade(&queue));
        McpLiveSubscription { queue }
    }
}

impl McpLivePublisher {
    pub fn publish(&self, fact: McpObservation, policy: McpObservationPolicy) {
        let queues = {
            let mut observers = self.hub.observers.lock_recover();
            observers.retain(|observer| observer.strong_count() != 0);
            observers
                .iter()
                .filter_map(Weak::upgrade)
                .filter(|queue| queue.address == self.address)
                .collect::<Vec<_>>()
        };
        for queue in queues {
            let mut state = queue.state.lock_recover();
            state.push(fact.clone(), policy);
            let waker = state.waker.take();
            drop(state);
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
}

impl State {
    fn push(&mut self, fact: McpObservation, policy: McpObservationPolicy) {
        let server = fact.server();
        let kind = fact.kind();
        let mut servers = std::collections::BTreeSet::new();
        for item in &self.items {
            if let McpLiveItem::Observation(fact) = item
                && !fact.server().is_empty()
            {
                servers.insert(fact.server());
            }
        }
        // Keep one bounded, anonymous omission counter for excess server keys.
        if !servers.contains(server) && servers.len() >= policy.servers {
            self.dropped_count(
                "",
                kind,
                match fact {
                    McpObservation::McpObservationsDropped { count, .. } => count,
                    _ => 1,
                },
            );
            return;
        }
        if fact.bytes() > policy.bytes {
            self.dropped("", kind);
            return;
        }
        if let McpObservation::McpObservationsDropped {
            server,
            kind,
            count,
        } = fact
        {
            self.dropped_count(&server, kind, count);
            return;
        }
        if let Some(item) = self.items.iter_mut().find(|item| match (item, &fact) {
            (
                McpLiveItem::Observation(McpObservation::McpProgress {
                    server: a, call: b, ..
                }),
                McpObservation::McpProgress { server, call, .. },
            ) => a == server && b == call,
            (
                McpLiveItem::Observation(McpObservation::McpListChanged { server: a, list: b }),
                McpObservation::McpListChanged { server, list },
            ) => a == server && b == list,
            _ => false,
        }) {
            *item = McpLiveItem::Observation(fact);
            return;
        }
        let count = self.items.iter().filter(|item| matches!(item, McpLiveItem::Observation(other) if other.server() == server && other.kind() == kind && !matches!(other, McpObservation::McpObservationsDropped { .. }))).count();
        let limit = match kind {
            McpObservationKind::Log => policy.logs,
            McpObservationKind::Progress => policy.calls,
            McpObservationKind::ListChanged => 3,
        };
        if count >= limit {
            self.dropped(server, kind);
            return;
        }
        self.items.push_back(McpLiveItem::Observation(fact));
    }
    fn dropped(&mut self, server: &str, kind: McpObservationKind) {
        self.dropped_count(server, kind, 1);
    }
    fn dropped_count(&mut self, server: &str, kind: McpObservationKind, amount: u64) {
        for item in &mut self.items {
            if let McpLiveItem::Observation(McpObservation::McpObservationsDropped {
                server: existing,
                kind: existing_kind,
                count,
            }) = item
                && existing == server
                && *existing_kind == kind
            {
                *count = count.saturating_add(amount);
                return;
            }
        }
        self.items.push_back(McpLiveItem::Observation(
            McpObservation::McpObservationsDropped {
                server: server.into(),
                kind,
                count: amount,
            },
        ));
    }
}

pub struct McpLiveSubscription {
    queue: Arc<Queue>,
}
impl Stream for McpLiveSubscription {
    type Item = McpLiveItem;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut state = self.queue.state.lock_recover();
        if let Some(item) = state.items.pop_front() {
            return Poll::Ready(Some(item));
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}
