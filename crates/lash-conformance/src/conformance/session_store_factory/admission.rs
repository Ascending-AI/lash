//! Session admission contract: create, rebind, and the durable relation the
//! rebind is checked against.

use super::*;
use pretty_assertions::assert_eq;

pub(super) async fn session_admission_contract(factory: Arc<dyn crate::SessionStoreFactory>) {
    let request = session_store_request(
        &SessionId::from("admission-created"),
        "admission-model",
        crate::SessionRelation::Child {
            parent_session_id: SessionId::from("admission-parent"),
            caused_by: None,
        },
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create explicitly bound admission store");
    let empty = crate::SessionBinding::root("");
    assert!(matches!(
        store
            .admit_and_bind_session(&empty)
            .await
            .expect_err("empty session id must be rejected"),
        crate::StoreError::InvalidSessionId { .. }
    ));

    let binding = crate::SessionBinding {
        session_id: request.session_id.clone(),
        relation: request.relation.clone(),
    };
    assert_eq!(
        store
            .admit_and_bind_session(&binding)
            .await
            .expect("admit factory-created session"),
        crate::SessionAdmission::Rebound
    );
    let created_meta = store
        .load_session_meta()
        .await
        .expect("load admitted metadata")
        .expect("admission must durably materialize metadata");
    assert_eq!(created_meta.session_id, binding.session_id);
    assert_eq!(created_meta.relation, binding.relation);

    let changed_binding = crate::SessionBinding {
        relation: crate::SessionRelation::Root,
        ..binding.clone()
    };
    assert_eq!(
        store
            .admit_and_bind_session(&changed_binding)
            .await
            .expect("rebind same session"),
        crate::SessionAdmission::Rebound
    );
    assert_eq!(
        store
            .load_session_meta()
            .await
            .expect("reload rebound metadata")
            .expect("rebound metadata"),
        created_meta,
        "rebind must preserve the original durable binding"
    );

    // A rebind that declares a *different* lineage is refused rather than
    // absorbed: the recorded relation is a durable fact (FIG-1559).
    let conflicting_binding = crate::SessionBinding {
        session_id: request.session_id.clone(),
        relation: crate::SessionRelation::Child {
            parent_session_id: SessionId::from("admission-other-parent"),
            caused_by: None,
        },
    };
    assert!(matches!(
        store
            .admit_and_bind_session(&conflicting_binding)
            .await
            .expect_err("rebind naming a different parent must be refused"),
        crate::StoreError::SessionRelationMismatch { .. }
    ));
    let forked_binding = crate::SessionBinding {
        session_id: request.session_id.clone(),
        relation: crate::SessionRelation::Fork {
            source_session_id: SessionId::from("admission-parent"),
            source_node_id: "admission-node".to_string(),
            observer_inheritance: crate::ObserverInheritance::None,
        },
    };
    assert!(matches!(
        store
            .admit_and_bind_session(&forked_binding)
            .await
            .expect_err("rebind changing the relation kind must be refused"),
        crate::StoreError::SessionRelationMismatch { .. }
    ));
    assert_eq!(
        store
            .load_session_meta()
            .await
            .expect("reload metadata after refused rebind")
            .expect("refused rebind metadata"),
        created_meta,
        "a refused rebind must leave the durable relation unchanged"
    );

    let cross_session = crate::SessionBinding {
        session_id: SessionId::from("admission-other"),
        ..binding
    };
    assert!(matches!(
        store
            .admit_and_bind_session(&cross_session)
            .await
            .expect_err("cross-session handle reuse must fail"),
        crate::StoreError::SessionBindingMismatch { .. }
    ));

    // A root session is equally pinned: claiming a parent for it is refused.
    let root_request = session_store_request(
        &SessionId::from("admission-root"),
        "admission-model",
        crate::SessionRelation::Root,
    );
    let root_store = factory
        .create_store(&root_request)
        .await
        .expect("create admission root fixture");
    let root_binding = crate::SessionBinding::from_create_request(&root_request);
    assert_eq!(
        root_store
            .admit_and_bind_session(&root_binding)
            .await
            .expect("same-relation rebind of a root session"),
        crate::SessionAdmission::Rebound
    );
    assert!(matches!(
        root_store
            .admit_and_bind_session(&crate::SessionBinding {
                session_id: root_request.session_id.clone(),
                relation: crate::SessionRelation::Child {
                    parent_session_id: SessionId::from("admission-parent"),
                    caused_by: None,
                },
            })
            .await
            .expect_err("claiming a parent for a recorded root must be refused"),
        crate::StoreError::SessionRelationMismatch { .. }
    ));
    assert_eq!(
        root_store
            .load_session_meta()
            .await
            .expect("reload root metadata")
            .expect("root metadata")
            .relation,
        crate::SessionRelation::Root,
        "a refused rebind must leave a root session a root"
    );

    let deleted_request = session_store_request(
        &SessionId::from("admission-deleted"),
        "admission-model",
        crate::SessionRelation::Root,
    );
    let deleted_store = factory
        .create_store(&deleted_request)
        .await
        .expect("create admission deletion fixture");
    factory
        .delete_session(&deleted_request.session_id)
        .await
        .expect("delete admission fixture");
    let deleted_binding = crate::SessionBinding::from_create_request(&deleted_request);
    assert_session_id_was_used_and_deleted(
        deleted_store
            .admit_and_bind_session(&deleted_binding)
            .await
            .expect_err("deleted id admission must fail"),
        &deleted_request.session_id,
    );
    deleted_store
        .vacuum()
        .await
        .expect("vacuum deleted admission fixture");
    assert_session_id_was_used_and_deleted(
        deleted_store
            .admit_and_bind_session(&deleted_binding)
            .await
            .expect_err("vacuum must preserve admission tombstone"),
        &deleted_request.session_id,
    );
}
