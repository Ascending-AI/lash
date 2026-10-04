//! Capture the immutable, observed body owner before its first fault cut.
use crate::e2e::control::WorkIdentity;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Deserialize, Serialize)]
struct BodyIdentity {
    session: String,
    work: WorkIdentity,
    protocol: u32,
}

pub(super) async fn capture(
    path: &Path,
    session: &str,
    run: &lash::TurnId,
    observe: impl Future<Output = Result<(WorkIdentity, u32)>>,
) -> Result<WorkIdentity> {
    let captured: BodyIdentity = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let (work, protocol) = observe.await?;
            let captured = BodyIdentity {
                session: session.into(),
                work,
                protocol,
            };
            captured.verify(session, run)?;
            super::super::super::write_atomically(path, &serde_json::to_vec(&captured)?)?;
            captured
        }
        Err(error) => return Err(error.into()),
    };
    captured.verify(session, run)?;
    Ok(captured.work)
}

pub(super) fn read(path: &Path, session: &str, run: &lash::TurnId) -> Result<WorkIdentity> {
    let captured: BodyIdentity = serde_json::from_slice(&std::fs::read(path)?)?;
    captured.verify(session, run)?;
    Ok(captured.work)
}

impl BodyIdentity {
    fn verify(&self, session: &str, run: &lash::TurnId) -> Result<()> {
        ensure!(
            self.session == session && self.work.run == run.as_str(),
            "body owner belongs to another session or Run"
        );
        ensure!(self.protocol == 7, "body owner was not observed under V7");
        ensure!(
            !self.work.ingress.is_empty()
                && !self.work.segment.is_empty()
                && self.work.call.is_none()
                && self.work.ordinal.is_none(),
            "body binding lacks its original base identity"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-4964: the initial real observation is pinned before BodyEntered;
    /// redelivery cannot turn transient failover SQL into a tool failure.
    #[tokio::test]
    async fn redelivered_body_keeps_its_bound_owner_without_querying_moving_partitions() {
        let directory = tempfile::tempdir().expect("case directory");
        let path = directory.path().join("body-owner.json");
        let run = lash::TurnId::from("accepted-run");
        let work = WorkIdentity {
            ingress: "accepted-input".into(),
            run: run.to_string(),
            segment: "observed-v7-invocation".into(),
            call: None,
            ordinal: None,
        };
        let first = capture(&path, "session", &run, async { Ok((work.clone(), 7)) })
            .await
            .expect("capture the initial binding");
        let redelivery = capture(&path, "session", &run, async {
            anyhow::bail!("SQL partition store is unavailable during failover")
        })
        .await
        .expect("redelivery needs no fresh SQL observation");
        assert_eq!(redelivery, first);
        assert!(
            capture(
                &path,
                "session",
                &lash::TurnId::from("another-run"),
                async { anyhow::bail!("must not query") }
            )
            .await
            .is_err(),
            "a binding never admits another Run"
        );
    }
}
