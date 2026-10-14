//! Connection-local request correlation. Live routes are pinned until their
//! response returns; only ended routes may be evicted from the grace window.
#![allow(deprecated, reason = "MCP logging remains a wire notification")]
use lash_core::{
    AttemptContext, ToolLiveFact, ToolLiveLogLevel, ToolLiveOmission, ToolLivePublisher,
};
use rmcp::{
    model::{
        LoggingLevel, LoggingMessageNotificationParam, ProgressNotificationParam, ProgressToken,
        RequestId,
    },
    service::{NotificationContext, RoleClient},
};
use std::{collections::VecDeque, time::Duration};

/// Independent correlation bound: default 128 routes and a two-second grace.
/// Hard maxima: 4096 routes and 60 seconds. Live calls are never evicted.
#[derive(Clone, Copy, Debug)]
pub struct McpRoutePolicy {
    routes: usize,
    grace: Duration,
}
#[derive(Debug, thiserror::Error)]
#[error("MCP route policy is outside its bounds")]
pub struct McpRoutePolicyError;
impl Default for McpRoutePolicy {
    fn default() -> Self {
        Self {
            routes: 128,
            grace: Duration::from_secs(2),
        }
    }
}
impl McpRoutePolicy {
    pub fn new(routes: usize, grace: Duration) -> Result<Self, McpRoutePolicyError> {
        if !(1..=4096).contains(&routes) || grace > Duration::from_secs(60) {
            return Err(McpRoutePolicyError);
        }
        Ok(Self { routes, grace })
    }
}
pub(crate) struct Routes {
    server: String,
    policy: McpRoutePolicy,
    calls: VecDeque<Call>,
}
struct Call {
    token: ProgressToken,
    request: RequestId,
    publisher: ToolLivePublisher,
    ended: Option<tokio::time::Instant>,
    progress: Option<f64>,
}
impl Routes {
    pub(crate) fn new(server: &str, policy: McpRoutePolicy) -> Self {
        Self {
            server: server.into(),
            policy,
            calls: VecDeque::new(),
        }
    }
    pub(crate) fn register(
        &mut self,
        token: ProgressToken,
        request: RequestId,
        context: &AttemptContext<'_>,
    ) -> bool {
        self.calls.retain(|call| {
            call.ended
                .is_none_or(|ended| ended.elapsed() <= self.policy.grace)
        });
        while self.calls.len() >= self.policy.routes {
            let Some(index) = self.calls.iter().position(|call| call.ended.is_some()) else {
                context.live().omit(ToolLiveOmission::RoutesFull);
                return false;
            };
            self.calls.remove(index);
        }
        self.calls.push_back(Call {
            token,
            request,
            publisher: context.live(),
            ended: None,
            progress: None,
        });
        true
    }
    pub(crate) fn end(&mut self, request: &RequestId) {
        if let Some(call) = self.calls.iter_mut().find(|call| &call.request == request) {
            call.ended = Some(tokio::time::Instant::now());
        }
    }
    pub(crate) fn progress(&mut self, params: ProgressNotificationParam) {
        let Some(call) = self
            .calls
            .iter_mut()
            .find(|call| call.token == params.progress_token)
        else {
            tracing::debug!(server = %self.server, "unrouted MCP progress");
            return;
        };
        // SDK notifications are spawned independently. MCP's increasing
        // progress contract makes the latest gauge independent of task order.
        if call.progress.is_some_and(|value| value > params.progress) {
            call.publisher.omit(ToolLiveOmission::Coalesced);
            return;
        }
        call.progress = Some(params.progress);
        call.publisher.publish(ToolLiveFact::Progress {
            progress: params.progress,
            total: params.total,
            message: params.message,
        });
    }
    pub(crate) fn log(
        &self,
        params: LoggingMessageNotificationParam,
        context: NotificationContext<RoleClient>,
    ) {
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
        let Some(call) = call else {
            tracing::info!(server = %self.server, level = ?params.level, logger = ?params.logger, data = %params.data, "MCP server log");
            return;
        };
        let level = match params.level {
            LoggingLevel::Debug => ToolLiveLogLevel::Debug,
            LoggingLevel::Info => ToolLiveLogLevel::Info,
            LoggingLevel::Notice => ToolLiveLogLevel::Notice,
            LoggingLevel::Warning => ToolLiveLogLevel::Warning,
            LoggingLevel::Error => ToolLiveLogLevel::Error,
            LoggingLevel::Critical => ToolLiveLogLevel::Critical,
            LoggingLevel::Alert => ToolLiveLogLevel::Alert,
            LoggingLevel::Emergency => ToolLiveLogLevel::Emergency,
        };
        call.publisher.publish(ToolLiveFact::Log {
            level,
            logger: params.logger,
            data: params.data,
        });
    }
}

pub(crate) struct RouteLease {
    routes: std::sync::Arc<tokio::sync::Mutex<Routes>>,
    request: RequestId,
    armed: bool,
}
impl RouteLease {
    pub(crate) fn new(
        routes: std::sync::Arc<tokio::sync::Mutex<Routes>>,
        request: RequestId,
    ) -> Self {
        Self {
            routes,
            request,
            armed: true,
        }
    }
    pub(crate) async fn finish(&mut self) {
        self.routes.lock().await.end(&self.request);
        self.armed = false;
    }
}
impl Drop for RouteLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut routes) = self.routes.try_lock() {
            routes.end(&self.request);
            return;
        }
        let routes = self.routes.clone();
        let request = self.request.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                routes.lock().await.end(&request);
            });
        }
    }
}
