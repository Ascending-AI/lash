//! Component state/config conversion postimages, recorded before binding.

use super::activation::Claim as Owned;
use lash_durable::domain::{OwnerKey, ProcessActorRow, ProcessWrite};
use lash_durable::{ActorTx, CommitLabel, DomainWrite, DurableError};
use serde::{Deserialize, Serialize};

use super::activation::{Live, Pass, ProcessActivation, corrupt};
use super::driver::Driver;
use crate::ProcessEnv;
use crate::ProcessEnvRef;
use crate::plugin::PluginState;
use lash_execution_contracts::process::ProcessRecord;

/// The whole state/config postimage of a recorded component format cut.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentFormatCut {
    pub env_ref: ProcessEnvRef,
    pub env: ProcessEnv,
    pub state: PluginState,
}

/// The recorded cut for this pair, replayed without running any converter.
#[doc(hidden)]
pub async fn recorded_component_cut(
    cx: &crate::ActorContext,
    process: &ProcessRecord,
) -> Result<Option<ComponentFormatCut>, DurableError> {
    let view = cx.acknowledged_process_view().await?;
    if view
        .row
        .as_ref()
        .is_none_or(|row| row.process != process.id)
    {
        return Err(corrupt(
            "process composition",
            "the retained view names another process",
        ));
    }
    let row = view.row;
    let driver = Driver::decode(row.as_ref().and_then(|row| row.driver_json.as_deref()))
        .map_err(|error| corrupt("a process component cut", error))?;
    Ok(driver
        .component_formats
        .filter(|cut| Some(&cut.env_ref) == process.env_ref()))
}

/// Reconstruct the complete preimage from original recorded publications.
/// The frontier skips publications already covered by a conversion cut;
/// their original receipts remain unchanged.
#[doc(hidden)]
pub async fn recorded_component_state(
    cx: &crate::ActorContext,
    process: &ProcessRecord,
    env: &ProcessEnv,
    state: Option<PluginState>,
) -> Result<PluginState, DurableError> {
    let view = cx.acknowledged_process_view().await?;
    if view
        .row
        .as_ref()
        .is_none_or(|row| row.process != process.id)
    {
        return Err(corrupt(
            "process composition",
            "the retained view names another process",
        ));
    }
    let row = view.row;
    let driver = Driver::decode(row.as_ref().and_then(|row| row.driver_json.as_deref()))
        .map_err(|error| corrupt("a process namespace checkpoint", error))?;
    let mut state = driver
        .namespace_state
        .or(state)
        .or_else(|| driver.component_formats.map(|cut| cut.state))
        .unwrap_or_default();
    for entry in env.components() {
        state
            .plugins
            .entry(entry.id().to_string())
            .or_insert_with(|| crate::plugin::PluginNamespaceState {
                format_version: entry.config().format_version(),
                ..Default::default()
            });
    }
    Ok(state)
}

impl ProcessActivation {
    /// Commit the conversion postimage and its claim obligations together.
    /// No binder or step has run on this activation before this cut.
    pub(super) async fn component_format_cut(
        &self,
        owned: &Owned<'_>,
        mut tx: ActorTx,
        record: &ProcessRecord,
        row: &ProcessActorRow,
        driver: &Driver,
        live: &mut Live,
    ) -> Result<Result<ActorTx, Pass>, DurableError> {
        if owned.draining() || live.bodies.runtime.built_tools().is_some() {
            return Ok(Ok(tx));
        }
        let cut = match self.steps.convert_components(&owned.cx, record).await {
            Ok(Some(cut)) => cut,
            Ok(None) => return Ok(Ok(tx)),
            Err(lash_execution_contracts::plugin_error::PluginError::Format(refusal)) => {
                return self
                    .park(
                        owned,
                        tx,
                        &lash_execution_contracts::park::ProcessParkReason::ComponentFormatRefused { refusal },
                    )
                    .await
                    .map(Err);
            }
            Err(lash_execution_contracts::plugin_error::PluginError::ProcessEnvRefused {
                refusal,
            }) => {
                return self
                    .park(
                        owned,
                        tx,
                        &lash_execution_contracts::park::ProcessParkReason::EnvRefused { refusal },
                    )
                    .await
                    .map(Err);
            }
            Err(error)
                if error.class()
                    == lash_execution_contracts::plugin_error::PluginErrorClass::Terminal =>
            {
                return self
                    .park(
                        owned,
                        tx,
                        &lash_execution_contracts::park::ProcessParkReason::CompositionUnavailable {
                            message: error.to_string(),
                        },
                    )
                    .await
                    .map(Err);
            }
            Err(error) => {
                return Err(DurableError::Store(lash_durable::StoreFailure {
                    code: None,
                    kind: lash_durable::StoreFailureKind::Unavailable,
                    message: error.to_string(),
                }));
            }
        };
        let environment = crate::plugin::ProcessComponentDeclaration::env_requirements(&cut.env);
        let mut facts = lash_durable::domain::RunFacts::default();
        let runs = driver
            .steps
            .values()
            .map(|step| step.run)
            .collect::<std::collections::BTreeSet<_>>();
        for run in runs {
            facts.extend(
                owned
                    .store()
                    .run_facts_of(
                        &OwnerKey::Process(record.id.clone()),
                        lash_durable::domain::RunSeq(run),
                    )
                    .await?,
            );
        }
        let waits =
            crate::runtime::actor::round::PinnedWaits::read(owned.store().as_ref(), &facts).await?;
        let fold = crate::runtime::actor::round::fold(
            &facts,
            &crate::runtime::actor::round::PolicyView::default(),
            &waits,
        )
        .map_err(|error| corrupt("component cut obligations", error))?;
        let requires = super::step_admission::outstanding_work(
            &record.id,
            environment,
            &live.record,
            driver,
            Some(&fold),
        )
        .requires();
        let mut driver = driver.clone();
        tracing::info!(process_id = %record.id, formats = ?cut.state.plugins.iter().map(|(id, namespace)| (id, namespace.format_version)).collect::<Vec<_>>(), requires = ?requires, "recording component format postimage");
        driver.namespace_state = Some(cut.state.clone());
        driver.component_formats = Some(cut);
        tx.write(DomainWrite::Process(ProcessWrite::ComponentFormats {
            process: record.id.clone(),
            expected_rev: row.state_rev,
            driver_json: driver.encode(),
        }));
        tx.set_requires(requires.clone());
        owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
        Ok(Err(Pass::Again))
    }
}
