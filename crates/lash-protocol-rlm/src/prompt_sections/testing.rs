//! Compose the RLM protocol's sections the way a session composes them, for
//! the protocol's prompt laws.

use std::sync::Arc;

use lash_core::plugin::prompt::{OfferedTools, PromptCall, PromptCatalog, PromptModel};
use lash_core::plugin::{PluginError, PluginRegistrar, SessionPlugin};
use lash_core::prompt_sections::{PromptPlan, PromptPurpose};
use lash_core::testing::prompt::{ComposedPrompt, PromptCutParts};
use lash_rlm_types::RlmTurnOptions;

use super::{RlmPromptFacts, RlmSectionBehaviour, register_sections};
use crate::dialect::SessionDialect;
use crate::plugin::{RLM_PROTOCOL_PLUGIN_ID, RlmChannel, RlmRecordedConfig};

/// The RLM protocol's sections under one behaviour, as a plugin.
#[derive(Clone)]
pub(crate) struct RlmSections {
    pub(crate) dialect: Arc<SessionDialect>,
    pub(crate) channel: RlmChannel,
    pub(crate) prompt_features: crate::protocol::RlmPromptFeatures,
    pub(crate) discovery: Option<lash_core::ToolDiscovery>,
    pub(crate) budget_tokens: Option<usize>,
}

impl RlmSections {
    /// The TypeScript cell channel with the default features and no budget.
    pub(crate) fn cell(dialect: SessionDialect) -> Self {
        Self {
            dialect: Arc::new(dialect),
            channel: RlmChannel::Cell,
            prompt_features: Default::default(),
            discovery: None,
            budget_tokens: None,
        }
    }

    pub(crate) fn typescript() -> Self {
        Self::cell(SessionDialect::prompt_only(
            Arc::new(crate::dialect::TypescriptDialect),
            lash_lashlang_runtime::LashlangSurface::default(),
        ))
    }
}

impl SessionPlugin for RlmSections {
    fn id(&self) -> &'static str {
        RLM_PROTOCOL_PLUGIN_ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        register_sections(
            reg,
            RlmSectionBehaviour {
                dialect: Arc::clone(&self.dialect),
                channel: self.channel,
                prompt_features: self.prompt_features,
                discovery: self.discovery.clone(),
                budget_tokens: self.budget_tokens,
            },
        )
    }
}

/// One call's committed cut.
pub(crate) struct Call {
    pub(crate) catalog: lash_core::ToolCatalog,
    pub(crate) facts: Option<RlmPromptFacts>,
    pub(crate) options: RlmTurnOptions,
    pub(crate) committed_usage: Option<lash_core::TokenUsage>,
    pub(crate) context_window_tokens: Option<u64>,
    pub(crate) iteration: u32,
    pub(crate) purpose: PromptPurpose,
    pub(crate) plan: PromptPlan,
}

impl Default for Call {
    fn default() -> Self {
        Self {
            catalog: lash_core::ToolCatalog::default(),
            facts: None,
            options: RlmTurnOptions::default(),
            committed_usage: None,
            context_window_tokens: None,
            iteration: 0,
            purpose: PromptPurpose::Turn,
            plan: PromptPlan::default(),
        }
    }
}

/// Compose `call` over the sections `plugins` register, in order.
pub(crate) fn compose(plugins: &[Arc<dyn SessionPlugin>], call: Call) -> ComposedPrompt {
    let recorded = RlmRecordedConfig::read(&RlmRecordedConfig::for_testing(call.options))
        .expect("the recorded namespace reads")
        .expect("the recorded namespace is present");
    let mut config = lash_core::PluginConfig::default();
    config.insert(
        RLM_PROTOCOL_PLUGIN_ID,
        serde_json::to_value(recorded).expect("the recorded namespace encodes"),
    );
    let cut = lash_core::testing::prompt::cut_with(
        PromptCutParts {
            call: PromptCall {
                session_id: lash_core::SessionId::from("rlm-prompt-laws"),
                frame: None,
                run: None,
                turn: None,
                iteration: call.iteration,
                call: call.iteration,
                purpose: call.purpose.clone(),
            },
            config: lash_core::AdmittedPluginConfig::new(config, 0),
            session: None,
            offered: OfferedTools::new(Arc::new(call.catalog), false),
            model: PromptModel {
                profile: None,
                context_window_tokens: call.context_window_tokens,
                committed_usage: call.committed_usage,
            },
            history: Default::default(),
            namespaces: Default::default(),
        },
        call.facts.map(|facts| Arc::new(facts) as _),
    );
    let catalog = PromptCatalog::of_plugins(plugins).expect("the sections register");
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a composition runtime")
        .block_on(lash_core::testing::prompt::compose(
            &catalog,
            &call.plan,
            &call.purpose,
            cut,
        ))
        .expect("the prompt composes")
}

/// The RLM sections alone.
pub(crate) fn compose_rlm(sections: RlmSections, call: Call) -> ComposedPrompt {
    compose(&[Arc::new(sections)], call)
}
