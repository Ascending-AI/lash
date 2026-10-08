//! The RLM protocol's session config owner (FIG-4379).
//!
//! A session records its RLM namespace once, at creation: the creator's
//! stated facts, the channel and dialect this
//! host selected (ADR 0096), and this host's configured behaviour
//! ([`RlmRecordedBehaviour`], FIG-4398). Creation reads no other session: a
//! child records what its creator states, like a root, and only a fork copies
//! a recorded namespace (ADR 0134). Every open delivers the namespace
//! unchanged.
//!
//! A session may change one setting: its render preferences, through
//! [`SetRlmRender`]. Its prompt is not config: the protocol contributes keyed
//! sections, and a host adds or wraps sections of its own (ADR 0133).
//! The termination is fixed at creation: no
//! command changes it, and a turn states it again through its run's options
//! ([`RlmRunOptions`]), which this owner applies over the recorded namespace.
//! The channel, the dialect and the behaviour are the session's pins: a
//! candidate that changes any of them is refused, and a run's options have
//! no field for them.
//!
//! Every reader of the namespace decodes [`RlmRecordedConfig`]: nothing
//! probes or strips its keys (FIG-4652).

use std::sync::{Arc, OnceLock};

use lash_core::facade_support::JsonSchema;
use lash_core::plugin::{
    CandidateFacts, ConfigCommand, ConfigOwner, ConfigRegistrar, ConfigRegistrationError,
    OwnerChange,
};
use lash_render::RenderParamsPatch;
use lash_rlm_types::{RlmCreateExtras, RlmRenderPatch, RlmTermination, RlmTurnOptions};

use super::RlmProtocolPluginConfig;
use super::channel::RlmChannel;
use super::config::RlmRecordedBehaviour;

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

impl RlmRecordedConfig {
    /// The namespace `namespace` holds, read as recorded. `None` for an
    /// empty one: a session that recorded no RLM namespace.
    pub fn read(
        namespace: &lash_core::ProtocolTurnOptions,
    ) -> Result<Option<Self>, lash_core::ProtocolTurnOptionsError> {
        if namespace.is_empty() {
            return Ok(None);
        }
        namespace.decode().map(Some)
    }

    /// What a turn of this session runs under, of what a run may state again.
    pub fn turn_options(&self) -> RlmTurnOptions {
        RlmTurnOptions {
            termination: self.termination.clone(),
            render: self.render.clone(),
        }
    }
}

#[cfg(any(test, feature = "testing"))]
impl RlmRecordedConfig {
    /// The recorded namespace of a session that stated `options`, for a
    /// test that executes the protocol without creating a session: an
    /// unbounded cell-channel behaviour without process lifecycle.
    #[expect(
        clippy::expect_used,
        reason = "a recorded RLM namespace is plain data and always encodes"
    )]
    pub fn for_testing(options: RlmTurnOptions) -> lash_core::ProtocolTurnOptions {
        lash_core::ProtocolTurnOptions::typed(Self {
            render: options.render,
            termination: options.termination,
            channel: Some(RlmChannel::Cell),
            dialect: None,
            behaviour: RlmProtocolPluginConfig::builder()
                .channel(RlmChannel::Cell)
                .instruction_limit(super::InstructionBound::unbounded())
                .memory_limit(super::MemoryBound::unbounded())
                .build()
                .recorded_behaviour(),
        })
        .expect("the recorded namespace encodes")
    }
}

