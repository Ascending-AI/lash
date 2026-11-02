//! Rewrite the readable workflow artifacts as an explicit tooling command.
#[expect(
    clippy::disallowed_methods,
    reason = "the recorder reads its workspace argument and writes generated artifacts"
)]
fn main() -> anyhow::Result<()> {
    let workspace = std::env::args_os()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("expected workspace directory"))?;
    let root = std::path::PathBuf::from(workspace).join("examples/workflows");
    for (relative, text) in workflow_corpus::render(&root)? {
        let path = root.join(relative);
        std::fs::create_dir_all(
            path.parent()
                .ok_or_else(|| anyhow::anyhow!("output parent"))?,
        )?;
        std::fs::write(path, text)?;
    }
    Ok(())
}
