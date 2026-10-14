//! Receive-side correlation and node-local publication. SDK notification tasks
//! may run after the response: retain a bounded recent token window per peer.
#![allow(
    deprecated,
    reason = "forward the MCP logging vocabulary while it remains on the wire"
)]
use lash_core::{
    AttemptContext, McpCallIdentity, McpLivePublisher, McpLogLevel, McpObservation,
    McpObservationHub, McpObservationKind, McpObservationPolicy,
};
use rmcp::{
    model::{
        LoggingLevel, LoggingMessageNotificationParam, ProgressNotificationParam, ProgressToken,
        RequestId,
    },
    service::{NotificationContext, RoleClient},
};
use std::{collections::VecDeque, sync::Arc};

pub(crate) struct Routes {
    server: String,
    server_publisher: McpLivePublisher,
    policy: McpObservationPolicy,
    calls: VecDeque<Call>,
}
struct Call {
    token: ProgressToken,
    request: RequestId,
    identity: McpCallIdentity,
    publisher: Option<McpLivePublisher>,
}

impl Routes {
    pub(crate) fn new(
        server: &str,
        hub: &Arc<McpObservationHub>,
        policy: McpObservationPolicy,
    ) -> Self {
        Self {
            server: server.into(),
            server_publisher: hub.server(server.into()),
            policy,
            calls: VecDeque::new(),
        }
    }
    pub(crate) fn register(
        &mut self,
        token: ProgressToken,
        request: RequestId,
        context: &AttemptContext<'_>,
    ) {
        while self.calls.len() >= self.policy.progress_calls() {
            self.calls.pop_front();
        }
        self.calls.push_back(Call {
            token,
            request,
            identity: McpCallIdentity {
                call_id: context.call_id().clone(),
                attempt: context.attempt_number(),
            },
            publisher: context.mcp_publisher(),
        });
    }
    pub(crate) fn progress(&self, params: ProgressNotificationParam) {
        let Some(call) = self
            .calls
            .iter()
            .find(|call| call.token == params.progress_token)
        else {
            self.server_publisher.publish(
                McpObservation::McpObservationsDropped {
                    server: self.server.clone(),
                    kind: McpObservationKind::Progress,
                    count: 1,
                },
                self.policy,
            );
            return;
        };
        if let Some(publisher) = &call.publisher {
            publisher.publish(
                McpObservation::McpProgress {
                    server: self.server.clone(),
                    call: call.identity.clone(),
                    progress: params.progress,
                    total: params.total,
                    message: params.message,
                },
                self.policy,
            );
        }
    }
    pub(crate) fn log(
        &self,
        params: LoggingMessageNotificationParam,
        context: NotificationContext<RoleClient>,
    ) {
        // rmcp extracts notification metadata into extensions on this protocol.
        let meta = context
            .extensions
            .get::<rmcp::model::NotificationMetaObject>()
            .unwrap_or(&context.meta);
        let token = meta
            .get("progressToken")
            .and_then(|value| serde_json::from_value::<ProgressToken>(value.clone()).ok());
        let request = context
            .extensions
            .get::<rmcp::service::OriginatingRequestId>()
            .map(|id| id.0.clone());
        let call = self.calls.iter().find(|call| {
            token.as_ref().is_some_and(|token| *token == call.token)
                || request
                    .as_ref()
                    .is_some_and(|request| *request == call.request)
        });
        // A message with explicit but expired correlation is a gap, never
        // mislabeled as unsolicited or attributed to another active call.
        if (token.is_some() || request.is_some()) && call.is_none() {
            self.server_publisher.publish(
                McpObservation::McpObservationsDropped {
                    server: self.server.clone(),
                    kind: McpObservationKind::Log,
                    count: 1,
                },
                self.policy,
            );
            return;
        }
        let level = match params.level {
            LoggingLevel::Debug => McpLogLevel::Debug,
            LoggingLevel::Info => McpLogLevel::Info,
            LoggingLevel::Notice => McpLogLevel::Notice,
            LoggingLevel::Warning => McpLogLevel::Warning,
            LoggingLevel::Error => McpLogLevel::Error,
            LoggingLevel::Critical => McpLogLevel::Critical,
            LoggingLevel::Alert => McpLogLevel::Alert,
            LoggingLevel::Emergency => McpLogLevel::Emergency,
        };
        let fact = McpObservation::McpLog {
            server: self.server.clone(),
            call: call.map(|call| call.identity.clone()),
            level,
            logger: params.logger,
            data: params.data,
        };
        match call {
            Some(call) => {
                if let Some(publisher) = &call.publisher {
                    publisher.publish(fact, self.policy);
                }
            }
            None => self.server_publisher.publish(fact, self.policy),
        }
    }
    pub(crate) fn changed(&self, list: lash_core::McpList) {
        self.server_publisher.publish(
            McpObservation::McpListChanged {
                server: self.server.clone(),
                list,
            },
            self.policy,
        );
    }
}
