use std::time::Instant;

pub fn elapsed_ms(started: Instant) -> f64 {
    round3(started.elapsed().as_secs_f64() * 1000.0)
}

pub fn nanos_to_ms(nanos: u64) -> f64 {
    round3(nanos as f64 / 1_000_000.0)
}

pub fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// Optional acknowledged perf-stat FIFO control. Separate profiling runs can
/// count a turn window without counting construction, seeding or validation.
pub(crate) fn instruction_control(command: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::{BufRead as _, Write as _};
    if let Some(path) = std::env::var_os("LASH_OFF_CPU_MARKS") {
        let monotonic_ns = lash_core_ids::perf_witness::latency::Stamp::now().monotonic_ns;
        writeln!(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?,
            "{}",
            serde_json::json!({"command":command,"monotonic_ns":monotonic_ns})
        )?;
    }
    let Some(control) = std::env::var_os("LASH_PERF_CONTROL") else {
        return Ok(());
    };
    let ack = std::env::var_os("LASH_PERF_ACK").context("LASH_PERF_CONTROL needs LASH_PERF_ACK")?;
    let mut writer = std::fs::OpenOptions::new().write(true).open(control)?;
    let reader = std::fs::File::open(ack)?;
    writeln!(writer, "{command}")?;
    writer.flush()?;
    let mut line = String::new();
    std::io::BufReader::new(reader).read_line(&mut line)?;
    anyhow::ensure!(
        line.trim() == "ack",
        "perf control was not acknowledged: {line:?}"
    );
    Ok(())
}
