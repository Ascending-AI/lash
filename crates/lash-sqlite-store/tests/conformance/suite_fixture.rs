/// A fixture flavor [`Retained`] opens from synchronous code.
trait BlockingFixture: Clone {
    fn open_on(substrate: Substrate) -> Self;
}

impl BlockingFixture for TestBackend {
    fn open_on(substrate: Substrate) -> Self {
        Self::blocking(substrate)
    }
}

/// Backends a fixture opened and must keep alive for its whole law.
#[derive(Clone)]
struct Retained<B = TestBackend>(Arc<Mutex<Vec<B>>>);

impl<B> Default for Retained<B> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
}

impl<B: Clone> Retained<B> {
    fn keep(&self, backend: &B) {
        self.0.lock_recover().push(backend.clone());
    }
}

impl<B: BlockingFixture> Retained<B> {
    /// A fresh backend, kept alive with the fixture.
    fn open_blocking(&self) -> B {
        let backend = B::open_on(SUBSTRATE);
        self.keep(&backend);
        backend
    }
}

fn root_session_request(
    session_id: &str,
) -> lash_core_store::session_store_factory_types::SessionStoreCreateRequest {
    lash_core_store::session_store_factory_types::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::fixture(session_id),
        relation: lash_core_store::session_identity::SessionRelation::Root,
        config: lash_core_store::session_graph::PersistedSessionConfig::new(
            lash_protocol_types::session_model::TurnBudget::Unbounded,
            lash_protocol_types::session_model::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ),
        head: lash_core_store::session_store_factory_types::SessionCreationHead::Config,
        retention: lash_core_store::store::Retention::UntilGc,
    }
}
