//! Independent useful-result checks for the host-shaped compute population.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

/// An oracle named by a verification turn, separate from the compute window.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Oracle {
    RecordKeys { records: usize, sorted: bool },
}

impl Oracle {
    pub fn check(&self, value: &serde_json::Value) -> Result<()> {
        match *self {
            Self::RecordKeys { records, sorted } => {
                ensure!(
                    records > 0,
                    "record oracle must check a nonempty population"
                );
                let keys = value
                    .as_array()
                    .context("record oracle requires all keys")?;
                ensure!(
                    keys.len() == records,
                    "record count: {} != {records}",
                    keys.len()
                );
                // Generate the mathematical input independently of the guest,
                // then use Rust's integer sort rather than the VM comparator.
                let mut expected: Vec<_> = (0..records)
                    .map(|i| (i as u64 * 7919) % records as u64)
                    .collect();
                if sorted {
                    expected.sort_unstable();
                }
                let actual = keys
                    .iter()
                    .map(|key| key.as_f64().context("nonnumeric record key"))
                    .collect::<Result<Vec<_>>>()?;
                for position in [0, records / 2, records.saturating_sub(1)] {
                    ensure!(
                        actual.get(position).copied()
                            == expected.get(position).map(|key| *key as f64),
                        "record key at {position} differs from the independent oracle"
                    );
                }
                ensure!(
                    actual
                        .iter()
                        .zip(&expected)
                        .all(|(actual, expected)| *actual == *expected as f64),
                    "record keys differ from the independent oracle"
                );
                ensure!(
                    !sorted || actual.windows(2).all(|pair| pair[0] <= pair[1]),
                    "record keys are not sorted"
                );
                Ok(())
            }
        }
    }
}
