//! The RLM protocol's session config owner (FIG-4379).
//!
//! A session records its RLM namespace once, at creation: the creator's
//! stated facts, the presentation format the prompt is written against
//! (`Markdown` for a root session, `RawFinalValue` for a child) when the
//! creator states none, the channel and dialect this host selected
//! (ADR 0096), and the behaviour this host's configuration states
//! ([`RlmRecordedBehaviour`], FIG-4398). Every open delivers the namespace
//! unchanged.
//!
//! The render preferences are the one setting a session may change, through
//! [`SetRlmRender`]. The termination and the final-answer format are fixed at
//! creation: no command changes them, and a turn restates them through its
//! run's protocol turn options instead. The channel, the dialect and the
//! behaviour are the session's pins: a candidate that changes any of them is
//! refused.

use std::sync::{Arc, OnceLock};

use lash_core::facade_support::JsonSchema;
use lash_core::plugin::{
    CandidateFacts, ConfigCommand, ConfigOwner, ConfigRegistrar, ConfigRegistrationError,
    CreationFacts, OwnerChange,
};
use lash_render::RenderParamsPatch;
use lash_rlm_types::{RlmCreateExtras, RlmFinalAnswerFormat, RlmRenderPatch, RlmTermination};

use super::RlmProtocolPluginConfig;
use super::channel::RlmChannel;
use super::config::RlmRecordedBehaviour;

/// The identity of the RLM owner's reducers.
pub const RLM_CONFIG_IMPLEMENTATION: &str = "lash-rlm-config:1";

/// The RLM namespace a session records.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
pub struct RlmRecordedConfig {
    /// The render preferences the session's prompts render values with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Value>")]
    pub render: Option<RlmRenderPatch>,
    /// The session-wide termination requirement. Absence is the `Natural`
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Value>")]
    pub termination: Option<RlmTermination>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Value>")]
    pub final_answer_format: Option<RlmFinalAnswerFormat>,
    /// The channel the session's programs run over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub channel: Option<RlmChannel>,
    /// The language id of the session's dialect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect: Option<String>,
    /// The behaviour the session's driver, prompt and interpreter run under.
    #[schemars(with = "serde_json::Value")]
    pub behaviour: RlmRecordedBehaviour,
}

/// What a creator states for the RLM namespace.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(transparent)]
pub struct RlmCreateConfig(#[schemars(with = "serde_json::Value")] pub RlmCreateExtras);

/// Why the RLM owner refused a candidate.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RlmConfigRefusal {
    /// The candidate changes a pin the session recorded at creation.
    PinChanged {
        pin: String,
        recorded: Option<String>,
        candidate: Option<String>,
    },
    /// The creating deployment has not declared whether it has process
    /// lifecycle, so the durable-sleep ability a new session records is
    /// unknown: a wiring fault of the deployment, never a default.
    ProcessLifecycleUndeclared,
}

impl std::fmt::Display for RlmConfigRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PinChanged {
                pin,
                recorded,
                candidate,
            } => write!(
                formatter,
                "the session's RLM {pin} is recorded as {} and cannot become {}",
                recorded.as_deref().unwrap_or("nothing"),
                candidate.as_deref().unwrap_or("nothing"),
            ),
            Self::ProcessLifecycleUndeclared => formatter.write_str(
                "the RLM protocol factory has not recorded whether process lifecycle is \
                 available, so a new session's abilities are unknown",
            ),
        }
    }
}

/// The RLM protocol's config owner, holding the channel and dialect this
/// host selected and the configuration a new session's behaviour is created
/// from.
#[derive(Clone, Debug)]
pub struct RlmConfigOwner {
    pub(crate) channel: RlmChannel,
    pub(crate) dialect: &'static str,
    pub(crate) config: RlmProtocolPluginConfig,
    /// The factory's process-lifecycle recording, shared: the owner is
    /// registered before the deployment declares it.
    pub(crate) process_lifecycle: Arc<OnceLock<bool>>,
}

impl ConfigOwner for RlmConfigOwner {
    type Create = RlmCreateConfig;
    type Recorded = RlmRecordedConfig;
    type Refusal = RlmConfigRefusal;

