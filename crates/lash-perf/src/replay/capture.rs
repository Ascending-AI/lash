//! Each counted workload owns a fresh producer, including its process caches.
use anyhow::{Context, Result, ensure};

use super::{GateArgs, receipt::ReplayReceipt, script::Workload, write_receipt};

pub(super) fn population(
    args: &GateArgs,
    workloads: &[(Workload, Option<usize>)],
) -> Result<Vec<ReplayReceipt>> {
    let mut receipts = Vec::new();
    for run in 0..5 {
        let out = args.out.with_extension(format!("{run}.json"));
        remove_old(&out)?;
        let mut combined: Option<ReplayReceipt> = None;
        let mut log = Vec::new();
        for (index, (script, turns)) in workloads.iter().enumerate() {
            let child_out = args.out.with_extension(format!("{run}.{index}.json"));
            remove_old(&child_out)?;
            let child = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "replay-gate",
                    "--capture",
                    "--capture-workload",
                    &script.name,
                ])
                .arg("--store-dir")
                .arg(args.store_dir.join(format!("run-{run}/{index}")))
                .arg("--out")
                .arg(&child_out)
                .output()?;
            log.extend_from_slice(&child.stdout);
            log.extend_from_slice(&child.stderr);
            std::fs::write(args.out.with_extension(format!("{run}.log")), &log)?;
            ensure!(
                child.status.success() && child_out.exists(),
                "calibration child {run}/{index} did not produce a receipt"
            );
            let receipt: ReplayReceipt = serde_json::from_slice(&std::fs::read(child_out)?)?;
            ensure!(
                receipt.workloads.len() == 1,
                "incomplete calibration producer"
            );
            let row = &receipt.workloads[0];
            ensure!(
                row.workload == script.name
                    && row.script_digest == script.digest()
                    && row.turns
                        == turns
                            .unwrap_or(script.turn_count())
                            .min(script.turn_count())
                    && row.sessions == script.sessions,
                "calibration workload geometry changed"
            );
            if let Some(first) = &mut combined {
                let a = &first.identity;
                let b = &receipt.identity;
                ensure!(
                    a.executable_sha256.is_some()
                        && a.executable_sha256 == b.executable_sha256
                        && a.commit == b.commit
                        && a.build_profile == b.build_profile
                        && a.allocator == b.allocator
                        && a.worker_resources == b.worker_resources,
                    "calibration producer identity drift"
                );
                first.workloads.extend(receipt.workloads);
            } else {
                combined = Some(receipt);
            }
        }
        let receipt = combined.context("empty counted population")?;
        write_receipt(&receipt, &out)?;
        receipts.push(receipt);
    }
    Ok(receipts)
}

fn remove_old(path: &std::path::Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
