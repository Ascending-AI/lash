//! Event handover fixtures shared by the durable wait laws.

use super::*;

fn cell_provider(
    cell: String,
    requests: &Arc<std::sync::Mutex<Vec<LlmRequest>>>,
) -> ProviderHandle {
    let requests = Arc::clone(requests);
    crate::testing::TestProvider::builder()
        .kind("cell-segment")
        .complete(move |request| {
            let requests = Arc::clone(&requests);
            let cell = cell.clone();
            async move {
                let call = {
                    let mut requests = requests.lock_recover();
                    requests.push(request);
                    requests.len()
                };
                Ok(text_response(&if call <= 2 {
                    cell
                } else {
                    typescript_block(r#"finish("asked again");"#)
                }))
            }
        })
        .build()
        .into_handle()
}

fn cell_core(
    backend: lash_core::Backend,
    work: Arc<dyn lash_core::SessionWorkEngine>,
    cell: String,
    requests: &Arc<std::sync::Mutex<Vec<LlmRequest>>>,
) -> LashCore {
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(backend)
        .with_session_work(work)
        .into_backend();
    rlm_core_builder_over(backend)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(
            crate::QueuedWorkBatchingConfig::new(1024).with_max_turn_input_admission(1),
        )
        .serve_test_llm_profile(cell_provider(cell, requests), mock_llm_profile_spec())
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ))
        .plugin(lash_core::testing::process_engine_plugin_fixture())
        .build(crate::testing::runtime_lease_owner())
        .expect("build the core")
}

mod waits;