    fn implementation(&self) -> &str {
        RLM_CONFIG_IMPLEMENTATION
    }

    /// The creator's stated facts, the presentation format a root or child
    /// session defaults to, and this host's channel, dialect and configured
    /// behaviour. A child inherits nothing from its parent's namespace.
    fn create(
        &self,
        input: Option<RlmCreateConfig>,
        facts: CreationFacts<'_, RlmRecordedConfig>,
    ) -> Result<Option<RlmRecordedConfig>, RlmConfigRefusal> {
        let stated = input.unwrap_or_default().0;
        let process_lifecycle = *self
            .process_lifecycle
            .get()
            .ok_or(RlmConfigRefusal::ProcessLifecycleUndeclared)?;
        let final_answer_format = stated.final_answer_format.unwrap_or({
            if facts.is_root_session {
                RlmFinalAnswerFormat::Markdown
            } else {
                RlmFinalAnswerFormat::RawFinalValue
            }
        });
        Ok(Some(RlmRecordedConfig {
            render: stated.render,
            termination: stated.termination,
            final_answer_format: Some(final_answer_format),
            channel: Some(self.channel),
            dialect: Some(self.dialect.to_string()),
            behaviour: self.config.recorded_behaviour(process_lifecycle),
        }))
    }

    /// A candidate keeps the channel, dialect and behaviour its base
    /// recorded.
    fn validate(
        &self,
        value: &RlmRecordedConfig,
        base: Option<&RlmRecordedConfig>,
        _facts: &CandidateFacts<'_>,
    ) -> Result<(), RlmConfigRefusal> {
        let Some(base) = base else {
            return Ok(());
        };
        if value.channel != base.channel {
            return Err(RlmConfigRefusal::PinChanged {
                pin: "channel".to_string(),
                recorded: base.channel.map(|channel| channel.as_str().to_string()),
                candidate: value.channel.map(|channel| channel.as_str().to_string()),
            });
        }
        if value.dialect != base.dialect {
            return Err(RlmConfigRefusal::PinChanged {
                pin: "dialect".to_string(),
                recorded: base.dialect.clone(),
                candidate: value.dialect.clone(),
            });
        }
        if value.behaviour != base.behaviour {
            let spelled = |behaviour: &RlmRecordedBehaviour| serde_json::to_string(behaviour).ok();
            return Err(RlmConfigRefusal::PinChanged {
                pin: "behaviour".to_string(),
                recorded: spelled(&base.behaviour),
                candidate: spelled(&value.behaviour),
            });
        }
        Ok(())
    }
}

/// Replace the session's render preferences, whole: the print and preview
/// parameters its prompts render values with. Both empty clears them, and
/// the protocol's configured render applies.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
pub struct SetRlmRender {
    #[serde(default, skip_serializing_if = "RenderParamsPatch::is_empty")]
    #[schemars(with = "serde_json::Value")]
    pub print: RenderParamsPatch,
    #[serde(default, skip_serializing_if = "RenderParamsPatch::is_empty")]
    #[schemars(with = "serde_json::Value")]
    pub preview: RenderParamsPatch,
}

impl ConfigCommand for SetRlmRender {
    type Owner = RlmConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_render";
}

