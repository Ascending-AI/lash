use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;

pub fn memory(pid: u32) -> Result<(u64, u64)> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let value = |key: &str| -> Result<u64> {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.split_whitespace().next())
            .context("missing Linux memory counter")?
            .parse()
            .map_err(Into::into)
    };
    Ok((value("VmRSS:")?, value("VmHWM:")?))
}
#[derive(Serialize)]
pub struct Summary {
    pub metric: String,
    pub count: usize,
    pub unit: String,
    pub p50: i64,
    pub p99: i64,
    pub max: i64,
    pub throughput_per_second: Option<f64>,
}
pub struct Samples {
    pub raw: std::io::BufWriter<std::fs::File>,
    pub summaries: Vec<Summary>,
}
impl Samples {
    pub fn new(path: &Path) -> Result<Self> {
        use std::io::Write;
        std::fs::create_dir_all(path)?;
        let mut raw = std::io::BufWriter::new(std::fs::File::create(path.join("samples.csv"))?);
        writeln!(raw, "metric,sample,value,unit")?;
        Ok(Self {
            raw,
            summaries: Vec::new(),
        })
    }
    pub fn record(&mut self, name: &str, values: &[i64], unit: &str) -> Result<()> {
        use std::io::Write;
        anyhow::ensure!(!values.is_empty(), "empty metric {name}");
        for (i, v) in values.iter().enumerate() {
            writeln!(self.raw, "{name},{i},{v},{unit}")?;
        }
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let sum: i128 = values.iter().map(|v| i128::from(*v)).sum();
        let summary = Summary {
            metric: name.into(),
            count: sorted.len(),
            unit: unit.into(),
            p50: sorted[(sorted.len() * 50).div_ceil(100) - 1],
            p99: sorted[(sorted.len() * 99).div_ceil(100) - 1],
            max: sorted[sorted.len() - 1],
            throughput_per_second: (unit == "ns"
                && sum > 0
                && (name.ends_with("/worker")
                    || name.ends_with("/baseline")
                    || name.ends_with("/batch")))
            .then(|| values.len() as f64 * 1e9 / sum as f64),
        };
        println!(
            "{name}: n={} p50={} p99={} max={} {unit}",
            summary.count, summary.p50, summary.p99, summary.max
        );
        self.summaries.push(summary);
        Ok(())
    }
    pub fn finish(&mut self, path: &Path) -> Result<()> {
        use std::io::Write;
        self.raw.flush()?;
        std::fs::write(
            path.join("summary.json"),
            serde_json::to_vec_pretty(&self.summaries)?,
        )?;
        Ok(())
    }
}
