//! Native acknowledgements belong to the case; post-fault facts belong to Lash.
use crate::e2e::evidence::{DecodedRecord, NativeRunFact};
use anyhow::{Result, ensure};
use lash_core::testing::EffectLayer;
use lash_core::{RuntimeEffectController, RuntimeEffectControllerError};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

mod engine;
pub(super) use engine::ObservedEngine;

pub(super) struct NativeCapture {
    path: PathBuf,
    records: Arc<Mutex<BTreeMap<String, NativeRunFact>>>,
    owner: Option<(lash_core::SessionId, lash_core::TurnId)>,
}
impl NativeCapture {
    pub(super) fn new(path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            path,
            records: Arc::new(Mutex::new(BTreeMap::new())),
            owner: None,
        })
    }
    pub(super) fn for_run(
        &self,
        session: lash_core::SessionId,
        run: lash_core::TurnId,
    ) -> Arc<Self> {
        Arc::new(Self {
            path: self.path.clone(),
            records: self.records.clone(),
            owner: Some((session, run)),
        })
    }

    fn retain(
        &self,
        name: String,
        record: DecodedRecord,
    ) -> Result<(), RuntimeEffectControllerError> {
        let mut records = self
            .records
            .lock()
            .map_err(|_| capture_error("native capture poisoned"))?;
        let (session, run) = self
            .owner
            .as_ref()
            .ok_or_else(|| capture_error("native acknowledgement has no admitted Run"))?;
        let fact = NativeRunFact {
            session: session.clone(),
            run: run.clone(),
            name: name.clone(),
            record,
            artifact: self.path.display().to_string(),
        };
        if let Some(previous) = records.get(&name) {
            if serde_json::to_value(previous).map_err(capture_error)?
                != serde_json::to_value(&fact).map_err(capture_error)?
            {
                return Err(capture_error(format!(
                    "native acknowledgement changed at {name}"
                )));
            }
        } else {
            records.insert(name, fact);
        }
        let bytes =
            serde_json::to_vec(&records.values().collect::<Vec<_>>()).map_err(capture_error)?;
        super::super::write_atomically(&self.path, &bytes).map_err(capture_error)
    }
}
fn capture_error(error: impl std::fmt::Display) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        lash_core::RuntimeErrorCode::EngineEffectController,
        error.to_string(),
    )
}
#[async_trait::async_trait]
impl EffectLayer for NativeCapture {
    async fn record_run_record(
        &self,
        inner: &dyn RuntimeEffectController,
        name: String,
        step: lash_core::RunRecordStep<'_>,
    ) -> Result<lash_core::tool_run::RunJournalEntry, RuntimeEffectControllerError> {
        let entry = inner.record_run_record(name.clone(), step).await?;
        self.retain(name, DecodedRecord::Run(entry.clone()))?;
        Ok(entry)
    }
    async fn record_run_schedule(
        &self,
        inner: &dyn RuntimeEffectController,
        name: String,
        step: lash_core::RunRecordStep<'_>,
    ) -> Result<lash_core::tool_run::RunJournalEntry, RuntimeEffectControllerError> {
        let entry = inner.record_run_schedule(name.clone(), step).await?;
        self.retain(name, DecodedRecord::Run(entry.clone()))?;
        Ok(entry)
    }
    fn start_run_attempt<'run>(
        &'run self,
        inner: &'run dyn RuntimeEffectController,
        name: String,
        step: lash_core::tool_dispatch::RunAttemptStep<'run>,
    ) -> lash_core::tool_dispatch::RunAttemptHandle<'run> {
        let lash_core::tool_dispatch::RunAttemptHandle { body, result } =
            inner.start_run_attempt(name.clone(), step);
        lash_core::tool_dispatch::RunAttemptHandle {
            body,
            result: Box::pin(async move {
                let entry = result.await?;
                self.retain(name, DecodedRecord::Attempt(Box::new(entry.clone())))?;
                Ok(entry)
            }),
        }
    }
}

pub(super) fn read(directories: &[PathBuf]) -> Result<Vec<NativeRunFact>> {
    let mut records = BTreeMap::new();
    let mut facts = Vec::new();
    for directory in directories {
        let path = directory.join("native-run-records.json");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let captured: Vec<NativeRunFact> = serde_json::from_slice(&bytes)?;
        for fact in captured {
            ensure!(
                fact.artifact == path.display().to_string(),
                "native receipt has another provenance"
            );
            let identity = (fact.session.clone(), fact.run.clone(), fact.name.clone());
            if let Some(previous) = records.get(&identity) {
                ensure!(
                    serde_json::to_value(previous)? == serde_json::to_value(&fact.record)?,
                    "native receipt changed on replay"
                );
            } else {
                records.insert(identity, fact.record.clone());
            }
            facts.push(fact);
        }
    }
    Ok(facts)
}
