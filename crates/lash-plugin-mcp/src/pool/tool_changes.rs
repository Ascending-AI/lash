//! One acknowledged tool-change subscription, with a coalesced dirty signal.
//!
//! Route notifications through the SDK handler seam rather than its buffered
//! Subscription helper: a change storm must not exhaust a notification queue
//! while the actor is waiting on bounded catalog discovery.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, Weak};

use lash_sansio::sync::MutexExt;
use rmcp::model::{
    ClientRequest, RequestId, ServerResult, SubscriptionFilter,
    SubscriptionsAcknowledgedNotificationParams, SubscriptionsListenRequest,
    SubscriptionsListenRequestParams,
};
use rmcp::service::{Peer, PeerRequestOptions, RoleClient};
use tokio::sync::watch;

use super::McpEntry;

#[derive(Default)]
struct State {
    expected: Option<RequestId>,
    acknowledged: Option<RequestId>,
    dirty: Option<RequestId>,
}

pub(crate) struct ToolChanges {
    entry: Weak<McpEntry>,
    generation: u64,
    state: Mutex<State>,
    ready: watch::Sender<bool>,
}

pub(super) struct ToolSubscription {
    pub(super) waiting:
        Pin<Box<dyn Future<Output = Result<ServerResult, rmcp::ServiceError>> + Send>>,
}

impl ToolChanges {
    pub(super) fn new(entry: Weak<McpEntry>, generation: u64) -> Self {
        Self {
            entry,
            generation,
            state: Mutex::new(State::default()),
            ready: watch::channel(false).0,
        }
    }

    pub(super) async fn subscribe(
        &self,
        peer: &Peer<RoleClient>,
    ) -> Result<ToolSubscription, rmcp::ServiceError> {
        let mut filter = SubscriptionFilter::new();
        filter.tools_list_changed = Some(true);
        let handle = peer
            .send_cancellable_request(
                ClientRequest::SubscriptionsListenRequest(SubscriptionsListenRequest::new(
                    SubscriptionsListenRequestParams::new(filter),
                )),
                PeerRequestOptions::no_options(),
            )
            .await?;
        {
            let mut state = self.state.lock_recover();
            state.expected = Some(handle.id.clone());

            self.publish(&state);
        }
        let mut ready = self.ready.subscribe();
        let mut waiting = Box::pin(handle.await_response());
        tokio::select! {
            ready = ready.wait_for(|ready| *ready) => { ready.map_err(|_| rmcp::ServiceError::TransportClosed)?; }
            result = &mut waiting => { result?; return Err(rmcp::ServiceError::UnexpectedResponse); }
        }
        Ok(ToolSubscription { waiting })
    }

    pub(crate) fn acknowledge(
        &self,
        params: SubscriptionsAcknowledgedNotificationParams,
        id: Option<RequestId>,
    ) {
        if params.notifications.tools_list_changed != Some(true) {
            return;
        }
        let Some(id) = id else {
            return;
        };
        let mut state = self.state.lock_recover();
        if state
            .expected
            .as_ref()
            .is_some_and(|expected| expected != &id)
        {
            return;
        }
        state.acknowledged = Some(id);
        self.publish(&state);
    }

    pub(crate) fn changed(&self, id: Option<RequestId>) {
        let Some(id) = id else {
            return;
        };
        let mut state = self.state.lock_recover();
        state.dirty = Some(id);
        self.publish(&state);
    }

    fn publish(&self, state: &State) {
        let ready = state.expected.is_some() && state.expected == state.acknowledged;
        if ready {
            self.ready.send_replace(true);
            if state.dirty == state.expected
                && let Some(entry) = self.entry.upgrade()
            {
                entry.request_tool_refresh(self.generation);
            }
        }
    }
}
