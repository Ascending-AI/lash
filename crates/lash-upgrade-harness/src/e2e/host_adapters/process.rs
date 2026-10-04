use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use tokio::process::{Child, Command};

use crate::e2e::case::{ArtifactIdentity, CaseLease};
use crate::e2e::control::{CleanupReceipt, ProcessReceipt};

/// A child is tracked before its readiness probe. Failed boot therefore has
/// the same cleanup owner as successful boot.
pub struct HostProcess {
    child: Child,
    pub receipt: ProcessReceipt,
    log: PathBuf,
    listeners: Vec<u16>,
}

impl HostProcess {
    pub async fn spawn(
        artifact: &ArtifactIdentity,
        lease: &mut CaseLease,
        role: &str,
        environment: BTreeMap<String, String>,
        listeners: Vec<u16>,
    ) -> Result<Self> {
        Self::spawn_with_args(artifact, lease, role, environment, listeners, &[]).await
    }

    pub async fn spawn_with_args(
        artifact: &ArtifactIdentity,
        lease: &mut CaseLease,
        role: &str,
        environment: BTreeMap<String, String>,
        listeners: Vec<u16>,
        args: &[String],
    ) -> Result<Self> {
        ensure!(
            !artifact.candidate_sha.is_empty(),
            "host artifact has no candidate provenance"
        );
        let digest = Command::new("sha256sum")
            .arg(&artifact.path)
            .output()
            .await?;
        ensure!(
            digest.status.success(),
            "cannot hash prebuilt host {}",
            artifact.path.display()
        );
        let actual = String::from_utf8(digest.stdout)?;
        ensure!(
            actual.split_whitespace().next() == Some(artifact.sha256.as_str()),
            "prebuilt host digest mismatch"
        );
        for port in &listeners {
            let probe = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, *port))
                .with_context(|| format!("host listener {port} is occupied"))?;
            drop(probe);
            ensure!(
                lease.ports.contains(port),
                "port {port} is not reserved by the case lease"
            );
        }
        std::fs::create_dir_all(&lease.directory)?;
        let incarnation = 1 + lease
            .processes
            .iter()
            .filter(|process| process.role == role)
            .count() as u32;
        std::fs::write(
            lease
                .directory
                .join(format!("{role}-{incarnation}-artifact.json")),
            serde_json::to_vec_pretty(artifact)?,
        )?;
        let log = lease.directory.join(format!("{role}-{incarnation}.log"));
        let out = File::create(&log)?;
        let mut command = Command::new(&artifact.path);
        command
            .args(args)
            .envs(environment)
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out)
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let child = command
            .spawn()
            .with_context(|| format!("boot {}", artifact.path.display()))?;
        let receipt = ProcessReceipt {
            role: role.into(),
            pid: child.id().context("child has no pid")?,
            incarnation,
            log: log.display().to_string(),
        };
        lease.processes.push(receipt.clone());
        Ok(Self {
            child,
            receipt,
            log,
            listeners,
        })
    }

    pub fn check_alive(&mut self) -> Result<()> {
        if let Some(status) = self.child.try_wait()? {
            bail!(
                "host {} exited {status}; log {}",
                self.receipt.role,
                self.log.display()
            );
        }
        Ok(())
    }

    pub async fn kill(&mut self) -> Result<()> {
        self.check_alive()?;
        let sent = Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", self.receipt.pid)])
            .status()
            .await?;
        ensure!(
            sent.success(),
            "could not kill the owned host process group"
        );
        self.child.wait().await.context("reap killed host")?;
        Ok(())
    }

    pub async fn stop(
        &mut self,
        deadline: Instant,
        shutdown_marker: Option<&str>,
    ) -> Result<Vec<CleanupReceipt>> {
        let status = if let Some(status) = self.child.try_wait()? {
            status
        } else {
            let sent = Command::new("kill")
                .args(["-TERM", &self.receipt.pid.to_string()])
                .status()
                .await?;
            ensure!(sent.success(), "host termination request failed");
            let budget = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(budget, self.child.wait()).await {
                Ok(status) => status?,
                Err(_) => {
                    self.kill().await?;
                    bail!(
                        "host failed graceful stop; force-reaped {}",
                        self.receipt.pid
                    );
                }
            }
        };
        if let Some(marker) = shutdown_marker {
            ensure!(
                status.success(),
                "host exited without orderly shutdown: {status}"
            );
            let log = std::fs::read_to_string(&self.log)?;
            ensure!(
                log.lines().any(|line| line.starts_with(marker)),
                "host did not acknowledge shutdown/flush"
            );
            ensure!(!log.contains("panicked at"), "owned host panicked");
        }
        let mut receipts = vec![CleanupReceipt {
            resource: format!("pid:{}", self.receipt.pid),
            closed: true,
            detail: "terminated and reaped".into(),
        }];
        for port in &self.listeners {
            ensure!(
                tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, *port))
                    .await
                    .is_err(),
                "host leaked listener {port}"
            );
            receipts.push(CleanupReceipt {
                resource: format!("listener:{port}"),
                closed: true,
                detail: "connection refused after reap".into(),
            });
        }
        Ok(receipts)
    }

    pub fn log(&self) -> &Path {
        &self.log
    }

    /// A one-shot live host must finish inside the case watchdog; successful
    /// exit alone is insufficient, so the caller still checks its row oracle.
    pub async fn finish(&mut self, deadline: Instant) -> Result<()> {
        match tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            self.child.wait(),
        )
        .await
        {
            Ok(status) => ensure!(
                status?.success(),
                "one-shot host failed; log {}",
                self.log.display()
            ),
            Err(_) => {
                self.kill().await?;
                bail!("one-shot host watchdog; log {}", self.log.display());
            }
        }
        ensure!(
            !std::fs::read_to_string(&self.log)?.contains("panicked at"),
            "one-shot host panicked"
        );
        Ok(())
    }
}

/// Poll a predicate, never elapsed time, with the case's bounded deadline.
pub async fn ready(
    process: &mut HostProcess,
    http: &reqwest::Client,
    url: &str,
    expected_service: &str,
    deadline: Instant,
) -> Result<()> {
    loop {
        process.check_alive()?;
        if let Ok(response) = http.get(url).send().await
            && response.status().is_success()
            && let Ok(body) = response.json::<serde_json::Value>().await
            && body["service"] == expected_service
        {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "host readiness predicate missed its deadline: {url}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
