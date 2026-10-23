//! Durable observations retain incompatible storage and process geometries.
use super::{Args, elapsed};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

pub(super) fn run(args: &Args, dir: &Path) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    let runtime =
        std::path::PathBuf::from("target").join(format!("compare-durable-{}", std::process::id()));
    std::fs::create_dir_all(&runtime)?;
    if let Some(binary) = &args.durable_substrate {
        let out = dir.join("durable-substrate.jsonl");
        std::fs::write(&out, [])?;
        let output = Command::new(binary)
            .args([
                "--store",
                "sqlite",
                "--case",
                "cell-0",
                "--samples",
                &args.runs.to_string(),
                "--sqlite-dir",
            ])
            .arg(runtime.join("sqlite"))
            .arg("--vm-worker")
            .arg(args.vm_worker.as_ref().context("missing vm worker")?)
            .arg("--out")
            .arg(&out)
            .output()?;
        std::fs::write(dir.join("durable-substrate.log"), &output.stderr)?;
        ensure!(
            output.status.success(),
            "durable-substrate failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let records = std::fs::read_to_string(&out)?
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>()?;
        let summary = records
            .iter()
            .find(|r| r["kind"] == "summary" && r["case"] == "cell-0")
            .context("no cell-0 summary")?;
        rows.push(json!({"engine":"lash","operation":"park-to-next-park cycle (ext_echo, 10 parks/cell)","storage":"SQLite file database WAL synchronous=FULL (fsync)","process":"resident engine, production subprocess worker","raw_receipt":out,"measurement":summary,"comparable_ratio":null}));
    }
    if let Some(binary) = &args.dqts_hibernate {
        let script = args
            .dqts_script
            .as_ref()
            .context("missing dquickts script")?
            .canonicalize()?;
        let binary = binary.canonicalize()?;
        let plain = Command::new(&binary).arg("plain").arg(&script).output()?;
        ensure!(plain.status.success(), "dquickts plain failed");
        let mut samples = Vec::new();
        for sample in 0..args.runs {
            let sample_dir = runtime.join(format!("dqts-slice-{sample}"));
            std::fs::create_dir_all(&sample_dir)?;
            let base = sample_dir.join("base");
            let state = sample_dir.join("state");
            let at = std::time::Instant::now();
            let mut output = Command::new(&binary)
                .arg("start")
                .arg(&script)
                .arg(&base)
                .arg(&state)
                .output()?;
            let mut text = output.stdout.clone();
            let mut processes = 1;
            while output.status.code() == Some(2) {
                ensure!(processes < 100_000, "dquickts exceeded slice bound");
                output = Command::new(&binary)
                    .arg("resume")
                    .arg(&base)
                    .arg(&state)
                    .output()?;
                text.extend_from_slice(&output.stdout);
                processes += 1;
            }
            let ns = elapsed(at);
            ensure!(
                output.status.success() && text == plain.stdout,
                "dquickts resumed output differs from plain"
            );
            samples.push(json!({"processes":processes,"total_ns":ns,"ns_per_process_slice":ns/f64::from(processes)}));
            // Snapshots are runtime outputs, not evidence; only the size is retained.
            std::fs::remove_file(base)?;
            std::fs::remove_file(state)?;
        }
        rows.push(json!({"engine":"dquickts","operation":"cross-process snapshot slice","storage":"base+delta files; no database; no fsync","process":"fresh exec for every slice; exec and file I/O included","script":script,"script_hash":super::hash(&std::fs::read(&script)?),"binary_hash":super::hash(&std::fs::read(&binary)?),"samples":samples,"comparable_ratio":null}));
    }
    Ok(rows)
}