/// What a creator states for the RLM namespace.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(transparent)]
pub struct RlmCreateConfig(#[schemars(with = "serde_json::Value")] pub RlmCreateExtras);

/// What a run states for the RLM namespace: termination and render preferences.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(transparent)]
pub struct RlmRunOptions(#[schemars(with = "serde_json::Value")] pub RlmTurnOptions);

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
    /// lifecycle: a wiring fault of the deployment, independent of the
    /// host-authored sleep choice.
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

/// Why the RLM protocol resolved no render for a run.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RlmRenderRefusal {
    /// The resolved render parameters did not encode as a record.
    Unencodable { message: String },
}

impl std::fmt::Display for RlmRenderRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unencodable { message } => {
                write!(
                    formatter,
                    "the resolved RLM render does not encode: {message}"
                )
            }
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
    type RunOptions = RlmRunOptions;

    /// The creator's stated facts, this host's channel and dialect, and its
    /// behaviour (FIG-4527). Nothing is read from another session.
    fn create(
        &self,
        input: Option<RlmCreateConfig>,
    ) -> Result<Option<RlmRecordedConfig>, RlmConfigRefusal> {
        let stated = input.unwrap_or_default().0;
        self.process_lifecycle
            .get()
            .ok_or(RlmConfigRefusal::ProcessLifecycleUndeclared)?;
        let behaviour = self.config.recorded_behaviour();
        Ok(Some(RlmRecordedConfig {
            render: stated.render,
            termination: stated.termination,
            channel: Some(self.channel),
            dialect: Some(self.dialect.to_string()),
            behaviour,
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

    /// A stated termination replaces the recorded
    /// one for the run, and stated render preferences apply field by field
    /// over the recorded ones. The pins stay as recorded: a run's options
    /// cannot name them.
    fn apply_run_options(
        &self,
        recorded: &RlmRecordedConfig,
        options: RlmRunOptions,
    ) -> Result<RlmRecordedConfig, RlmConfigRefusal> {
        let RlmTurnOptions {
            termination,
            render,
        } = options.0;
        let render = match (render, recorded.render.as_ref()) {
            (Some(stated), Some(recorded)) => Some(RlmRenderPatch {
                print: stated.print.over(&recorded.print),
                preview: stated.preview.over(&recorded.preview),
            }),
            (stated, recorded) => stated.or_else(|| recorded.cloned()),
        };
        Ok(RlmRecordedConfig {
            render,
            termination: termination.or_else(|| recorded.termination.clone()),
            ..recorded.clone()
        })
    }
}

/// Replace the session's render preferences, whole: the print and preview
/// parameters its prompts render values with. Both empty clears them, and
/// the render the session recorded at creation applies.
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

/// The RLM owner's registration: the owner and its commands.
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

    fn created(input: Option<RlmCreateExtras>) -> RlmRecordedConfig {
        owner()
            .create(input.map(RlmCreateConfig))
            .expect("create")
            .expect("the RLM owner always records its namespace")
    }

    fn facts_for<T>(check: impl FnOnce(&CandidateFacts<'_>) -> T) -> T {
        let config = lash_core::PersistedSessionConfig::from_policy(
            &lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ),
            lash_core::SessionToolAccess::ambient(),
        );
        let core = lash_core::CoreConfig::of(&config);
        check(&CandidateFacts {
            core: &core,
            plugin_config: &config.plugin_config,
        })
    }

    /// FIG-4652: the owner lays a run's options over its recorded namespace.
    /// Stated render fields apply over the session's field by field, a
    /// stated termination replaces the session's for the run, and nothing a
    /// run cannot state moves.
    #[test]
    fn run_options_apply_over_the_recorded_namespace_field_by_field() {
        let print = |max_chars, max_depth| RlmRenderPatch {
            print: RenderParamsPatch {
                max_chars,
                max_depth,
                ..Default::default()
            },
            ..Default::default()
        };
        let recorded = created(Some(RlmCreateExtras {
            render: Some(print(Some(5), None)),
            termination: Some(RlmTermination::FinishRequired { schema: None }),
        }));
        let applied = owner()
            .apply_run_options(
                &recorded,
                RlmRunOptions(RlmTurnOptions {
                    render: Some(print(None, Some(1))),
                    ..RlmTurnOptions::default()
                }),
            )
            .expect("the run's render options apply");
        assert_eq!(applied.render, Some(print(Some(5), Some(1))));
        assert_eq!(
            applied,
            RlmRecordedConfig {
                render: applied.render.clone(),
                ..recorded.clone()
            },
            "nothing the run did not state moved"
        );
        let resolved = crate::render::ResolvedRlmRender::resolve(
            &print(Some(9), Some(4)),
            &applied.render.clone().expect("render patch"),
        );
        assert_eq!(resolved.print.max_chars, 5);
        assert_eq!(resolved.print.max_depth, 1);

        let run = owner()
            .apply_run_options(
                &recorded,
                RlmRunOptions(RlmTurnOptions {
                    termination: Some(RlmTermination::Natural { schema: None }),
                    render: None,
                }),
            )
            .expect("the run's termination applies");
        assert_eq!(
            run.termination,
            Some(RlmTermination::Natural { schema: None })
        );
        assert_eq!(run.render, recorded.render);
        assert_eq!(
            owner()
                .apply_run_options(&recorded, RlmRunOptions::default())
                .expect("empty options apply"),
            recorded,
            "a run that states nothing runs under the recorded namespace"
        );
    }

    /// FIG-4652: a run's options have no field for a pin, so a payload
    /// naming one does not decode, even when it repeats the
    /// recorded value. Every recorded field a run cannot state is covered:
    /// the list is the recorded namespace's own keys.
    #[test]
    fn run_options_have_no_field_for_a_pin() {
        let recorded = serde_json::to_value(created(Some(RlmCreateExtras {
            render: Some(RlmRenderPatch::default()),
            termination: Some(RlmTermination::Natural { schema: None }),
        })))
        .expect("the recorded namespace encodes");
        let stated: std::collections::BTreeSet<&str> = recorded
            .as_object()
            .expect("the namespace is an object")
            .iter()
            .filter(|(key, value)| {
                serde_json::from_value::<RlmRunOptions>(serde_json::json!({ *key: value })).is_ok()
            })
            .map(|(key, _)| key.as_str())
            .collect();
        assert_eq!(
            stated,
            std::collections::BTreeSet::from(["render", "termination"]),
            "a run states only its termination and its render"
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
        let mut config = lash_core::PersistedSessionConfig::from_policy(
            &lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ),
            lash_core::SessionToolAccess::ambient(),
        );
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
                &lash_core::store::plugin_writers::PluginAdmission::default(),
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
                .resolve(
                    config,
                    &record,
                    &lash_core::EmptyLlmProfiles,
                    &lash_core::store::plugin_writers::PluginAdmission::default(),
                    &lash_core::plugin::prompt::PromptCatalog::default(),
                )
                .expect("the recorded config reads")
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
        let base = created(None);
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

    /// Creation cannot record behaviour before the deployment surface is declared.
    #[test]
    fn creation_refuses_an_undeclared_deployment_surface() {
        assert_eq!(
            owner_with(None)
                .create(None)
                .expect_err("undeclared lifecycle"),
            RlmConfigRefusal::ProcessLifecycleUndeclared
        );
    }

    /// The render a deployment configures is recorded behaviour: a session
    /// opened on a deployment configured otherwise resolves its render over
    /// the recorded one (FIG-4527).
    #[test]
    fn the_configured_render_is_recorded_behaviour() {
        let mut creating = config();
        creating.render.print.max_chars = Some(11);
        let recorded = creating.recorded_behaviour();
        assert_eq!(recorded.render, creating.render);
        let opened = config().under_recorded_behaviour(&recorded);
        assert_eq!(opened.render, creating.render);
        assert_ne!(opened.render, config().render);
    }
}
