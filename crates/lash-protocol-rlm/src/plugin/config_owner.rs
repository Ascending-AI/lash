//! The RLM protocol's session config owner (FIG-4379).
//!
//! A session records its RLM namespace once, at creation: the creator's
//! stated facts, the presentation format the prompt is written against
//! (`Markdown` for a root session, `RawFinalValue` for a child) when the
//! creator states none, the channel and dialect this host selected
//! (ADR 0096), and its behaviour ([`RlmRecordedBehaviour`], FIG-4398): its
//! parent's recorded behaviour for a child, this host's configured behaviour
//! otherwise (FIG-4527). Every open delivers the namespace unchanged.
//!
//! A session may change two settings. Its render preferences, through
//! [`SetRlmRender`]. And its prompt config ([`RlmPrompt`], FIG-4588): the
//! host's share of the system prompt, which a creator states, a child copies
//! from its parent, and [`SetRlmPrompt`] and [`SetRlmPromptContext`] replace.
//! The termination and the final-answer format are fixed at creation: no
//! command changes them, and a turn restates them through its run's options
//! ([`RlmRunOptions`]), which this owner applies over the recorded namespace.
//! The channel, the dialect and the behaviour are the session's pins: a
//! candidate that changes any of them is refused, and a run's options have
//! no field for them or for the prompt.
//!
//! Every reader of the namespace decodes [`RlmRecordedConfig`]: nothing
//! probes or strips its keys (FIG-4652).

use std::sync::{Arc, OnceLock};

use lash_core::facade_support::JsonSchema;
use lash_core::plugin::{
    CandidateFacts, ConfigCommand, ConfigOwner, ConfigRegistrar, ConfigRegistrationError,
    CreationFacts, OwnerChange,
};
use lash_render::RenderParamsPatch;
use lash_rlm_types::{
    RlmCreateExtras, RlmFinalAnswerFormat, RlmPrompt, RlmRenderPatch, RlmTermination,
    RlmTurnOptions,
};

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
    /// The host's share of the session's system prompt: what the protocol
    /// renders around the declarations it generates.
    #[schemars(with = "serde_json::Value")]
    pub prompt: RlmPrompt,
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

    /// What a turn of this session runs under, of what a run may restate.
    pub fn turn_options(&self) -> RlmTurnOptions {
        RlmTurnOptions {
            termination: self.termination.clone(),
            final_answer_format: self.final_answer_format.clone(),
            render: self.render.clone(),
        }
    }
}

#[cfg(any(test, feature = "testing"))]
impl RlmRecordedConfig {
    /// The recorded namespace of a session that stated `options`, for a
    /// test that drives the protocol without creating a session: an
    /// unbounded cell-channel behaviour without process lifecycle, and the
    /// built-in prompt.
    #[expect(
        clippy::expect_used,
        reason = "a recorded RLM namespace is plain data and always encodes"
    )]
    pub fn for_testing(options: RlmTurnOptions) -> lash_core::ProtocolTurnOptions {
        lash_core::ProtocolTurnOptions::typed(Self {
            render: options.render,
            termination: options.termination,
            final_answer_format: options.final_answer_format,
            channel: Some(RlmChannel::Cell),
            dialect: None,
            behaviour: RlmProtocolPluginConfig::builder()
                .channel(RlmChannel::Cell)
                .instruction_limit(super::InstructionBound::unbounded())
                .memory_limit(super::MemoryBound::unbounded())
                .build()
                .recorded_behaviour(false),
            prompt: RlmPrompt::default(),
        })
        .expect("the recorded namespace encodes")
    }
}

