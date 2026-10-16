//! A bound extension capability for a test that calls a plugin callback
//! directly, outside the runtime that would bind it.

use crate::store::plugin_writers::PluginCallbackIdentity;
use crate::trace::{BodyTrace, ExtensionTrace, TraceRuntime};
use std::sync::{Arc, Mutex};

/// The capability of one run of a registered callback under a fresh,
/// observed turn scope of a runtime, and the guard that revokes it.
pub struct BoundExtensionTrace {
    trace: ExtensionTrace,
    body: BodyTrace,
    annotations: Option<Arc<Mutex<lash_trace::ToolSpanAnnotations>>>,
}

impl BoundExtensionTrace {
    /// The capability the callback is handed.
    pub fn trace(&self) -> ExtensionTrace {
        self.trace.clone()
    }

    /// The callback returned: what is still open closes `body_exit`.
    pub fn finish(self) {
        self.body.finish();
    }

    /// `attempt`, handed this capability as a tool body's.
    pub fn on<'run>(&self, attempt: crate::AttemptContext<'run>) -> crate::AttemptContext<'run> {
        attempt.with_trace(self.trace.clone())
    }

    /// What the body set on its own tool span so far.
    pub fn tool_span_annotations(&self) -> lash_trace::ToolSpanAnnotations {
        self.annotations
            .as_ref()
            .map(|annotations| {
                annotations
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            })
            .unwrap_or_default()
    }
}

/// A new observed turn scope of `runtime`, under a fresh producer.
fn turn_scope(runtime: &TraceRuntime) -> lash_trace::DurableTraceScope {
    let producer = runtime.open_producer(
        lash_trace::ProducerKind::Send,
        &lash_trace::TraceCall::default(),
        lash_trace::TraceIds::default(),
    );
    lash_trace::DurableTraceScope::admit(
        lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::Turn {
            session_id: crate::SessionId::from("extension-test"),
            turn_id: crate::TurnId::from("extension-test-turn"),
        }),
        producer.parent().as_ref(),
        lash_trace::TraceLinks::new(),
        runtime.clock().timestamp_ms(),
    )
}

/// Binds a capability as the registered `callback` under a new observed turn
/// scope of `runtime`, as the runtime binds one before it runs a hook.
pub fn bound_extension_trace(
    runtime: &TraceRuntime,
    callback: &PluginCallbackIdentity,
) -> BoundExtensionTrace {
    let (trace, body) = ExtensionTrace::seed(
        runtime.unreplayed(Some(turn_scope(runtime))),
        lash_trace::TraceContext::default(),
    )
    .for_callback(callback);
    BoundExtensionTrace {
        trace,
        body,
        annotations: None,
    }
}

/// Binds a tool body's capability as the registered provider `callback`
/// under a new observed turn scope of `runtime`, with its own tool span's
/// annotations, as the runtime binds one before it runs the body.
pub fn bound_tool_trace(
    runtime: &TraceRuntime,
    callback: &PluginCallbackIdentity,
) -> BoundExtensionTrace {
    let annotations = Arc::new(Mutex::new(lash_trace::ToolSpanAnnotations::default()));
    let mut spec = crate::trace::BodySpec::at(
        runtime.unreplayed(Some(turn_scope(runtime))),
        lash_trace::TraceContext::default(),
    );
    spec.callback = crate::trace::BoundCallback::of(callback);
    spec.tool_span = Some(Arc::clone(&annotations));
    let (trace, body) = spec.bind();
    BoundExtensionTrace {
        trace,
        body,
        annotations: Some(annotations),
    }
}
