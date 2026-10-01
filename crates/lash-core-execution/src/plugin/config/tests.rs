use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// A counter owner: it records a count and a label set at creation. The
/// label has no command, so it is immutable; `Increment` adds to the count
/// and refuses to pass `limit`, and the final candidate may not exceed 100.
struct CounterOwner {
    implementation: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CounterConfig {
    count: u32,
    label: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CounterCreate {
    #[serde(default)]
    count: Option<u32>,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CounterRefusal {
    PastLimit { limit: u32 },
    OverHundred { count: u32 },
}

impl std::fmt::Display for CounterRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PastLimit { limit } => write!(formatter, "past the limit {limit}"),
            Self::OverHundred { count } => write!(formatter, "{count} is over 100"),
        }
    }
}

impl ConfigOwner for CounterOwner {
    type Create = CounterCreate;
    type Recorded = CounterConfig;
    type Refusal = CounterRefusal;

    fn implementation(&self) -> &str {
        self.implementation
    }

    fn create(
        &self,
        input: Option<CounterCreate>,
        facts: CreationFacts<'_, CounterConfig>,
    ) -> Result<Option<CounterConfig>, CounterRefusal> {
        let inherited = facts.parent.cloned();
        let input = input.unwrap_or(CounterCreate {
            count: None,
            label: None,
        });
        Ok(Some(CounterConfig {
            count: input
                .count
                .or(inherited.as_ref().map(|parent| parent.count))
                .unwrap_or(0),
            label: input
                .label
                .or(inherited.map(|parent| parent.label))
                .unwrap_or_else(|| {
                    if facts.is_root_session {
                        "root".to_string()
                    } else {
                        "child".to_string()
                    }
                }),
        }))
    }

