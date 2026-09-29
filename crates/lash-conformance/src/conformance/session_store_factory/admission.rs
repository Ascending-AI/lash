//! Session admission contract: create, rebind, and the durable relation the
//! rebind is checked against.

use super::*;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn session_admission_contract(factory: Arc<dyn crate::DeploymentStore>) {
    // `admit_session` is the one admission seam (ADR 0112 §1.1). It refuses
    // an invalid id, then a deletion tombstone, then creates or checks the
    // recorded lineage, in one transaction.
    let with_relation = |request: &crate::SessionStoreCreateRequest,
                         relation: crate::SessionRelation| {
        crate::SessionStoreCreateRequest {
            relation,
            ..request.clone()
        }
    };
    let empty = session_store_request(
        &SessionId::from(""),
        "admission-model",
        crate::SessionRelation::Root,
    );
    assert!(matches!(
        factory
            .admit_session(&empty)
            .await
            .expect_err("empty session id must be rejected"),
        crate::StoreError::InvalidSessionId { .. }
    ));

    // The host API's creating verb bakes its config in (FIG-4099, FIG-4112).
    let request = crate::SessionStoreCreateRequest {
        head: crate::SessionCreationHead::Config,
        ..session_store_request(
            &SessionId::from("admission-created"),
            "admission-model",
            crate::SessionRelation::Child {
                parent_session_id: SessionId::from("admission-parent"),
                caused_by: None,
            },
        )
    };
    assert_eq!(
        factory
            .admit_session(&request)
            .await
            .expect("admit a fresh session"),
        crate::SessionAdmission::Created
    );
    let store = factory
        .live_view(&request.session_id)
        .await
        .expect("look up the admitted session")
        .expect("the admitted session is live");
    assert_eq!(
        factory
            .admit_session(&request)
            .await
            .expect("admit the same session again"),
        crate::SessionAdmission::Rebound
    );
    let created_meta = store
        .load_session_meta()
        .await
        .expect("load admitted metadata")
        .expect("admission must durably materialize metadata");
    assert_eq!(created_meta.session_id, request.session_id);
    assert_eq!(created_meta.relation, request.relation);

    // Config is baked at creation (FIG-4099): the creating admission wrote
    // the creator's config as the session's initial head, at head and config
    // revision 0 with no frame, in the transaction that wrote the row; the
    // rebind above wrote nothing over it.
    let created_head = store
        .load_session_head_meta()
        .await
        .expect("load the created head")
        .expect("a creating admission writes the config head");
    let mut expected_config = request.config.clone();
    expected_config.config_revision = 0;
    assert_eq!(created_head.config, expected_config);
    assert_eq!(created_head.head_revision, 0);
    assert_eq!(created_head.current_frame_node_id, None);
    assert_eq!(created_head.checkpoint_ref, None);
    assert_eq!(created_head.leaf_node_id, None);
    let restated = crate::SessionStoreCreateRequest {
        config: crate::PersistedSessionConfig::from(&crate::SessionPolicy {
            model: crate::ModelSpec::builder("a-rebinding-model")
                .context_window_tokens(1_000)
                .build()
                .expect("model spec"),
            ..request.config.session_policy()
        }),
        ..request.clone()
    };
    assert_eq!(
        factory
            .admit_session(&restated)
            .await
            .expect("rebind with other config"),
        crate::SessionAdmission::Rebound
    );
    assert_eq!(
        store
            .load_session_head_meta()
            .await
            .expect("reload the head")
            .expect("the head survives a rebind")
            .config,
        expected_config,
        "a rebinding admission never writes config"
    );
    // A creator that commits its own first head writes only the row.
    let self_committing = session_store_request(
        &SessionId::from("admission-self-committing"),
        "admission-model",
        crate::SessionRelation::Root,
    );
    assert_eq!(
        self_committing.head,
        crate::SessionCreationHead::CommittedByCreator
    );
    assert_eq!(
        factory
            .admit_session(&self_committing)
            .await
            .expect("admit a self-committing session"),
        crate::SessionAdmission::Created
    );
    assert!(
        factory
            .live_view(&self_committing.session_id)
            .await
            .expect("look up the self-committing session")
            .expect("the admitted session is live")
            .load_session_head_meta()
            .await
            .expect("load its head")
            .is_none(),
        "a self-committing creator's admission writes no head"
    );
    let loaded =
        crate::store::load_session_window_state(&store, crate::store::WindowSelector::Current)
            .await
            .expect("a config-only head loads as a window")
            .expect("the created session has a window");
    assert_eq!(loaded.config, expected_config);
    assert_eq!(loaded.state.policy.model, request.config.model);

    // `Root` declares no lineage, so it always rebinds and writes nothing.
    assert_eq!(
        factory
            .admit_session(&with_relation(&request, crate::SessionRelation::Root))
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
    assert!(matches!(
        factory
            .admit_session(&with_relation(
                &request,
                crate::SessionRelation::Child {
                    parent_session_id: SessionId::from("admission-other-parent"),
                    caused_by: None,
                },
            ))
            .await
            .expect_err("rebind naming a different parent must be refused"),
        crate::StoreError::SessionRelationMismatch { .. }
    ));
    assert!(matches!(
        factory
            .admit_session(&with_relation(
                &request,
                crate::SessionRelation::Fork {
                    source_session_id: SessionId::from("admission-parent"),
                    source_node_id: "admission-node".into(),
                },
            ))
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

    // A view cannot be pointed at another session: a request carrying a
    // foreign session id is refused before it reaches the store (ADR 0112 §3).
    let foreign_meta = crate::SessionMeta {
        session_id: SessionId::from("admission-other"),
        ..created_meta.clone()
    };
    assert!(matches!(
        store
            .save_session_meta(foreign_meta)
            .await
            .expect_err("a view must refuse another session's request"),
        crate::StoreError::ForeignSessionRequest { .. }
    ));
    assert!(
        factory
            .live_view(&SessionId::from("admission-other"))
            .await
            .expect("look up the foreign session")
            .is_none(),
        "a refused foreign request writes nothing"
    );

    // A root session is equally pinned: claiming a parent for it is refused.
    let root_request = session_store_request(
        &SessionId::from("admission-root"),
        "admission-model",
        crate::SessionRelation::Root,
    );
    let root_store = factory
        .admit_view(&root_request)
        .await
        .expect("create admission root fixture");
    assert_eq!(
        factory
            .admit_session(&root_request)
            .await
            .expect("same-relation rebind of a root session"),
        crate::SessionAdmission::Rebound
    );
    assert!(matches!(
        factory
            .admit_session(&with_relation(
                &root_request,
                crate::SessionRelation::Child {
                    parent_session_id: SessionId::from("admission-parent"),
                    caused_by: None,
                },
            ))
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

    // A recorded fork is pinned to *both* halves of its lineage: the session it
    // branched from and the node it branched at. Restating it as a child of its
    // own source is a different lineage, not a paraphrase of the same one, and
    // moving the branch point silently would rewrite where the history it
    // continues was cut.
    let fork_source_session_id = SessionId::from("admission-fork-source");
    let fork_request = session_store_request(
        &SessionId::from("admission-fork"),
        "admission-model",
        crate::SessionRelation::Fork {
            source_session_id: fork_source_session_id.clone(),
            source_node_id: "admission-fork-node".into(),
        },
    );
    let fork_store = factory
        .admit_view(&fork_request)
        .await
        .expect("create admission fork fixture");
    assert_eq!(
        factory
            .admit_session(&fork_request)
            .await
            .expect("same-relation rebind of a forked session"),
        crate::SessionAdmission::Rebound
    );
    assert!(matches!(
        factory
            .admit_session(&with_relation(
                &fork_request,
                crate::SessionRelation::Child {
                    parent_session_id: fork_source_session_id.clone(),
                    caused_by: None,
                },
            ))
            .await
            .expect_err("restating a recorded fork as a child of its own source must be refused"),
        crate::StoreError::SessionRelationMismatch { .. }
    ));
    assert!(matches!(
        factory
            .admit_session(&with_relation(
                &fork_request,
                crate::SessionRelation::Fork {
                    source_session_id: fork_source_session_id.clone(),
                    source_node_id: "admission-fork-other-node".into(),
                },
            ))
            .await
            .expect_err("moving a recorded fork's branch point must be refused"),
        crate::StoreError::SessionRelationMismatch { .. }
    ));
    assert_eq!(
        fork_store
            .load_session_meta()
            .await
            .expect("reload fork metadata")
            .expect("fork metadata")
            .relation,
        fork_request.relation,
        "a refused rebind must leave the recorded fork lineage unchanged"
    );

    let deleted_request = session_store_request(
        &SessionId::from("admission-deleted"),
        "admission-model",
        crate::SessionRelation::Root,
    );
    factory
        .admit_session(&deleted_request)
        .await
        .expect("create admission deletion fixture");
    factory
        .delete_session(&deleted_request.session_id)
        .await
        .expect("delete admission fixture");
    assert_session_id_was_used_and_deleted(
        factory
            .admit_session(&deleted_request)
            .await
            .expect_err("deleted id admission must fail"),
        &deleted_request.session_id,
    );
    factory
        .vacuum(&deleted_request.session_id)
        .await
        .expect("vacuum deleted admission fixture");
    assert_session_id_was_used_and_deleted(
        factory
            .admit_session(&deleted_request)
            .await
            .expect_err("vacuum must preserve admission tombstone"),
        &deleted_request.session_id,
    );

    // Error precedence (FIG-1282): the tombstone is checked before the
    // recorded lineage, so a deleted id admitted under a conflicting relation
    // answers SessionDeleted rather than SessionRelationMismatch.
    assert_session_id_was_used_and_deleted(
        factory
            .admit_session(&with_relation(
                &deleted_request,
                crate::SessionRelation::Child {
                    parent_session_id: SessionId::from("admission-parent"),
                    caused_by: None,
                },
            ))
            .await
            .expect_err("a deleted id admitted under another lineage must report the tombstone"),
        &deleted_request.session_id,
    );
}
