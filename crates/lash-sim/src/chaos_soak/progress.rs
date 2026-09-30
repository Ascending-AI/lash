use std::fs::File;
use std::io::{self, Write};
use std::time::Instant;

/// Writes outside libtest's capture buffer, including while a case is stuck.
pub(super) struct Progress {
    case: String,
    index: usize,
    seed: u64,
    started: Instant,
    file: Option<File>,
}

impl Progress {
    pub(super) fn new(index: usize, seed: u64) -> io::Result<Self> {
        let case = std::thread::current()
            .name()
            .unwrap_or("chaos-soak")
            .to_owned();
        let file = std::env::var_os("TEST_UNDECLARED_OUTPUTS_DIR")
            .map(|directory| {
                let name: String = case
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                    .collect();
                File::create(
                    std::path::PathBuf::from(directory)
                        .join(format!("{name}-{seed:016x}-{index}.log")),
                )
            })
            .transpose()?;
        Ok(Self {
            case,
            index,
            seed,
            started: Instant::now(),
            file,
        })
    }

    pub(super) fn record(&mut self, message: impl std::fmt::Display) {
        let line = format!(
            "chaos-soak case={} epoch={} seed={:#x} wall={:?}: {message}",
            self.case,
            self.index,
            self.seed,
            self.started.elapsed()
        );
        let _ = writeln!(io::stderr().lock(), "{line}");
        if let Some(file) = &mut self.file {
            // Write each line directly so an unfinished case has a progress file.
            if let Err(error) = writeln!(file, "{line}") {
                let _ = writeln!(io::stderr().lock(), "chaos-soak progress file: {error}");
            }
        }
    }

    pub(super) async fn wait<F: std::future::Future>(&mut self, phase: &str, work: F) -> F::Output {
        let mut work = std::pin::pin!(work);
        loop {
            tokio::select! {
                result = &mut work => return result,
                () = tokio::time::sleep(std::time::Duration::from_secs(15)) => {
                    self.record(format!("still awaiting {phase}"));
                }
            }
        }
    }
}
