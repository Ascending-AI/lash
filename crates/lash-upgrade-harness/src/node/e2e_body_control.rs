//! H1 leaf body gates, bound to the ingress/controller's actual segment.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::e2e::control::{Barrier, BarrierKind, FileBarriers, WorkIdentity};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BodyControls {
    pub directory: PathBuf,
    pub bindings: PathBuf,
    pub timeout_ms: u64,
    pub gates: Vec<BodyGate>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BodyGate {
    pub label: String,
    pub ordinal: u32,
}

impl BodyControls {
    pub fn bind(&self, work: &WorkIdentity) -> Result<()> {
        ensure!(
            !work.ingress.is_empty() && !work.run.is_empty() && !work.segment.is_empty(),
            "incomplete admitted body binding"
        );
        std::fs::create_dir_all(&self.bindings)?;
        crate::node::write_atomically(&self.binding(&work.run), &serde_json::to_vec(work)?)
    }

    fn binding(&self, run: &str) -> PathBuf {
        self.bindings.join(format!(
            "{}.json",
            lash_core::stable_hash::sha256_hex(run.as_bytes())
        ))
    }

    pub async fn enter(
        &self,
        label: &str,
        run: &str,
        call: &str,
        ordinal: u32,
        artifact: String,
    ) -> Result<()> {
        if !self
            .gates
            .iter()
            .any(|gate| gate.label == label && gate.ordinal == ordinal)
        {
            return Ok(());
        }
        ensure!(self.timeout_ms > 0, "body gate needs a deadline");
        let deadline = Instant::now() + Duration::from_millis(self.timeout_ms);
        let mut work: WorkIdentity = loop {
            match std::fs::read(self.binding(run)) {
                Ok(bytes) => break serde_json::from_slice(&bytes)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            ensure!(
                Instant::now() < deadline,
                "controller never bound the actual body segment"
            );
            // Poll an atomic controller receipt, never a guessed readiness delay.
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        ensure!(
            work.run == run && !work.ingress.is_empty() && !work.segment.is_empty(),
            "controller bound another Run"
        );
        work.call = Some(call.into());
        work.ordinal = Some(ordinal);
        let barrier = Barrier {
            work,
            kind: BarrierKind::BodyEntered,
        };
        let barriers = FileBarriers::new(self.directory.clone(), deadline)?;
        barriers.hold(&barrier)?;
        barriers.enter(&barrier, artifact).await
    }
}
