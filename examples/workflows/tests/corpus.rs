#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the corpus law reads declared fixture data"
)]
fn design_8_corpus_pins_readable_per_dialect_outputs() -> anyhow::Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let outputs = workflow_corpus::render(root)?;
    for (relative, expected) in outputs {
        let path = root.join(&relative);
        let actual = std::fs::read_to_string(&path).map_err(|error| {
            anyhow::anyhow!("{relative}: {error}; run the corpus bless command")
        })?;
        anyhow::ensure!(
            actual == expected,
            "{relative} differs; run the corpus bless command and review the diff"
        );
    }
    Ok(())
}
