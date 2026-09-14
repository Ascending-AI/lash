use std::path::{Path, PathBuf};

use chrono::Utc;

pub fn default_report_path(kind: &str) -> PathBuf {
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    repo_root()
        .join(".benchmarks")
        .join(kind)
        .join(format!("{stamp}.json"))
}

pub fn default_dhat_output_path(report_out: &Path, fallback_stem: &str) -> PathBuf {
    let stem = report_out
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(fallback_stem);
    report_out.with_file_name(format!("{stem}.dhat.json"))
}

#[expect(
    clippy::expect_used,
    reason = "CARGO_MANIFEST_DIR of this crate is <repo>/crates/lash-perf, which the message states, so two parents exist"
)]
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("lash-perf crate should live under <repo>/crates/")
        .to_path_buf()
}
