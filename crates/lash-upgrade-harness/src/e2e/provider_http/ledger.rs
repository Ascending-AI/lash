//! Independent outside-effect oracle. The fixture owns this synced append
//! log; killing a Lash host cannot erase its acceptance or dedup identity.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectDelivery {
    pub owner: String,
    pub run: String,
    pub call_id: String,
    pub attempt: u32,
    pub payload: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectAcceptance {
    pub delivery: EffectDelivery,
    pub mutated: bool,
    pub result: Value,
}

/// Dedup is by the logical call ID, not attempt ordinal: a reported retry
/// may advance the ordinal while repeating the same external intent.
pub struct EffectLedger {
    file: File,
    accepted: BTreeMap<String, EffectAcceptance>,
    deliveries: Vec<EffectAcceptance>,
    poisoned: bool,
}

impl EffectLedger {
    /// Reopen validates every persisted delivery. A partial/corrupt log is
    /// missing evidence, never permission to repeat the outside mutation.
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        let mut bytes = String::new();
        file.read_to_string(&mut bytes)?;
        ensure!(
            bytes.is_empty() || bytes.ends_with('\n'),
            "incomplete effect ledger"
        );
        let mut ledger = Self {
            file,
            accepted: BTreeMap::new(),
            deliveries: Vec::new(),
            poisoned: false,
        };
        for line in bytes.lines() {
            let acceptance: EffectAcceptance = serde_json::from_str(line)?;
            let expected = ledger.propose(&acceptance.delivery)?;
            ensure!(acceptance == expected, "effect ledger acceptance changed");
            ledger.apply(acceptance);
        }
        Ok(ledger)
    }

    fn propose(&self, delivery: &EffectDelivery) -> Result<EffectAcceptance> {
        ensure!(
            !self.poisoned,
            "effect ledger write failed; reopen required"
        );
        ensure!(
            delivery.attempt > 0,
            "outside effect attempt ordinals start at one"
        );
        ensure!(
            [&delivery.owner, &delivery.run, &delivery.call_id]
                .iter()
                .all(|part| !part.is_empty()),
            "outside effect requires owner, Run and call identity"
        );
        let existing = self.accepted.get(&delivery.call_id);
        if let Some(existing) = existing {
            ensure!(
                existing.delivery.owner == delivery.owner
                    && existing.delivery.run == delivery.run
                    && existing.delivery.payload == delivery.payload,
                "outside effect call ID reused with changed owner, Run or content"
            );
        }
        Ok(EffectAcceptance {
            delivery: delivery.clone(),
            mutated: existing.is_none(),
            result: existing.map_or_else(
                || serde_json::json!({ "accepted": delivery.payload }),
                |existing| existing.result.clone(),
            ),
        })
    }

    fn apply(&mut self, acceptance: EffectAcceptance) {
        self.accepted
            .entry(acceptance.delivery.call_id.clone())
            .or_insert_with(|| acceptance.clone());
        self.deliveries.push(acceptance);
    }

    /// The durable acceptance precedes the HTTP reply and accepted barrier.
    /// A failed write poisons this instance, so it cannot append beyond an
    /// ambiguous acceptance in the same process.
    pub fn accept(&mut self, delivery: EffectDelivery) -> Result<EffectAcceptance> {
        let acceptance = self.propose(&delivery)?;
        let mut bytes = serde_json::to_vec(&acceptance)?;
        bytes.push(b'\n');
        if let Err(error) = self
            .file
            .write_all(&bytes)
            .and_then(|()| self.file.sync_all())
        {
            self.poisoned = true;
            return Err(error).context("persist outside-effect acceptance");
        }
        self.apply(acceptance.clone());
        Ok(acceptance)
    }

    pub fn deliveries(&self) -> &[EffectAcceptance] {
        &self.deliveries
    }

    pub fn mutation_count(&self) -> usize {
        self.accepted.len()
    }
}
