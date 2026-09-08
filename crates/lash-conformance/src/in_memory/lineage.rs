use lash_core::testing::in_memory_lineage_handles as handles;
#[tokio::test]
async fn in_memory_fork_lineage_conformance() {
    crate::conformance::fork_lineage_conformance(handles()).await;
}

#[tokio::test]
async fn in_memory_fork_lineage_no_carrier_law() {
    crate::conformance::fork_lineage_no_carrier_law(handles()).await;
}

#[tokio::test]
async fn in_memory_fork_plan_matches_edge_walk_law() {
    crate::conformance::fork_plan_matches_edge_walk_law(handles()).await;
}
