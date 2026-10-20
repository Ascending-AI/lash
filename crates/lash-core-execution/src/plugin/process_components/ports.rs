//! Host services and the ports each captured component may read.
use super::*;

/// The host services every component of a deployment builds against.
#[derive(Clone)]
pub struct ProcessComponentHost {
    pub tracing: crate::trace::TraceRuntime,
    pub extensions: PluginExtensions,
    pub sessions: Option<Arc<dyn ProcessSessionReads>>,
}

impl ProcessComponentHost {
    pub fn new(tracing: crate::trace::TraceRuntime) -> Self {
        Self {
            tracing,
            extensions: PluginExtensions::default(),
            sessions: None,
        }
    }

    #[must_use]
    pub fn with_session_reads(mut self, sessions: Arc<dyn ProcessSessionReads>) -> Self {
        self.sessions = Some(sessions);
        self
    }

    /// The ports `entry` grants.
    pub fn ports(&self, entry: &ProcessComponentEntry) -> ProcessComponentPorts {
        ProcessComponentPorts {
            tracing: self.tracing.clone(),
            extensions: self.extensions.clone(),
            granted: entry.sessions().clone(),
            reads: self.sessions.clone(),
        }
    }
}

/// The live services a component binds lazily.
pub struct ProcessComponentPorts {
    pub tracing: crate::trace::TraceRuntime,
    pub extensions: PluginExtensions,
    granted: BTreeSet<SessionId>,
    reads: Option<Arc<dyn ProcessSessionReads>>,
}

impl ProcessComponentPorts {
    /// A read service for `session` when this component's env entry grants
    /// it (D17).
    ///
    /// # Errors
    ///
    /// The entry does not grant `session`, or the host serves no session
    /// reads.
    pub fn session(&self, session: &SessionId) -> Result<Arc<dyn SessionReadService>, PluginError> {
        if !self.granted.contains(session) {
            return Err(PluginError::Session(format!(
                "this component's environment entry grants no read of session `{session}`"
            )));
        }
        let Some(reads) = self.reads.as_ref() else {
            return Err(PluginError::Session(
                "this deployment serves no session reads to process components".into(),
            ));
        };
        reads.session(session)
    }
}