    fn validate(
        &self,
        value: &CounterConfig,
        _base: Option<&CounterConfig>,
        _facts: &CandidateFacts<'_>,
    ) -> Result<(), CounterRefusal> {
        if value.count > 100 {
            return Err(CounterRefusal::OverHundred { count: value.count });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Increment {
    by: u32,
    limit: u32,
}

impl ConfigCommand for Increment {
    type Owner = CounterOwner;
    type Output = u32;
    const NAME: &'static str = "increment";
}

struct CounterFactory {
    id: &'static str,
    implementation: &'static str,
    reductions: Arc<AtomicUsize>,
}

impl CounterFactory {
    fn new(id: &'static str) -> Self {
        Self {
            id,
            implementation: "counter:1",
            reductions: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl PluginFactory for CounterFactory {
    fn id(&self) -> &'static str {
        self.id
    }

    fn build(
        &self,
        _ctx: &crate::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn crate::plugin::SessionPlugin>, crate::PluginError> {
        Err(crate::PluginError::Session(
            "not built in these laws".to_string(),
        ))
    }

    fn register_config(&self, reg: &mut ConfigRegistrar) -> Result<(), ConfigRegistrationError> {
        reg.owner(CounterOwner {
            implementation: self.implementation,
        })?;
        let reductions = Arc::clone(&self.reductions);
        reg.command::<Increment>(move |recorded, command| {
            reductions.fetch_add(1, Ordering::SeqCst);
            let count = recorded.count + command.by;
            if count > command.limit {
                return Err(CounterRefusal::PastLimit {
                    limit: command.limit,
                });
            }
            Ok(OwnerChange {
                recorded: CounterConfig {
                    count,
                    ..recorded.clone()
                },
                output: count,
            })
        })
    }
}

fn registry(factories: Vec<Arc<dyn PluginFactory>>) -> ConfigRegistry {
    ConfigRegistry::build(&factories).expect("valid registrations")
}

fn counters() -> (ConfigRegistry, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let first = CounterFactory::new("first");
    let second = CounterFactory::new("second");
    let reductions = (
        Arc::clone(&first.reductions),
        Arc::clone(&second.reductions),
    );
    (
        registry(vec![Arc::new(first), Arc::new(second)]),
        reductions.0,
        reductions.1,
    )
}

fn head(registry: &ConfigRegistry, revision: u64) -> crate::PersistedSessionConfig {
    let mut config = crate::PersistedSessionConfig::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    );
    config.plugin_config = registry
        .resolve_creation(None, &PluginOptions::default(), None, true)
        .expect("creation config");
    config.config_revision = revision;
    config
}

fn increment(owner: &str, by: u32, limit: u32) -> ConfigCommandEntry {
    ConfigCommandEntry {
        owner: owner.to_string(),
        command: "increment".to_string(),
        args: serde_json::json!({ "by": by, "limit": limit }),
    }
}

#[test]
fn creation_records_defaults_stated_values_and_what_a_child_inherits() {
    let (registry, _, _) = counters();
    let root = registry
        .resolve_creation(
            Some("first"),
            &PluginOptions::typed("first", serde_json::json!({ "count": 3 })).expect("options"),
            None,
            true,
        )
        .expect("root config");
    assert_eq!(root.protocol_plugin_id(), Some("first"));
    assert_eq!(
        root.get("first"),
        Some(&serde_json::json!({ "count": 3, "label": "root" }))
    );
    assert_eq!(
        root.get("second"),
        Some(&serde_json::json!({ "count": 0, "label": "root" })),
        "an owner asked with nothing stated records its defaults"
    );
    let child = registry
        .resolve_creation(None, &PluginOptions::default(), Some(&root), false)
        .expect("child config");
    assert_eq!(
        child.get("first"),
        Some(&serde_json::json!({ "count": 3, "label": "root" })),
        "the owner decides what a child inherits"
    );

    let refused = registry
        .resolve_creation(
            None,
            &PluginOptions::typed("nobody", serde_json::json!({})).expect("options"),
            None,
            true,
        )
        .expect_err("an unowned namespace is refused");
    assert_eq!(
        refused.downcast_ref::<UnknownPluginConfigOwner>(),
        Some(&UnknownPluginConfigOwner {
            plugin_ids: vec!["nobody".to_string()],
        })
    );
    let refused = registry
        .resolve_creation(
            None,
            &PluginOptions::typed("first", serde_json::json!({ "colour": "red" }))
                .expect("options"),
            None,
            true,
        )
        .expect_err("creation input the owner does not accept is refused");
    assert_eq!(
        refused
            .downcast_ref::<ConfigRefusal>()
            .map(|refusal| refusal.owner.as_str()),
        Some("first")
    );
}

#[test]
fn ingress_refuses_unknown_owners_commands_and_arguments_typed() {
    let (registry, _, _) = counters();
    assert_eq!(
        registry.admit("t", 0, Vec::new()),
        Err(ConfigSubmitError::Empty)
    );
    assert_eq!(
        registry.admit("t", 0, vec![increment("nobody", 1, 10)]),
        Err(ConfigSubmitError::UnknownOwner {
            owner: "nobody".to_string()
        })
    );
    let mut unknown = increment("first", 1, 10);
    unknown.command = "decrement".to_string();
    assert_eq!(
        registry.admit("t", 0, vec![unknown]),
        Err(ConfigSubmitError::UnknownCommand {
            owner: "first".to_string(),
            command: "decrement".to_string(),
        })
    );
    let mut invalid = increment("first", 1, 10);
    invalid.args = serde_json::json!({ "by": 1, "limit": 10, "label": "renamed" });
    assert!(matches!(
        registry.admit("t", 0, vec![invalid]),
        Err(ConfigSubmitError::InvalidArgs { .. })
    ));
    let admitted = registry
        .admit("t", 0, vec![increment("first", 1, 10)])
        .expect("admitted");
    assert_eq!(
        admitted.implementations,
        BTreeMap::from([("first".to_string(), "counter:1".to_string())])
    );
}

#[test]
fn a_stale_transaction_runs_no_reducer_and_publishes_nothing() {
    let (registry, first, _) = counters();
    let base = head(&registry, 4);
    let transaction = registry
        .admit("t", 3, vec![increment("first", 1, 10)])
        .expect("admitted");
    let resolution = registry.resolve(&base, &transaction, &crate::EmptyModels);
    assert_eq!(
        resolution.result,
        ConfigResolutionDecision::Stale {
            expected: 3,
            actual: 4
        }
    );
    assert_eq!(first.load(Ordering::SeqCst), 0);
    let mut published = base.clone();
    assert_eq!(
        resolution.publish(&mut published),
        ConfigTransactionOutcome::Stale {
            expected: 3,
            actual: 4
        }
    );
    assert_eq!(published, base);
}

#[test]
fn ordered_commands_of_two_owners_publish_together_with_one_revision_step() {
    let (registry, _, _) = counters();
    let base = head(&registry, 7);
    let transaction = registry
        .admit(
            "t",
            7,
            vec![
                increment("first", 2, 10),
                increment("second", 5, 10),
                increment("first", 3, 10),
            ],
        )
        .expect("admitted");
    let resolution = registry.resolve(&base, &transaction, &crate::EmptyModels);
    let mut published = base.clone();
    assert_eq!(
        resolution.publish(&mut published),
        ConfigTransactionOutcome::Applied {
            base_revision: 7,
            revision: 8,
            outputs: vec![
                serde_json::json!(2),
                serde_json::json!(5),
                serde_json::json!(5)
            ],
        },
        "same-owner commands compose in order"
    );
    assert_eq!(
        published.plugin_config.get("first"),
        Some(&serde_json::json!({ "count": 5, "label": "root" }))
    );
    assert_eq!(
        published.plugin_config.get("second"),
        Some(&serde_json::json!({ "count": 5, "label": "root" }))
    );
}

#[test]
fn a_refused_member_refuses_the_whole_transaction() {
    let (registry, _, _) = counters();
    let base = head(&registry, 0);
    let transaction = registry
        .admit(
            "t",
            0,
            vec![increment("first", 2, 10), increment("second", 50, 10)],
        )
        .expect("admitted");
    let resolution = registry.resolve(&base, &transaction, &crate::EmptyModels);
    let mut published = base.clone();
    let ConfigTransactionOutcome::Refused { refusal } = resolution.publish(&mut published) else {
        panic!("the transaction is refused");
    };
    assert_eq!(refusal.index, Some(1));
    assert_eq!(refusal.owner, "second");
    assert_eq!(refusal.command.as_deref(), Some("increment"));
    assert_eq!(
        serde_json::from_value::<CounterRefusal>(refusal.refusal).expect("typed refusal"),
        CounterRefusal::PastLimit { limit: 10 }
    );
    assert_eq!(
        published, base,
        "nothing of a refused transaction publishes"
    );
}

#[test]
fn the_final_candidate_is_validated_by_every_touched_owner() {
    let (registry, _, _) = counters();
    let base = head(&registry, 0);
    let transaction = registry
        .admit(
            "t",
            0,
            vec![increment("first", 60, 1000), increment("first", 60, 1000)],
        )
        .expect("admitted");
    let resolution = registry.resolve(&base, &transaction, &crate::EmptyModels);
    let ConfigResolutionDecision::Refused { refusal } = resolution.result else {
        panic!("the final candidate is refused");
    };
    assert_eq!(refusal.index, None);
    assert_eq!(
        serde_json::from_value::<CounterRefusal>(refusal.refusal).expect("typed refusal"),
        CounterRefusal::OverHundred { count: 120 }
    );
}

#[test]
fn a_transaction_admitted_under_other_reducers_is_not_resolved_here() {
    let (registry, _, _) = counters();
    let transaction = registry
        .admit("t", 0, vec![increment("first", 1, 10)])
        .expect("admitted");
    let mut rebuilt = CounterFactory::new("first");
    rebuilt.implementation = "counter:2";
    let rebuilt = self::registry(vec![Arc::new(rebuilt)]);
    assert_eq!(
        rebuilt.check_implementations(&transaction),
        Err(ConfigImplementationMismatch {
            owner: "first".to_string(),
            recorded: "counter:1".to_string(),
            current: Some("counter:2".to_string()),
        })
    );
    registry
        .check_implementations(&transaction)
        .expect("the admitting registry runs its own reducers");
}

#[test]
fn typed_commands_address_the_owner_that_registered_their_type() {
    let (registry, _, _) = counters();
    let entries = registry
        .entries(&ConfigTransaction::of(core::SetTurnBudget {
            turn_budget: crate::TurnBudget::bounded(4),
        }))
        .expect("the core registers SetTurnBudget");
    assert_eq!(entries[0].owner, CORE_CONFIG_OWNER);
    assert_eq!(entries[0].command, "set_turn_budget");
}

/// A catalog serving each `(key, context window, efforts)` entry under its
/// key, with the key as its wire model.
fn catalog(entries: &[(&str, usize, &[&str])]) -> crate::ModelRegistry {
    let provider = crate::testing::TestProvider::builder()
        .kind("config-tests")
        .build()
        .into_handle();
    entries
        .iter()
        .try_fold(
            crate::ModelRegistry::new(),
            |registry, (key, context_window_tokens, efforts)| {
                registry.register(
                    *key,
                    crate::RegisteredModel::new(
                        metadata(key, *context_window_tokens, efforts),
                        provider.clone(),
                    ),
                )
            },
        )
        .expect("every key registers once")
}

fn metadata(key: &str, context_window_tokens: usize, efforts: &[&str]) -> crate::ModelMetadata {
    let metadata = crate::ModelMetadata::builder(key)
        .context_window_tokens(context_window_tokens)
        .build()
        .expect("model metadata");
    if efforts.is_empty() {
        return metadata;
    }
    metadata.with_capability(crate::ModelCapability {
        reasoning: Some(crate::ReasoningCapability {
            efforts: efforts.iter().map(|effort| (*effort).to_string()).collect(),
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn recorded(key: &str, context_window_tokens: usize, efforts: &[&str]) -> crate::ModelConfig {
    crate::ModelConfig::new(crate::RecordedModel::mint(
        crate::ModelKey::new(key),
        metadata(key, context_window_tokens, efforts),
    ))
}

fn resolve_core(
    registry: &ConfigRegistry,
    base: &crate::PersistedSessionConfig,
    models: &dyn crate::RuntimeModels,
    transaction: ConfigTransaction,
) -> ConfigResolution {
    let entries = registry.entries(&transaction).expect("entries");
    let transaction = registry.admit("t", 0, entries).expect("admitted");
    registry.resolve(base, &transaction, models)
}

fn core_refusal(resolution: &ConfigResolution) -> (Option<usize>, core::CoreConfigRefusal) {
    let ConfigResolutionDecision::Refused { refusal } = &resolution.result else {
        panic!("the core owner refuses: {resolution:?}");
    };
    assert_eq!(refusal.owner, CORE_CONFIG_OWNER);
    (
        refusal.index,
        serde_json::from_value(refusal.refusal.clone()).expect("the core owner's typed refusal"),
    )
}

#[test]
fn core_commands_keep_what_they_do_not_name() {
    let (registry, _, _) = counters();
    let mut base = head(&registry, 0);
    base.model = Some(
        recorded("base-model", 1000, &["low"])
            .with_reasoning(crate::ReasoningSelection::Effort("low".to_string())),
    );
    base.attachment_acceptance = Arc::new(crate::provider::AttachmentCapabilitySnapshot {
        revision: "recorded-acceptance".to_string(),
        acceptors: Vec::new(),
    });
    let models = catalog(&[
        ("base-model", 1000, &["low"]),
        ("next-model", 2000, &["low"]),
    ]);
    let resolution = resolve_core(
        &registry,
        &base,
        &models,
        ConfigTransaction::new()
            .then(core::SetModel {
                model: crate::ModelKey::new("next-model"),
            })
            .then(core::SetAutonomy { autonomous: true }),
    );
    let mut published = base.clone();
    assert!(matches!(
        resolution.publish(&mut published),
        ConfigTransactionOutcome::Applied { revision: 1, .. }
    ));
    assert_eq!(
        published.model,
        Some(
            recorded("next-model", 2000, &["low"])
                .with_reasoning(crate::ReasoningSelection::Effort("low".to_string()))
        ),
        "a model change records the minted binding and keeps the reasoning"
    );
    assert_eq!(
        published.attachment_acceptance, base.attachment_acceptance,
        "a model change keeps the attachment-acceptance snapshot"
    );
    assert!(published.autonomous, "the second command applied too");
    assert_eq!(published.plugin_config, base.plugin_config);
}

/// Selecting the recorded key again mints its binding from the catalog the
/// transaction resolves against: the one way a catalog edit reaches a
/// session.
#[test]
fn a_model_command_naming_the_recorded_key_mints_it_again() {
    let (registry, _, _) = counters();
    let mut base = head(&registry, 0);
    base.model = Some(recorded("model", 1000, &[]));
    let models = catalog(&[("model", 4000, &[])]);
    let resolution = resolve_core(
        &registry,
        &base,
        &models,
        ConfigTransaction::of(core::SetModel {
            model: crate::ModelKey::new("model"),
        }),
    );
    let mut published = base.clone();
    assert!(matches!(
        resolution.publish(&mut published),
        ConfigTransactionOutcome::Applied { .. }
    ));
    assert_eq!(published.model, Some(recorded("model", 4000, &[])));
}

#[test]
fn a_model_command_naming_an_unregistered_key_is_refused_typed() {
    let (registry, _, _) = counters();
    let mut base = head(&registry, 0);
    base.model = Some(recorded("model", 1000, &[]));
    let resolution = resolve_core(
        &registry,
        &base,
        &catalog(&[("model", 1000, &[])]),
        ConfigTransaction::of(core::SetModel {
            model: crate::ModelKey::new("missing"),
        }),
    );
    assert_eq!(
        core_refusal(&resolution),
        (
            Some(0),
            core::CoreConfigRefusal::UnknownModel {
                key: crate::ModelKey::new("missing"),
            }
        )
    );
}

/// A reasoning is judged against the model the final candidate records: an
/// effort the recorded model does not declare is refused, the same effort
/// applies beside a model change to one that declares it, whatever the
/// order, and a session with no model takes no reasoning.
#[test]
fn a_reasoning_command_is_judged_against_the_final_recorded_model() {
    let (registry, _, _) = counters();
    let deep = crate::ReasoningSelection::Effort("deep".to_string());
    let models = catalog(&[("plain", 1000, &[]), ("deep-model", 1000, &["deep"])]);
    let mut base = head(&registry, 0);
    base.model = Some(recorded("plain", 1000, &[]));

    let alone = resolve_core(
        &registry,
        &base,
        &models,
        ConfigTransaction::of(core::SetReasoning {
            reasoning: deep.clone(),
        }),
    );
    let (index, refusal) = core_refusal(&alone);
    assert_eq!(index, None, "the final candidate is refused, not a command");
    assert!(
        matches!(
            &refusal,
            core::CoreConfigRefusal::ReasoningRefused { key, reasoning, .. }
                if key.as_str() == "plain" && *reasoning == deep
        ),
        "{refusal:?}"
    );

    for transaction in [
        ConfigTransaction::of(core::SetReasoning {
            reasoning: deep.clone(),
        })
        .then(core::SetModel {
            model: crate::ModelKey::new("deep-model"),
        }),
        ConfigTransaction::of(core::SetModel {
            model: crate::ModelKey::new("deep-model"),
        })
        .then(core::SetReasoning {
            reasoning: deep.clone(),
        }),
    ] {
        let resolution = resolve_core(&registry, &base, &models, transaction);
        let mut published = base.clone();
        assert!(matches!(
            resolution.publish(&mut published),
            ConfigTransactionOutcome::Applied { .. }
        ));
        assert_eq!(
            published.model,
            Some(recorded("deep-model", 1000, &["deep"]).with_reasoning(deep.clone()))
        );
    }

    let mut unselected = head(&registry, 0);
    unselected.model = None;
    let without_model = resolve_core(
        &registry,
        &unselected,
        &models,
        ConfigTransaction::of(core::SetReasoning {
            reasoning: deep.clone(),
        }),
    );
    assert_eq!(
        core_refusal(&without_model),
        (
            Some(0),
            core::CoreConfigRefusal::ReasoningWithoutModel { reasoning: deep }
        )
    );
}

#[test]
fn the_catalog_lists_every_registered_command_with_its_schemas() {
    let (registry, _, _) = counters();
    let catalog = registry.catalog(9);
    assert_eq!(catalog.revision, 9);
    let increment = catalog
        .commands
        .iter()
        .find(|descriptor| descriptor.owner == "first" && descriptor.command == "increment")
        .expect("the counter's command is listed");
    assert!(increment.input_schema.to_string().contains("limit"));
    assert!(increment.refusal_schema.to_string().contains("past_limit"));
    assert!(catalog.commands.iter().any(
        |descriptor| descriptor.owner == CORE_CONFIG_OWNER && descriptor.command == "set_model"
    ));
}

#[test]
fn registrations_that_cannot_stand_are_refused() {
    struct Reserved;
    impl PluginFactory for Reserved {
        fn id(&self) -> &'static str {
            CORE_CONFIG_OWNER
        }
        fn build(
            &self,
            _ctx: &crate::plugin::PluginSessionContext,
        ) -> Result<Arc<dyn crate::plugin::SessionPlugin>, crate::PluginError> {
            Err(crate::PluginError::Session("unused".to_string()))
        }
    }
    assert_eq!(
        ConfigRegistry::build(&[Arc::new(Reserved) as Arc<dyn PluginFactory>]).map(drop),
        Err(ConfigRegistrationError::ReservedOwner {
            owner: CORE_CONFIG_OWNER.to_string()
        })
    );
    let mut reg = ConfigRegistrar::new("orphan");
    assert!(matches!(
        reg.command::<Increment>(|recorded, _| Ok(OwnerChange {
            recorded: recorded.clone(),
            output: 0,
        })),
        Err(ConfigRegistrationError::ForeignCommand { .. })
    ));
}
