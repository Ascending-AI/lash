//! TRAFFIC-CONSERVATION: export only complete intervals, conserving existing
//! frame/byte counters and SQLite CACHE_WRITE bytes without rounding residuals.
use anyhow::{Result, ensure};
use lash_sqlite_store::write_observer;
use lash_vm_client::IpcTraffic;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default)]
pub struct Reading {
    pub sqlite: Option<write_observer::Receipt>,
    pub ipc: Option<IpcTraffic>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub sqlite: Option<write_observer::Receipt>,
    pub ipc: Option<IpcReport>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IpcReport {
    pub sent: std::collections::BTreeMap<String, lash_vm_client::FrameTraffic>,
    pub received: std::collections::BTreeMap<String, lash_vm_client::FrameTraffic>,
}
impl From<IpcTraffic> for IpcReport {
    fn from(traffic: IpcTraffic) -> Self {
        Self {
            sent: IpcTraffic::SENT_KINDS
                .into_iter()
                .zip(traffic.sent)
                .map(|(name, bin)| (name.to_owned(), bin))
                .collect(),
            received: IpcTraffic::RECEIVED_KINDS
                .into_iter()
                .zip(traffic.received)
                .map(|(name, bin)| (name.to_owned(), bin))
                .collect(),
        }
    }
}
impl Reading {
    pub fn difference(
        &self,
        before: &Self,
        metrics: &std::collections::BTreeMap<String, f64>,
    ) -> Result<Report> {
        let sqlite = match (&before.sqlite, &self.sqlite) {
            (Some(before), Some(after)) => {
                let interval = after.difference(before).ok_or_else(|| anyhow::anyhow!(
                    "incomplete SQLite attribution: before_complete={} after_complete={} active={} overflow_bytes={} unknown_bytes={} failed_writes={}",
                    before.complete, after.complete, after.active_operations, after.overflow_bytes, after.unknown_bytes, after.failed_writes
                ))?;
                ensure!(
                    metrics.get("store_bytes_written").is_some_and(
                        |n| *n == interval.bytes(write_observer::WriteKind::WalBody) as f64
                    ),
                    "SQLite WAL body bytes do not conserve store_bytes_written"
                );
                Some(interval)
            }
            (None, None) => None,
            _ => anyhow::bail!("SQLite attribution source changed across the boundary"),
        };
        let ipc = match (&before.ipc, &self.ipc) {
            (Some(before), Some(after)) => {
                let interval = after
                    .difference(before)
                    .ok_or_else(|| anyhow::anyhow!("incomplete IPC attribution"))?;
                let total = interval.totals();
                ensure!(
                    metrics.get("ipc_frames") == Some(&(total.frames as f64))
                        && metrics.get("ipc_bytes") == Some(&(total.bytes as f64)),
                    "IPC kinds do not conserve ipc_frames and ipc_bytes"
                );
                Some(interval.into())
            }
            (None, None) => None,
            _ => anyhow::bail!("IPC attribution source changed across the boundary"),
        };
        Ok(Report { sqlite, ipc })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// TRAFFIC-CONSERVATION: report admission refuses a boundary loss,
    /// overflow or residual even if the caller supplies plausible totals.
    #[test]
    fn traffic_conservation_refuses_incomplete_or_nonconserving_intervals() {
        let baseline = Reading {
            sqlite: Some(write_observer::Receipt {
                epoch: 1,
                complete: true,
                ..Default::default()
            }),
            ipc: Some(IpcTraffic::default()),
        };
        let metrics = BTreeMap::from([
            ("store_bytes_written".to_owned(), 0.0),
            ("ipc_frames".to_owned(), 0.0),
            ("ipc_bytes".to_owned(), 0.0),
        ]);
        assert!(baseline.difference(&baseline, &metrics).is_ok());
        for (active, overflow, unknown) in [(1, 0, 0), (0, 1, 0), (0, 0, 1)] {
            let mut incomplete = baseline.clone();
            let sqlite = incomplete.sqlite.as_mut().unwrap();
            sqlite.active_operations = active;
            sqlite.overflow_bytes = overflow;
            sqlite.unknown_bytes = unknown;
            assert!(incomplete.difference(&baseline, &metrics).is_err());
            assert!(baseline.difference(&incomplete, &metrics).is_err());
        }
        for name in ["store_bytes_written", "ipc_frames", "ipc_bytes"] {
            let mut wrong = metrics.clone();
            wrong.insert(name.to_owned(), 1.0);
            assert!(baseline.difference(&baseline, &wrong).is_err());
        }
        let mut undecodable = baseline.clone();
        undecodable.ipc.as_mut().unwrap().received[16].frames = 1;
        assert!(undecodable.difference(&baseline, &metrics).is_err());
    }
}
