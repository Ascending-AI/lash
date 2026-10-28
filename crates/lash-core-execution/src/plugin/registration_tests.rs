use super::*;

/// FIG-7545: a released session must not leave its allocation or owner key in
/// the weak registry. A stale view must not unregister its owner's successor.
/// The final execution composition lease also releases its inventory observation.
#[test]
fn released_sessions_drop_their_registration_without_erasing_a_successor() {
    let host = PluginHost::empty(
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(Arc::new(crate::SystemClock)),
    );
    let make = || {
        host.build_session(PluginSessionRequest::creation(
            "released-owner",
            SessionAuthorityContext::fixture(),
        ))
        .expect("defer the session")
    };
    let session = make();
    let owner = session.owner.clone();
    host.register_session(&owner, &session)
        .expect("register the session");
    let stale_view = (*session).clone();
    let capabilities = Arc::clone(&session.capabilities);
    let composition = session.composition();
    let weak = Arc::downgrade(&session);
    drop(session);
    assert!(
        weak.upgrade().is_none(),
        "the registered session was released"
    );
    assert!(
        host.sessions.lock_recover().is_empty(),
        "a released session leaves a weak allocation and owner key behind"
    );

    let successor = make();
    host.register_session(&owner, &successor)
        .expect("register the successor");
    drop(stale_view);
    drop(capabilities);
    assert_eq!(
        host.materialized_inventories.retained_slots_for_testing(),
        (1, 2),
        "a retained execution composition stays observable after its session views leave"
    );
    drop(composition);
    assert_eq!(
        host.materialized_inventories.retained_slots_for_testing(),
        (1, 1),
        "the last execution composition lease must release only its own inventory observation"
    );
    let current = host
        .session(&SessionId::from("released-owner"))
        .expect("the successor stays registered");
    assert!(Arc::ptr_eq(&current, &successor));
    drop(current);
    drop(successor);
    assert!(host.sessions.lock_recover().is_empty());
    assert_eq!(
        host.materialized_inventories.retained_slots_for_testing(),
        (0, 0)
    );
}
