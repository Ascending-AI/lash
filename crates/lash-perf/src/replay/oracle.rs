//! Independent useful-result checks for the host-shaped compute population.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

/// A bounded part of the complete result, checked outside the compute window.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Oracle {
    Value {
        expected: serde_json::Value,
    },
    RecordKeys {
        records: usize,
        offset: usize,
        keys: usize,
        sorted: bool,
    },
}

impl Oracle {
    pub fn check(&self, value: &serde_json::Value) -> Result<Vec<u64>> {
        match *self {
            Self::Value { ref expected } => {
                ensure!(
                    value == expected,
                    "finished value differs from the independent oracle"
                );
                Ok(Vec::new())
            }
            Self::RecordKeys {
                records,
                offset,
                keys,
                sorted,
            } => {
                ensure!(
                    keys > 0 && offset <= records && keys <= records - offset,
                    "record oracle range is outside its population"
                );
                ensure!(
                    value["count"].as_f64() == Some(records as f64),
                    "record count differs from {records}"
                );
                let actual = value["keys"]
                    .as_array()
                    .context("record oracle requires keys")?;
                ensure!(
                    actual.len() == keys,
                    "record key chunk length differs from {keys}"
                );
                // Generate the mathematical input independently of the guest,
                // then use Rust's integer sort rather than the VM comparator.
                let mut expected: Vec<_> = (0..records)
                    .map(|i| (i as u64 * 7919) % records as u64)
                    .collect();
                if sorted {
                    expected.sort_unstable();
                }
                let expected = &expected[offset..offset + keys];
                let actual = actual
                    .iter()
                    .map(|key| key.as_f64().context("nonnumeric record key"))
                    .collect::<Result<Vec<_>>>()?;
                for position in [0, keys / 2, keys - 1] {
                    ensure!(
                        actual[position] == expected[position] as f64,
                        "record key at {} differs from independent oracle",
                        offset + position
                    );
                }
                ensure!(
                    actual
                        .iter()
                        .zip(expected)
                        .all(|(actual, expected)| *actual == *expected as f64),
                    "record keys differ from the independent oracle"
                );
                ensure!(
                    !sorted || actual.windows(2).all(|pair| pair[0] <= pair[1]),
                    "record keys are not sorted"
                );
                // Every element has been checked against the exact integer
                // sequence, including each boundary between adjacent chunks.
                Ok(actual.into_iter().map(|key| key as u64).collect())
            }
        }
    }
}
