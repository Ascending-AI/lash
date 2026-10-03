use super::*;
use pretty_assertions::assert_eq;

/// FIG-4859/L4 at a real persisted checkpoint boundary: a namespace recorded
/// at the plugin's native format but holding a payload outside the store's
/// invariants is corruption the format stamp cannot excuse. Rematerialization
/// refuses it typed — a state error, never a format one — before the factory
/// builds or any callback runs, and the durable record is untouched.
#[expect(
    clippy::unwrap_used,
    reason = "the corrupt-boundary law asserts every persisted fixture and commit"
)]
pub(super) async fn plugin_state_corrupt_boundary(store: Arc<dyn RuntimeStore>, session_id: &str) {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut factories = crate::testing::test_standard_protocol_factories();
    factories.push(Arc::new(FormatPlugin(calls.clone())));
    let host = crate::PluginHost::new(factories);
    let mut state = RuntimeSessionState {
        session_id: crate::SessionId::fixture(session_id),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    let seed = host
        .build_session(PluginSessionRequest::creation(
            "corrupt-seed",
            Default::default(),
        ))
        .unwrap();
    let mut snapshot = seed.export_state();
    // `bad key` fails `validate_key` at any stamp; recorded at the native
    // format so no migration runs and the payload itself is what refuses.
    snapshot.plugins.insert(
        "format-state".into(),
        lash_core::PluginNamespaceState {
            format_version: lash_core::FormatVersion::new(2).unwrap(),
            generation: 3,
            values: std::collections::BTreeMap::from([("bad key".into(), serde_json::json!(17))]),
        },
    );
    state.set_plugin_state(Some(snapshot));
    // The fleet record permits the stamp the corrupted namespace carries: the
    // refusal must come from the payload, not the range.
    store
        .provision_plugin_writers(&[crate::store::plugin_writers::PluginWriterRegistration {
            plugin: "format-state".into(),
            native: lash_core::FormatVersion::new(2).unwrap(),
            writable: (1..=2)
                .map(|format| lash_core::FormatVersion::new(format).unwrap())
                .collect(),
        }])
        .await
        .unwrap();
    commit(&store, &mut state).await;

    let durable =
        crate::conformance::helpers::load_window_state(&store, &SessionId::fixture(session_id))
            .await
            .unwrap()
            .unwrap();
    let original = durable.plugin_state().unwrap().clone();
    let bytes = rmp_serde::to_vec_named(&original).unwrap();
    let original_head = durable.head_revision;
    calls.store(0, std::sync::atomic::Ordering::SeqCst);

    let result = host
        .isolated_registry()
        .build_session(PluginSessionRequest::rematerialization(
            SessionId::fixture(session_id),
            &original,
            SessionAuthorityContext {
                plugin_config: durable.admitted_plugin_config(),
                ..Default::default()
            },
        ));
    let Err(error) = result else {
        panic!("a malformed native-format payload must refuse");
    };
    assert!(
        matches!(
            error,
            PluginError::State(PluginStateError::InvalidKey { .. })
        ),
        "the refusal is a typed state error: {error}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no factory build, registration or readiness ran"
    );
    let after =
        crate::conformance::helpers::load_window_state(&store, &SessionId::fixture(session_id))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(after.head_revision, original_head);
    assert_eq!(
        rmp_serde::to_vec_named(after.plugin_state().unwrap()).unwrap(),
        bytes,
        "a refused rematerialization leaves the durable record untouched"
    );
}
