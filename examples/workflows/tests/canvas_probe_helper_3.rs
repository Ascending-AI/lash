/// FIG-7514: every admitted canvas edit prints and independently re-lowers.
#[test]
fn admitted_canvas_edits_print_and_relower() -> anyhow::Result<()> {
    workflow_corpus::canvas_prints(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        "probe-helper",
        (3, 4),
    )
}
