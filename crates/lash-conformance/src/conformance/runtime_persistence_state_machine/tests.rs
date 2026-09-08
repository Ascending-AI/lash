use super::*;

async fn one_pending_batch_model(store: &crate::InMemorySessionStore) -> ReferenceModel {
    let mut model = ReferenceModel::default();
    let mut shape = RunShape::default();
    apply_operation(
        store,
        None,
        &mut model,
        &mut shape,
        0,
        &RuntimePersistenceOp::EnqueueWork {
            slot: 0,
            value: 0,
            coalesce: false,
        },
    )
    .await
    .expect("enqueue modeled batch");
    model
}

#[tokio::test]
async fn assert_model_agreement_attributes_total_queue_store_seam_drop() {
    let store = crate::InMemorySessionStore::new();
    let model = one_pending_batch_model(&store).await;
    store.drop_next_list_queued_work_batch();

    assert_eq!(
        assert_model_agreement(&store, &model).await,
        Err("queued-work state differs from the reference model".to_string())
    );
}

#[tokio::test]
async fn assert_model_agreement_attributes_pending_queue_store_seam_drop() {
    let store = crate::InMemorySessionStore::new();
    let model = one_pending_batch_model(&store).await;
    store.drop_next_list_pending_queued_work_batch();

    assert_eq!(
        assert_model_agreement(&store, &model).await,
        Err("pending queued-work projection differs from live-claim model".to_string())
    );
}
