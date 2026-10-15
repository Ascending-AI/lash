//! Host consent, selection, redaction and bounds for all trace consumers.
use super::LashCoreBuilder;
use std::sync::Arc;

impl LashCoreBuilder {
    /// State whether built-in telemetry carries content: prompts, responses,
    /// rendered instructions, tool arguments and results, executed code and
    /// diagnostic or provider text. One choice governs every record sink and
    /// the telemetry adapter. Defaults to
    /// [`crate::tracing::ContentPolicy::standard`] (omitted): records keep
    /// identities, statuses and counts. It never changes what a session
    /// stores or returns.
    pub fn content_policy(mut self, content: crate::tracing::ContentPolicy) -> Self {
        self.content_policy = Some(content);
        self
    }
    /// Narrows content or optional leaves before building, keyed by stable ids.
    pub fn capture_selector(mut self, selector: Arc<dyn crate::tracing::CaptureSelector>) -> Self {
        self.capture_selector = Some(selector);
        self
    }
    /// Redacts or offloads captured content before every sink, transactionally.
    pub fn content_hook(mut self, hook: Arc<dyn crate::tracing::ContentHook>) -> Self {
        self.content_hook = Some(hook);
        self
    }

    /// Select the bounds on the content a record carries. Defaults to
    /// [`ContentLimits::standard`](crate::tracing::ContentLimits::standard).
    pub fn content_limits(mut self, limits: crate::tracing::ContentLimits) -> Self {
        self.content_limits = Some(limits);
        self
    }
}
