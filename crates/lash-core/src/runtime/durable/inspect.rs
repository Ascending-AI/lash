//! Read an open turn through the same builder and replay path as recovery.

use crate::facade_support::RuntimeSessionStateFacadeOps as _;
use std::sync::Arc;

use super::session::{TurnError, TurnRestoreError};
use crate::runtime::LashRuntime;
use lash_vocabulary::identity::TurnId;

impl LashRuntime {
    /// Rebuild an open turn for inspection from committed rows.
    /// This needs no serving claim and never executes an effect or a plugin
    /// decision. A retired turn has no open-turn records.
    ///
    /// # Errors
    ///
    /// Store refusals, malformed build inputs, or the same typed replay
    /// divergence that prevents an owner from resuming the turn.
    pub async fn inspect_open_turn(
        &self,
        run: &TurnId,
    ) -> Result<Option<crate::TurnMachine>, TurnError> {
        let backend = self.host.core.backend();
        let reads = backend.durable();
        let session = &self.state.session_id;
        let Some(phase) = super::phase_root::read_phase(reads.as_ref(), session, run).await? else {
            return Ok(None);
        };
        let undecodable = |reason| TurnRestoreError::Undecodable {
            run: run.clone(),
            reason,
        };
        let journal = super::turn_build::read_journal(reads.turn_journal(session, run).await?)
            .map_err(undecodable)?;
        if journal.inputs.len() < phase.journal_inputs
            && super::phase_root::read_phase(reads.as_ref(), session, run)
                .await?
                .is_none()
        {
            return Ok(None);
        }
        let build = journal
            .build
            .ok_or_else(|| undecodable("the turn's journal records no build inputs".to_owned()))?;
        let config = phase
            .config
            .ok_or_else(|| undecodable("the turn records no run configuration".to_owned()))?;
        let pin = phase
            .window_pin
            .ok_or_else(|| undecodable("the turn records no window pin".to_owned()))?;
        let mut state = if super::head::window_pin(&self.state)
            .map_err(|error| undecodable(error.to_string()))?
            == pin
        {
            self.state.clone()
        } else {
            let head = serde_json::from_str(pin.as_str())
                .map_err(|error| undecodable(error.to_string()))?;
            let store = super::head::session_store(backend, session).await?;
            super::head::load_state(
                &store,
                session,
                crate::store::WindowSelector::Admitted(head),
            )
            .await?
        };
        let window = super::head::load_window(backend, session, &pin).await?;
        state.install_run_view(config.run());
        let policy = self
            .host
            .resolve_session_policy(session, state.effective_policy().clone())
            .map_err(crate::runtime::turn_config::llm_profile_unconfigured)?;
        let appends = crate::runtime::turn_commit_draft::TurnGraphAppendDraft::from_resident_state(
            &state,
            Arc::clone(&self.host.core.clock),
        );
        for records in &build.pressure {
            appends
                .record(session, records)
                .map_err(|error| undecodable(error.to_string()))?;
        }
        let prepared = crate::PluginSession::apply_before_turn(phase.before_turn);
        appends
            .apply_session_contributions(session, &prepared.session)
            .map_err(|error| undecodable(error.to_string()))?;
        let mut boundary = crate::runtime::TurnBoundary::from_state_with_graph_appends(
            state.clone(),
            Arc::clone(&self.host.core.clock),
            state.turn_scope(run),
            self.host.core.durability.commit_budget,
            appends,
        );
        boundary.apply_prepared_messages(&window.then(build.messages.clone()));
        // Prepare protocol implementations, never the removed plugin's decision
        // callbacks. The machine builder itself consumes only these inputs.
        self.services
            .plugins
            .prepare_capabilities()
            .map_err(|error| undecodable(error.to_string()))?;
        let preamble = Arc::new(self.services.plugins.protocol_driver().build_preamble(
            crate::ProtocolBuildInput {
                tool_catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(Vec::new())),
                plugin_extensions: self.services.plugins.extensions().clone(),
                writer_formats: Arc::new(crate::protocol_build::FleetWriterFormats(
                    self.fleet_format(),
                )),
            },
        ));
        let syncs = crate::runtime::turn_driver::tool_catalog::recorded_syncs(
            &config,
            &self.services.plugins,
            self.fleet_format(),
            reads.environments().as_ref(),
        )
        .await?;
        let mut machine = crate::runtime::turn_driver::tool_catalog::build_turn_machine(
            crate::runtime::turn_driver::tool_catalog::MachineBasis {
                preamble,
                session_id: session,
                turn_id: run,
                policy: &policy,
                protocol_turn_options: state.effective_protocol_turn_options(),
                head_window: window,
                events: boundary.active_events(),
            },
            &build,
            None,
        )
        .map_err(|divergence| TurnRestoreError::Diverged {
            run: run.clone(),
            divergence,
        })?;
        super::turn_build::replay(
            &mut machine,
            journal.inputs,
            phase.journal_inputs,
            run,
            &|iteration| syncs.get(&iteration).cloned(),
        )?;
        Ok(Some(machine))
    }
}
