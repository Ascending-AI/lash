use super::*;

pub(super) async fn compare_plugin_state(
    sqlite_root: &Path,
    postgres: &PostgresStorage,
    database_url: &str,
    nonce: &str,
) {
    let mut runners = runners_for_case(
        CaseName::PluginStateSeam,
        sqlite_root,
        postgres,
        database_url,
        nonce,
    )
    .await;
    let mut observations = Vec::new();
    for runner in &mut runners {
        let mut child = runner.create_request();
        child.session_id.push_str("-child");
        let child_store = runner
            .factory()
            .create_conformance_store(&child)
            .await
            .unwrap();
        let states = lash_conformance::plugin_state_boundary_trace(
            runner.store(),
            &runner.session_id,
            child_store,
            &child.session_id,
        )
        .await;
        observations.push((runner.name, states));
        runner.close_reopened_postgres_pool().await;
    }
    for pair in observations.windows(2) {
        assert_eq!(
            pair[0].1, pair[1].1,
            "plugin state differs between {} and {}",
            pair[0].0, pair[1].0
        );
    }
    eprintln!("PASS plugin_state_boundary_fork_rebuild: backends=3 decoded_checkpoints=9");
}
