use super::*;

lash_conformance::session_close_tests!({
    let backend = TestEngineBackend::open(SUBSTRATE).await;
    let host = backend.effect_host() as Arc<dyn EffectHost>;
    let stores = backend.as_stores();
    (backend, "sqlite-session-close", host, stores, None)
});