/// The RLM owner's registration: the owner and its one command.
pub(crate) fn register(
    registrar: &mut ConfigRegistrar,
    owner: RlmConfigOwner,
) -> Result<(), ConfigRegistrationError> {
    registrar.owner(owner)?;
    registrar.command::<SetRlmRender>(|recorded, command| {
        let render = RlmRenderPatch {
            print: command.print,
            preview: command.preview,
        };
        let empty = render.print.is_empty() && render.preview.is_empty();
        Ok(OwnerChange {
            recorded: RlmRecordedConfig {
                render: (!empty).then_some(render),
                ..recorded.clone()
            },
            output: (),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RlmProtocolPluginConfig {
        RlmProtocolPluginConfig::builder()
            .channel(RlmChannel::Cell)
            .instruction_limit(crate::InstructionBound::instructions(1000))
            .memory_limit(crate::MemoryBound::mebibytes(1))
            .build()
    }

    fn owner_with(process_lifecycle: Option<bool>) -> RlmConfigOwner {
        let declared = OnceLock::new();
        if let Some(process_lifecycle) = process_lifecycle {
            declared.set(process_lifecycle).expect("first declaration");
        }
        RlmConfigOwner {
            channel: RlmChannel::Cell,
            dialect: "typescript",
            config: config(),
            process_lifecycle: Arc::new(declared),
        }
    }

    fn owner() -> RlmConfigOwner {
        owner_with(Some(false))
    }

    fn created(input: Option<RlmCreateExtras>, is_root_session: bool) -> RlmRecordedConfig {
        owner()
            .create(
                input.map(RlmCreateConfig),
                CreationFacts {
                    parent: None,
                    is_root_session,
                },
            )
            .expect("create")
            .expect("the RLM owner always records its namespace")
    }

    fn facts_for<T>(check: impl FnOnce(&CandidateFacts<'_>) -> T) -> T {
        let config = lash_core::PersistedSessionConfig::from(&lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ));
        let core = lash_core::CoreConfig::of(&config);
        check(&CandidateFacts {
            core: &core,
            plugin_config: &config.plugin_config,
        })
    }

    /// A root session defaults to `Markdown` and a child to `RawFinalValue`,
    /// and every session records this host's channel and dialect.
    #[test]
    fn creation_defaults_the_format_by_lineage_and_records_the_pins() {
        let root = created(None, true);
        assert_eq!(
            root.final_answer_format,
            Some(RlmFinalAnswerFormat::Markdown)
        );
        assert_eq!(root.channel, Some(RlmChannel::Cell));
        assert_eq!(root.dialect.as_deref(), Some("typescript"));
        assert_eq!(root.behaviour, config().recorded_behaviour(false));
        assert_eq!(
            created(None, false).final_answer_format,
            Some(RlmFinalAnswerFormat::RawFinalValue)
        );
    }

    /// Stated facts are recorded as stated.
    #[test]
    fn creation_records_the_stated_facts() {
        let recorded = created(
            Some(RlmCreateExtras {
                termination: Some(RlmTermination::FinishRequired { schema: None }),
                final_answer_format: Some(RlmFinalAnswerFormat::RawFinalValue),
                render: None,
            }),
            true,
        );
        assert_eq!(
            recorded.termination,
            Some(RlmTermination::FinishRequired { schema: None })
        );
        assert_eq!(
            recorded.final_answer_format,
            Some(RlmFinalAnswerFormat::RawFinalValue)
        );
    }

    /// A create request cannot choose the dialect: the host selects it where
    /// it constructs the protocol, and the create contract denies the key.
    #[test]
    fn creation_input_naming_a_dialect_does_not_decode() {
        let error = serde_json::from_value::<RlmCreateConfig>(
            serde_json::json!({ "dialect": "typescript" }),
        )
        .expect_err("the create contract carries no language choice");
        assert!(error.to_string().contains("dialect"), "{error}");
    }

    /// `SetRlmRender` resolves through the registry the factory registers: it
    /// replaces the render whole and leaves every other recorded fact alone;
    /// both parameter sets empty clears it.
    #[test]
    fn set_render_replaces_the_render_and_keeps_the_facts() {
        let factory = crate::RlmProtocolPluginFactory::new(
            config(),
            std::sync::Arc::new(crate::TypescriptDialect),
            &crate::testing::sqlite_recording_backend_blocking().clone(),
        )
        .with_process_lifecycle(false);
        let registry =
            lash_core::ConfigRegistry::build(&[std::sync::Arc::new(factory)]).expect("registry");
        let mut config = lash_core::PersistedSessionConfig::from(&lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ));
        config.plugin_config = registry
            .resolve_creation(
                Some(crate::RLM_PROTOCOL_PLUGIN_ID),
                &lash_core::PluginOptions::typed(
                    crate::RLM_PROTOCOL_PLUGIN_ID,
                    RlmCreateExtras {
                        termination: Some(RlmTermination::FinishRequired { schema: None }),
                        ..RlmCreateExtras::default()
                    },
                )
                .expect("options"),
                None,
                true,
            )
            .expect("creation");
        let recorded = |config: &lash_core::PersistedSessionConfig| {
            serde_json::from_value::<RlmRecordedConfig>(
                config
                    .plugin_config
                    .get(crate::RLM_PROTOCOL_PLUGIN_ID)
                    .cloned()
                    .expect("the RLM namespace is recorded"),
            )
            .expect("recorded")
        };
        let base = recorded(&config);
        let apply = |config: &mut lash_core::PersistedSessionConfig, command: SetRlmRender| {
            let transaction = lash_core::ConfigTransaction::of(command);
            let record = registry
                .admit(
                    "set-render",
                    config.config_revision,
                    registry.entries(&transaction).expect("entries"),
                )
                .expect("admitted");
            let outcome = registry
                .resolve(config, &record, &|_, _| Ok(()))
                .publish(config);
            assert!(
                matches!(outcome, lash_core::ConfigTransactionOutcome::Applied { .. }),
                "{outcome:?}"
            );
        };
        let print = RenderParamsPatch {
            max_chars: Some(64),
            ..RenderParamsPatch::default()
        };

        apply(
            &mut config,
            SetRlmRender {
                print: print.clone(),
                preview: RenderParamsPatch::default(),
            },
        );
        assert_eq!(
            recorded(&config),
            RlmRecordedConfig {
                render: Some(RlmRenderPatch {
                    print,
                    preview: RenderParamsPatch::default(),
                }),
                ..base.clone()
            }
        );

        apply(&mut config, SetRlmRender::default());
        assert_eq!(recorded(&config), base);
        assert_eq!(config.config_revision, 2);
    }

    /// A candidate that changes the channel, the dialect or the behaviour
    /// is refused typed; one that keeps them is admitted.
    #[test]
    fn a_candidate_keeps_the_recorded_pins() {
        let base = created(None, true);
        let owner = owner();
        facts_for(|facts| {
            owner
                .validate(&base, Some(&base), facts)
                .expect("the recorded pins are kept");
            let rechanneled = RlmRecordedConfig {
                channel: Some(RlmChannel::NativeTool),
                ..base.clone()
            };
            assert_eq!(
                owner.validate(&rechanneled, Some(&base), facts),
                Err(RlmConfigRefusal::PinChanged {
                    pin: "channel".to_string(),
                    recorded: Some("cell".to_string()),
                    candidate: Some("native_tool".to_string()),
                })
            );
            let redialected = RlmRecordedConfig {
                dialect: Some("python".to_string()),
                ..base.clone()
            };
            assert!(matches!(
                owner.validate(&redialected, Some(&base), facts),
                Err(RlmConfigRefusal::PinChanged { pin, .. }) if pin == "dialect"
            ));
            let rebounded = RlmRecordedConfig {
                behaviour: RlmRecordedBehaviour {
                    instruction_limit: crate::InstructionBound::instructions(7),
                    ..base.behaviour.clone()
                },
                ..base.clone()
            };
            assert!(matches!(
                owner.validate(&rebounded, Some(&base), facts),
                Err(RlmConfigRefusal::PinChanged { pin, .. }) if pin == "behaviour"
            ));
        });
    }

    /// A session records the deployment's behaviour, durable sleep included
    /// when the deployment has process lifecycle; a deployment that never
    /// declared it creates nothing.
    #[test]
    fn creation_records_the_deployments_behaviour() {
        let created_under = |process_lifecycle| {
            owner_with(process_lifecycle).create(
                None,
                CreationFacts {
                    parent: None,
                    is_root_session: true,
                },
            )
        };
        let without = created_under(Some(false))
            .expect("create")
            .expect("recorded");
        assert!(!without.behaviour.lashlang_abilities.sleep);
        assert_eq!(
            without.behaviour.instruction_limit,
            crate::InstructionBound::instructions(1000)
        );
        let with = created_under(Some(true))
            .expect("create")
            .expect("recorded");
        assert!(with.behaviour.lashlang_abilities.sleep);
        assert_eq!(
            created_under(None).expect_err("undeclared lifecycle"),
            RlmConfigRefusal::ProcessLifecycleUndeclared
        );
    }
}
