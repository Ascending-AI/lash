use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use super::host;
use super::manifest::{Manifest, Scenario};
use super::report::{self, Judge, Row};
use super::{Evidence, Job, write_json};

pub(super) struct Executable {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub fixture: bool,
}

impl Executable {
    pub fn current() -> Result<Self> {
        Ok(Self {
            program: std::env::current_exe()?,
            args: Vec::new(),
            fixture: false,
        })
    }

    fn spawn(&self, job: &Path, phase: &str, directory: &Path) -> Result<ChildGuard> {
        let log = std::fs::File::create(directory.join(format!("{phase}.log")))?;
        // exec preserves the PID the parent must kill and reap. In particular,
        // do not kill a shell wrapper and leave its worker alive over the store.
        let mut command = Command::new("bash");
        command
            .args(["-c", "ulimit -v 8388608; exec \"$@\"", "eval-worker"])
            .arg(&self.program)
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        if self.fixture {
            command
                .env("LASH_RUBRIC_FIXTURE_JOB", job)
                .env("LASH_RUBRIC_FIXTURE_PHASE", phase);
        } else {
            command.arg("eval-worker").arg(job).arg(phase);
        }
        Ok(ChildGuard(
            command.spawn().context("start isolated eval worker")?,
        ))
    }
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn freeze(path: &Path) -> Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

async fn execute(
    job: &Job,
    path: &Path,
    executable: &Executable,
    timeout: Duration,
) -> Result<bool> {
    let mut child = executable.spawn(path, "initial", &job.directory)?;
    let deadline = Instant::now() + timeout;
    let recovery = job.case.scenario == Scenario::KillRestart;
    loop {
        if let Some(status) = child.0.try_wait()? {
            ensure!(
                status.success(),
                "initial worker exited {status}; see initial.log"
            );
            ensure!(!recovery, "recovery cut was never reached");
            return Ok(false);
        }
        if recovery
            && host::trace(&job.directory.join("trace.jsonl"))
                .is_ok_and(|trace| host::cut_seen(&trace, &job.case))
        {
            child.0.kill()?;
            let status = child.0.wait()?;
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                ensure!(
                    status.signal() == Some(9),
                    "worker was not killed by SIGKILL: {status}"
                );
            }
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "initial worker timed out; see initial.log"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut child = executable.spawn(path, "resume", &job.directory)?;
    loop {
        if let Some(status) = child.0.try_wait()? {
            ensure!(
                status.success(),
                "resume worker exited {status}; see resume.log"
            );
            return Ok(true);
        }
        ensure!(
            Instant::now() < deadline,
            "resume worker timed out; see resume.log"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub(super) async fn run(
    dataset: &Manifest,
    source: &str,
    directory: &Path,
    executable: &Executable,
    judge: &Judge,
) -> Result<Vec<Row>> {
    dataset.validate()?;
    if directory.exists() {
        bail!("output directory already exists; runs never overwrite evidence");
    }
    if let Some(parent) = directory.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir(directory)?;
    let directory = directory.canonicalize()?;
    std::fs::write(directory.join("manifest.toml"), source)?;
    freeze(&directory.join("manifest.toml"))?;
    write_json(&directory.join("dataset.json"), dataset)?;
    freeze(&directory.join("dataset.json"))?;
    let mut rows = Vec::new();
    for case in &dataset.cases {
        for repeat in 0..dataset.repeats {
            for (position, arm) in dataset.arm_order.iter().enumerate() {
                let model = dataset
                    .models
                    .iter()
                    .find(|model| &model.id == arm)
                    .context("unknown arm")?;
                let bundle = format!("{}/{}/{repeat}", case.id, model.id);
                let job = Job {
                    case: case.clone(),
                    model: model.clone(),
                    directory: directory.join(&bundle),
                };
                std::fs::create_dir_all(&job.directory)?;
                let job_path = job.directory.join("job.json");
                write_json(&job_path, &job)?;
                freeze(&job_path)?;
                let inputs = serde_json::to_vec(case)?;
                std::fs::write(job.directory.join("inputs.json"), &inputs)?;
                freeze(&job.directory.join("inputs.json"))?;
                let mut failures = Vec::new();
                let killed = match execute(
                    &job,
                    &job_path,
                    executable,
                    Duration::from_secs(dataset.timeout_seconds),
                )
                .await
                {
                    Ok(killed) => killed,
                    Err(error) => {
                        failures.push(format!("{error:#}"));
                        for phase in ["initial", "resume"] {
                            if let Ok(log) =
                                std::fs::read_to_string(job.directory.join(format!("{phase}.log")))
                            {
                                failures.push(format!(
                                    "{phase} worker: {}",
                                    log.chars().take(16000).collect::<String>()
                                ));
                            }
                        }
                        false
                    }
                };
                write_json(&job.directory.join("recovery.json"), &killed)?;
                let evidence = match std::fs::read(job.directory.join("evidence.json"))
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| Ok(serde_json::from_slice::<Evidence>(&bytes)?))
                {
                    Ok(evidence) => evidence,
                    Err(error) => {
                        failures.push(format!("evidence unavailable: {error:#}"));
                        Evidence {
                            messages: Vec::new(),
                            turn_index: 0,
                            settled_view: serde_json::Value::Null,
                            trace: host::trace(&job.directory.join("trace.jsonl"))
                                .unwrap_or_default(),
                        }
                    }
                };
                // Every matrix cell has the same artifact layout, including
                // failures. Missing observations remain explicit null/empty data.
                write_json(&job.directory.join("evidence.json"), &evidence)?;
                host::archive(&job, &evidence)?;
                let gates = report::gates(case, &evidence, killed);
                write_json(&job.directory.join("gates.json"), &gates)?;
                let score = judge.score(case, &dataset.judge, &evidence, &gates).await?;
                write_json(&job.directory.join("judgment.json"), &score)?;
                if let Some(error) = &score.failure {
                    failures.push(error.clone());
                }
                let db = job.directory.join("lash-sessions.db");
                let store_sha256 = if db.exists() {
                    freeze(&db)?;
                    Some(sha(&std::fs::read(&db)?))
                } else {
                    None
                };
                let row = Row {
                    case: case.id.clone(),
                    arm: model.clone(),
                    arm_position: position,
                    repeat,
                    bundle,
                    input_sha256: sha(&inputs),
                    store_sha256,
                    covered: failures.is_empty() && gates.iter().all(|gate| gate.passed),
                    accepted: report::accepted(&gates, score.score, case.pass_score, &failures),
                    gates,
                    score: score.score,
                    failures,
                };
                write_json(&job.directory.join("result.json"), &row)?;
                rows.push(row);
                // Preserve partial progress if a subsequent cell is interrupted.
                report::render(&directory, dataset, &rows)?;
            }
        }
    }
    Ok(rows)
}