/// What a creator states for the RLM namespace.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(transparent)]
pub struct RlmCreateConfig(#[schemars(with = "serde_json::Value")] pub RlmCreateExtras);

/// What a run states for the RLM namespace: its termination, its final
/// answer format and its render preferences.
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

/// Why the RLM protocol resolved no render for a root.
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

    fn implementation(&self) -> &str {
        RLM_CONFIG_IMPLEMENTATION
    }

    /// The creator's stated facts, the presentation format a root or child
    /// session defaults to, this host's channel and dialect, and the
    /// session's behaviour. A child inherits its parent's recorded behaviour,
    /// whatever the host creating it is configured with (FIG-4527); a session
    /// with no recorded parent behaviour records this host's. The prompt is
    /// the creator's stated one; a creator that states none leaves a child
    /// its parent's recorded prompt, and a session with no parent the
    /// built-in default (FIG-4588).
    fn create(
        &self,
        input: Option<RlmCreateConfig>,
        facts: CreationFacts<'_, RlmRecordedConfig>,
    ) -> Result<Option<RlmRecordedConfig>, RlmConfigRefusal> {
        let stated = input.unwrap_or_default().0;
        let behaviour = match facts.parent {
            Some(parent) => parent.behaviour.clone(),
            None => self.config.recorded_behaviour(
                *self
                    .process_lifecycle
                    .get()
                    .ok_or(RlmConfigRefusal::ProcessLifecycleUndeclared)?,
            ),
        };
        let final_answer_format = stated.final_answer_format.unwrap_or({
            if facts.is_root_session {
                RlmFinalAnswerFormat::Markdown
            } else {
                RlmFinalAnswerFormat::RawFinalValue
            }
        });
        let prompt = stated
            .prompt
            .or_else(|| facts.parent.map(|parent| parent.prompt.clone()))
            .unwrap_or_default();
        Ok(Some(RlmRecordedConfig {
            render: stated.render,
            termination: stated.termination,
            final_answer_format: Some(final_answer_format),
            channel: Some(self.channel),
            dialect: Some(self.dialect.to_string()),
            behaviour,
            prompt,
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

    /// A stated termination or final-answer format replaces the recorded
    /// one for the run, and stated render preferences apply field by field
    /// over the recorded ones. The pins and the prompt stay as recorded: a
    /// run's options cannot name them.
    fn apply_run_options(
        &self,
        recorded: &RlmRecordedConfig,
        options: RlmRunOptions,
    ) -> Result<RlmRecordedConfig, RlmConfigRefusal> {
        let RlmTurnOptions {
            termination,
            final_answer_format,
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
            final_answer_format: final_answer_format
                .or_else(|| recorded.final_answer_format.clone()),
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

/// Replace the session's prompt config, whole: the intro, what built-in text
/// is left out, the host's instructions and its context. The default value
/// restores the protocol's built-in prompt.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
pub struct SetRlmPrompt {
    #[schemars(with = "serde_json::Value")]
    pub prompt: RlmPrompt,
}

impl ConfigCommand for SetRlmPrompt {
    type Owner = RlmConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_prompt";
}

/// Replace the context of the session's prompt config, the part a host
/// changes between turns, and leave the rest of the prompt as recorded.
/// An empty list clears it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
pub struct SetRlmPromptContext {
    pub context: Vec<String>,
}

impl ConfigCommand for SetRlmPromptContext {
    type Owner = RlmConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_prompt_context";
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
    })?;
    registrar.command::<SetRlmPrompt>(|recorded, command| {
        Ok(OwnerChange {
            recorded: RlmRecordedConfig {
                prompt: command.prompt,
                ..recorded.clone()
            },
            output: (),
        })
    })?;
    registrar.command::<SetRlmPromptContext>(|recorded, command| {
        Ok(OwnerChange {
            recorded: RlmRecordedConfig {
                prompt: RlmPrompt {
                    context: command.context,
                    ..recorded.prompt.clone()
                },
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
            lash_core::MaxToolCalls::new(1024),
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
        let recorded = created(
            Some(RlmCreateExtras {
                render: Some(print(Some(5), None)),
                termination: Some(RlmTermination::FinishRequired { schema: None }),
                ..RlmCreateExtras::default()
            }),
            true,
        );
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

        let restated = owner()
            .apply_run_options(
                &recorded,
                RlmRunOptions(RlmTurnOptions {
                    termination: Some(RlmTermination::Natural),
                    final_answer_format: Some(RlmFinalAnswerFormat::RawFinalValue),
                    render: None,
                }),
            )
            .expect("the run's termination applies");
        assert_eq!(restated.termination, Some(RlmTermination::Natural));
        assert_eq!(
            restated.final_answer_format,
            Some(RlmFinalAnswerFormat::RawFinalValue)
        );
        assert_eq!(restated.render, recorded.render);
        assert_eq!(
            owner()
                .apply_run_options(&recorded, RlmRunOptions::default())
                .expect("empty options apply"),
            recorded,
            "a run that states nothing runs under the recorded namespace"
        );
    }

    /// FIG-4652: a run's options have no field for a pin or for the prompt,
    /// so a payload naming one does not decode, even when it restates the
    /// recorded value. Every recorded field a run cannot state is covered:
    /// the list is the recorded namespace's own keys.
    #[test]
    fn run_options_have_no_field_for_a_pin_or_the_prompt() {
        let recorded = serde_json::to_value(created(
            Some(RlmCreateExtras {
                render: Some(RlmRenderPatch::default()),
                termination: Some(RlmTermination::Natural),
                ..RlmCreateExtras::default()
            }),
            true,
        ))
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
            std::collections::BTreeSet::from(["final_answer_format", "render", "termination"]),
            "a run restates only its termination, its answer format and its render"
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
                prompt: None,
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
            lash_core::MaxToolCalls::new(1024),
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
                .resolve(config, &record, &lash_core::EmptyLlmProfiles)
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

    /// A child session records its parent's behaviour, whatever the host
    /// that creates it is configured with, and needs nothing declared by
    /// that host to do so (FIG-4527).
    #[test]
    fn a_child_session_records_its_parents_behaviour() {
        let parent = created(None, true);
        let mut otherwise = config();
        otherwise.instruction_limit = crate::InstructionBound::instructions(7);
        otherwise.prompt_features.decomposition = false;
        otherwise.render.print.max_chars = Some(11);
        for process_lifecycle in [Some(true), None] {
            let creating_host = RlmConfigOwner {
                config: otherwise.clone(),
                ..owner_with(process_lifecycle)
            };
            let child = creating_host
                .create(
                    None,
                    CreationFacts {
                        parent: Some(&parent),
                        is_root_session: false,
                    },
                )
                .expect("create the child")
                .expect("the child records its namespace");
            assert_eq!(child.behaviour, parent.behaviour);
            assert_ne!(child.behaviour, otherwise.recorded_behaviour(true));
        }
    }

    fn host_prompt() -> RlmPrompt {
        RlmPrompt {
            intro: lash_rlm_types::RlmPromptIntro::Host {
                text: "You are the release assistant.".to_string(),
            },
            omit_builtin_guidance: true,
            omit_builtin_execution: false,
            instructions: vec!["Answer in British English.".to_string()],
            context: vec!["Release 4.2 freezes on Friday.".to_string()],
        }
    }

    /// FIG-4588: a session whose creator states no prompt records the
    /// built-in default, spelled on the wire as the built-in intro and
    /// nothing else.
    #[test]
    fn creation_defaults_the_prompt_to_the_built_in_one() {
        let recorded = created(None, true);
        assert_eq!(recorded.prompt, RlmPrompt::default());
        assert_eq!(
            serde_json::to_value(&recorded).expect("recorded")["prompt"],
            serde_json::json!({ "intro": { "kind": "builtin" } })
        );
    }

    /// FIG-4588: the prompt a creator states is the session's, whole.
    #[test]
    fn creation_records_the_stated_prompt() {
        let recorded = created(
            Some(RlmCreateExtras {
                prompt: Some(host_prompt()),
                ..RlmCreateExtras::default()
            }),
            true,
        );
        assert_eq!(recorded.prompt, host_prompt());
    }

    /// FIG-4588: a child copies its parent's recorded prompt when its
    /// creator states none, and records the stated one otherwise.
    #[test]
    fn a_child_session_copies_its_parents_prompt() {
        let parent = created(
            Some(RlmCreateExtras {
                prompt: Some(host_prompt()),
                ..RlmCreateExtras::default()
            }),
            true,
        );
        let child_of = |input: Option<RlmCreateExtras>| {
            owner()
                .create(
                    input.map(RlmCreateConfig),
                    CreationFacts {
                        parent: Some(&parent),
                        is_root_session: false,
                    },
                )
                .expect("create the child")
                .expect("the child records its namespace")
        };
        assert_eq!(child_of(None).prompt, host_prompt());
        // A creator that states other facts and no prompt still inherits it.
        assert_eq!(
            child_of(Some(RlmCreateExtras {
                final_answer_format: Some(RlmFinalAnswerFormat::RawFinalValue),
                ..RlmCreateExtras::default()
            }))
            .prompt,
            host_prompt()
        );
        let stated = RlmPrompt {
            instructions: vec!["Only read; never write.".to_string()],
            ..RlmPrompt::default()
        };
        assert_eq!(
            child_of(Some(RlmCreateExtras {
                prompt: Some(stated.clone()),
                ..RlmCreateExtras::default()
            }))
            .prompt,
            stated
        );
    }

    /// FIG-4588: the prompt commands resolve through the registry the
    /// factory registers. Each applies as one config transaction: the config
    /// a root was admitted under keeps its revision and its prompt, and the
    /// published config, which the next root is admitted under, carries the
    /// next revision and the new prompt. `SetRlmPrompt` replaces the prompt
    /// whole; `SetRlmPromptContext` replaces its context and nothing else.
    /// Neither touches another recorded fact.
    #[test]
    fn the_prompt_commands_change_the_next_roots_prompt() {
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
            lash_core::MaxToolCalls::new(1024),
        ));
        config.plugin_config = registry
            .resolve_creation(
                Some(crate::RLM_PROTOCOL_PLUGIN_ID),
                &lash_core::PluginOptions::default(),
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
        fn apply<C: ConfigCommand>(
            registry: &lash_core::ConfigRegistry,
            config: &mut lash_core::PersistedSessionConfig,
            id: &str,
            command: C,
        ) {
            let transaction = lash_core::ConfigTransaction::of(command);
            let record = registry
                .admit(
                    id,
                    config.config_revision,
                    registry.entries(&transaction).expect("entries"),
                )
                .expect("admitted");
            let outcome = registry
                .resolve(config, &record, &lash_core::EmptyLlmProfiles)
                .expect("the recorded config reads")
                .publish(config);
            assert!(
                matches!(outcome, lash_core::ConfigTransactionOutcome::Applied { .. }),
                "{outcome:?}"
            );
        }
        let base = recorded(&config);
        assert_eq!(base.prompt, RlmPrompt::default());

        // The config the running root was admitted under.
        let admitted = config.clone();
        apply(
            &registry,
            &mut config,
            "set-prompt",
            SetRlmPrompt {
                prompt: host_prompt(),
            },
        );
        assert_eq!(admitted.config_revision, 0);
        assert_eq!(recorded(&admitted), base);
        assert_eq!(config.config_revision, 1);
        assert_eq!(
            recorded(&config),
            RlmRecordedConfig {
                prompt: host_prompt(),
                ..base.clone()
            }
        );

        let admitted = config.clone();
        let context = vec!["Release 4.2 shipped.".to_string()];
        apply(
            &registry,
            &mut config,
            "set-prompt-context",
            SetRlmPromptContext {
                context: context.clone(),
            },
        );
        assert_eq!(admitted.config_revision, 1);
        assert_eq!(recorded(&admitted).prompt, host_prompt());
        assert_eq!(config.config_revision, 2);
        assert_eq!(
            recorded(&config),
            RlmRecordedConfig {
                prompt: RlmPrompt {
                    context,
                    ..host_prompt()
                },
                ..base.clone()
            }
        );

        // The default value restores the built-in prompt.
        apply(
            &registry,
            &mut config,
            "reset-prompt",
            SetRlmPrompt::default(),
        );
        assert_eq!(recorded(&config), base);
        assert_eq!(config.config_revision, 3);
    }

    /// FIG-4588: the prompt is not a pin. A candidate that changes it and
    /// keeps the channel, dialect and behaviour is admitted.
    #[test]
    fn a_candidate_may_change_the_prompt() {
        let base = created(None, true);
        let candidate = RlmRecordedConfig {
            prompt: host_prompt(),
            ..base.clone()
        };
        facts_for(|facts| {
            owner()
                .validate(&candidate, Some(&base), facts)
                .expect("the prompt is the session's to change");
        });
    }

    /// FIG-4588: a recorded namespace states its prompt. One written without
    /// it is not read as the default.
    #[test]
    fn a_recorded_namespace_without_a_prompt_does_not_decode() {
        let mut recorded = serde_json::to_value(created(None, true)).expect("recorded");
        recorded
            .as_object_mut()
            .expect("namespace object")
            .remove("prompt");
        let error = serde_json::from_value::<RlmRecordedConfig>(recorded)
            .expect_err("the prompt is a required recorded fact");
        assert!(error.to_string().contains("prompt"), "{error}");
    }

    /// The render a deployment configures is recorded behaviour: a session
    /// opened on a deployment configured otherwise resolves its render over
    /// the recorded one (FIG-4527).
    #[test]
    fn the_configured_render_is_recorded_behaviour() {
        let mut creating = config();
        creating.render.print.max_chars = Some(11);
        let recorded = creating.recorded_behaviour(false);
        assert_eq!(recorded.render, creating.render);
        let opened = config().under_recorded_behaviour(&recorded);
        assert_eq!(opened.render, creating.render);
        assert_ne!(opened.render, config().render);
    }
}
