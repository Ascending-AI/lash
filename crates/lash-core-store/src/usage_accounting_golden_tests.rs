#[test]
#[ignore = "regenerates crates/lash-core-store/src/testdata/usage_fact_payload_v4.hex"]
#[expect(
    clippy::disallowed_methods,
    reason = "the opt-in test generator writes the corpus in the supplied workspace"
)]
fn regenerate_usage_fact_payload_v4_golden_corpus() {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    let root = std::env::var_os("BUILD_WORKSPACE_DIRECTORY").expect("regeneration workspace");
    std::fs::write(
        std::path::PathBuf::from(root)
            .join("crates/lash-core-store/src/testdata/usage_fact_payload_v4.hex"),
        super::tests::usage_fact_golden_rows(),
    )
    .expect("write golden corpus");
}
