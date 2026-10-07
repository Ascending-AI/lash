use crate::support::ProcessWorkWiring;

pub(super) struct CoreWorkSetup {
    /// The backend's process work, which the core's sessions and host
    /// process APIs admit into.
    pub(super) process: ProcessWorkWiring,
}

#[derive(Clone)]
pub(crate) struct ResolvedPorts {
    pub(crate) process: ProcessWorkWiring,
}

/// Shared, lazily-initialized host-work state for a [`LashCore`].
///
/// The once-guard ([`tokio::sync::OnceCell`]) resolves the ports exactly
/// once across `LashCore` clones, on the first `session().open()` or admin
/// path that needs them.
pub(crate) struct CoreWorkSlot {
    pub(super) setup: CoreWorkSetup,
    drivers: tokio::sync::OnceCell<ResolvedPorts>,
}

impl CoreWorkSlot {
    pub(super) fn new(setup: CoreWorkSetup) -> Self {
        Self {
            setup,
            drivers: tokio::sync::OnceCell::new(),
        }
    }

    /// Idempotent: the once-guard resolves the ports once.
    pub(crate) async fn ports(&self) -> ResolvedPorts {
        self.drivers
            .get_or_init(|| async {
                ResolvedPorts {
                    process: self.setup.process.clone(),
                }
            })
            .await
            .clone()
    }
}
