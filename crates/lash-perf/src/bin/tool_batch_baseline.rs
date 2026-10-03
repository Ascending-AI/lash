//! Controlled tool-route cost receipts for FIG-4868 and the final arc comparison.

use clap::Parser;
use std::path::PathBuf;

#[path = "tool_batch_baseline/mod.rs"]
mod tool_cost;

#[derive(Parser)]
#[command(about = "Capture the complete controlled tool invocation tree on the Restate double")]
struct Args {
    #[arg(long, default_value = "1,2,16", value_delimiter = ',')]
    widths: Vec<usize>,
    #[arg(
        long,
        default_value = "32,8192,262144,65536,1048576",
        value_delimiter = ','
    )]
    payload_bytes: Vec<usize>,
    #[arg(
        long,
        default_value = "done,retry,deferred,declared-start,cancel,race-loser,turn-transfer,process-transfer",
        value_delimiter = ','
    )]
    branches: Vec<tool_cost::Branch>,
    #[arg(long)]
    out: PathBuf,
    /// Immutable runtime source revision. The archive script supplies HEAD.
    #[arg(long)]
    source_sha: String,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    use std::io::Write as _;
    let args = Args::parse();
    anyhow::ensure!(
        !args.widths.is_empty() && args.widths.iter().all(|width| *width > 0),
        "widths must be positive"
    );
    anyhow::ensure!(
        !args.payload_bytes.is_empty() && args.payload_bytes.iter().all(|size| *size > 0),
        "payload sizes must be positive"
    );
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&args.out)?;
    for branch in args.branches {
        for &width in &args.widths {
            for &size in &args.payload_bytes {
                if !matches!(branch, tool_cost::Branch::Done) && size != args.payload_bytes[0] {
                    continue;
                }
                let receipt = tool_cost::measure(branch, width, size, &args.source_sha).await?;
                writeln!(file, "{}", serde_json::to_string(&receipt)?)?;
                file.flush()?;
                println!(
                    "branch={branch:?} width={width} payload={size} source={} raw={} invocations={}",
                    receipt.source.total,
                    receipt.engine.total,
                    receipt.invocations.len()
                );
            }
        }
    }
    Ok(())
}
