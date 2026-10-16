//! Regenerate the reviewable workflow corpus in the explicitly named tree.
use anyhow::{Context, Result, ensure};

#[expect(
    clippy::disallowed_methods,
    reason = "the bless command reads argv and writes explicitly supplied corpus paths"
)]
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 2 && args[0] == "--bless",
        "usage: workflow-corpus --bless <examples/workflows>"
    );
    let root = std::path::Path::new(&args[1]);
    let outputs = workflow_corpus::render(root)?;
    for (relative, text) in &outputs {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().context("output parent")?)?;
        std::fs::write(path, text)?;
    }
    println!("blessed {} readable outputs", outputs.len());
    Ok(())
}
