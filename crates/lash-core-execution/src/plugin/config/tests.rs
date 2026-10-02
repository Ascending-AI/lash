use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// A counter owner: it records a count and a label set at creation. The
/// label has no command, so it is immutable; `Increment` adds to the count
/// and refuses to pass `limit`, and the final candidate may not exceed 100.
struct CounterOwner;

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
    type RunOptions = CounterRun;

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

    /// A run restates the count, within the limit it names.
    fn apply_run_options(
        &self,
        recorded: &CounterConfig,
        options: CounterRun,
    ) -> Result<CounterConfig, CounterRefusal> {
        if options.count > options.limit {
            return Err(CounterRefusal::PastLimit {
                limit: options.limit,
            });
        }
        Ok(CounterConfig {
            count: options.count,
            ..recorded.clone()
        })
    }
}

/// What a run states for a counter: its count for the run. The label is no
/// field of it.
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CounterRun {
    count: u32,
    limit: u32,
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
    reductions: Arc<AtomicUsize>,
}

impl CounterFactory {
    fn new(id: &'static str) -> Self {
        Self {
            id,
            reductions: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl PluginFactory for CounterFactory {
    fn id(&self) -> &'static str {
        self.id
    }

    fn declaration(&self) -> crate::plugin::PluginDeclaration {
        crate::plugin::PluginDeclaration::initial(self.id())
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
        reg.owner(CounterOwner)?;
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
        .resolve_creation(
            None,
            &PluginOptions::default(),
            None,
            true,
            &crate::store::plugin_writers::PluginAdmission::default(),
        )
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
            &crate::store::plugin_writers::PluginAdmission::default(),
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
        .resolve_creation(
            None,
            &PluginOptions::default(),
            Some(&root),
            false,
            &crate::store::plugin_writers::PluginAdmission::default(),
        )
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
            &crate::store::plugin_writers::PluginAdmission::default(),
        )
        .expect_err("an unowned namespace is refused");
    assert_eq!(
        refused,
        ConfigFault::Refused(ConfigRefusal {
            owner: "nobody".to_string(),
            at: RefusalSite::Creation,
            reason: ConfigRefusalReason::UnknownOwner,
        })
    );
    let refused = registry
        .resolve_creation(
            None,
            &PluginOptions::typed("first", serde_json::json!({ "colour": "red" }))
                .expect("options"),
            None,
            true,
            &crate::store::plugin_writers::PluginAdmission::default(),
        )
        .expect_err("creation input the owner does not accept is refused");
    assert!(
        matches!(
            &refused,
            ConfigFault::Refused(ConfigRefusal {
                owner,
                at: RefusalSite::Creation,
                reason: ConfigRefusalReason::Unreadable {
                    role: ConfigValueRole::CreationInput,
                    ..
                },
            }) if owner == "first"
        ),
        "the owner's creation input is unreadable: {refused:?}"
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
        admitted,
        ConfigTransactionRecord {
            id: "t".to_string(),
            expected_revision: 0,
            entries: vec![increment("first", 1, 10)],
        },
        "the record is the submitter's request and nothing of this build"
    );
}

#[test]
fn a_stale_transaction_runs_no_reducer_and_publishes_nothing() {
    let (registry, first, _) = counters();
    let base = head(&registry, 4);
    let transaction = registry
        .admit("t", 3, vec![increment("first", 1, 10)])
        .expect("admitted");
    let resolution = registry
        .resolve(
            &base,
            &transaction,
            &crate::EmptyLlmProfiles,
            &crate::store::plugin_writers::PluginAdmission::default(),
        )
        .expect("the recorded config reads");
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
    let resolution = registry
        .resolve(
            &base,
            &transaction,
            &crate::EmptyLlmProfiles,
            &crate::store::plugin_writers::PluginAdmission::default(),
        )
        .expect("the recorded config reads");
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
    let resolution = registry
        .resolve(
            &base,
            &transaction,
            &crate::EmptyLlmProfiles,
            &crate::store::plugin_writers::PluginAdmission::default(),
        )
        .expect("the recorded config reads");
    let mut published = base.clone();
    let ConfigTransactionOutcome::Refused { refusal } = resolution.publish(&mut published) else {
        panic!("the transaction is refused");
    };
    assert_eq!(refusal.owner, "second");
    assert_eq!(
        refusal.at,
        RefusalSite::Command {
            index: 1,
            command: "increment".to_string(),
        }
    );
    assert_eq!(
        refusal.owner_refusal::<CounterRefusal>(),
        Some(CounterRefusal::PastLimit { limit: 10 })
    );
    assert_eq!(
        refusal.to_string(),
        "config command 1 (`second.increment`) refused: past the limit 10",
        "the display is derived from the site and the reason"
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
    let resolution = registry
        .resolve(
            &base,
            &transaction,
            &crate::EmptyLlmProfiles,
            &crate::store::plugin_writers::PluginAdmission::default(),
        )
        .expect("the recorded config reads");
    let ConfigResolutionDecision::Refused { refusal } = resolution.result else {
        panic!("the final candidate is refused");
    };
    assert_eq!(refusal.at, RefusalSite::Candidate);
    assert_eq!(
        refusal.owner_refusal::<CounterRefusal>(),
        Some(CounterRefusal::OverHundred { count: 120 })
    );
}

/// The reason `transaction` is refused for over `base`, with its site.
fn refused_over(
    registry: &ConfigRegistry,
    base: &crate::PersistedSessionConfig,
    entry: ConfigCommandEntry,
) -> ConfigRefusal {
    let transaction = ConfigTransactionRecord {
        id: "t".to_string(),
        expected_revision: base.config_revision,
        entries: vec![entry],
    };
    let resolution = registry
        .resolve(
            base,
            &transaction,
            &crate::EmptyLlmProfiles,
            &crate::store::plugin_writers::PluginAdmission::default(),
        )
        .expect("the recorded config reads");
    let ConfigResolutionDecision::Refused { refusal } = resolution.result else {
        panic!("the transaction is refused: {resolution:?}");
    };
    refusal
}

/// FIG-4652: what the framework refuses is its own variant, never data in
/// the owner's slot. A recorded transaction that names an owner or a command
/// this build does not register, a namespace the session never recorded, or
/// arguments the command cannot read each refuse with their own reason, at
/// the command that met them.
#[test]
fn framework_refusals_are_their_own_reasons_and_never_the_owners_data() {
    let (registry, _, _) = counters();
    let base = head(&registry, 0);
    let at = |command: &str| RefusalSite::Command {
        index: 0,
        command: command.to_string(),
    };

    let unknown_owner = refused_over(&registry, &base, increment("nobody", 1, 10));
    assert_eq!(
        unknown_owner,
        ConfigRefusal {
            owner: "nobody".to_string(),
            at: at("increment"),
            reason: ConfigRefusalReason::UnknownOwner,
        }
    );

    let unknown_command = refused_over(
        &registry,
        &base,
        ConfigCommandEntry {
            command: "decrement".to_string(),
            ..increment("first", 1, 10)
        },
    );
    assert_eq!(
        unknown_command,
        ConfigRefusal {
            owner: "first".to_string(),
            at: at("decrement"),
            reason: ConfigRefusalReason::UnknownCommand,
        }
    );

    let mut unrecorded_base = base.clone();
    unrecorded_base.plugin_config = PluginConfig::for_protocol(None);
    let unrecorded = refused_over(&registry, &unrecorded_base, increment("first", 1, 10));
    assert_eq!(
        unrecorded,
        ConfigRefusal {
            owner: "first".to_string(),
            at: at("increment"),
            reason: ConfigRefusalReason::UnrecordedNamespace,
        }
    );

    let unreadable = refused_over(
        &registry,
        &base,
        ConfigCommandEntry {
            args: serde_json::json!({ "by": "one" }),
            ..increment("first", 1, 10)
        },
    );
    assert_eq!(unreadable.at, at("increment"));
    assert!(
        matches!(
            unreadable.reason,
            ConfigRefusalReason::Unreadable {
                role: ConfigValueRole::Arguments,
                ..
            }
        ),
        "arguments the command cannot read: {unreadable:?}"
    );

    for refusal in [unknown_owner, unknown_command, unrecorded, unreadable] {
        assert_eq!(
            refusal.owner_refusal::<CounterRefusal>(),
            None,
            "a framework reason carries no owner refusal: {refusal:?}"
        );
        assert_eq!(
            serde_json::to_value(&refusal).expect("encodes")["reason"].get("refusal"),
            None,
            "nothing lash minted sits in the owner's slot: {refusal:?}"
        );
    }
}

/// FIG-4652: a recorded namespace its owner cannot read is corruption of
/// the session's stored config. It refuses nothing: a transaction resolves
/// to no decision to record, whether its command changes that namespace or
/// only the final validation meets it; a run override validates to no
/// verdict; and a child's creation records nothing from a corrupt parent.
#[test]
fn an_unreadable_recorded_namespace_is_corruption_and_refuses_nothing() {
    let (registry, reductions, _) = counters();
    let mut base = head(&registry, 0);
    base.plugin_config
        .insert("first", serde_json::json!({ "count": "three" }));
    let corrupt = |error: RecordedNamespaceCorrupt| {
        assert_eq!(error.owner, "first");
        assert!(
            matches!(
                error.clone().into_store_error(),
                crate::StoreError::StoredDataCorrupt {
                    record_kind: "session_config_namespace",
                    ..
                }
            ),
            "{error:?}"
        );
    };

    for (what, entry) in [
        (
            "a command on the corrupt namespace",
            increment("first", 1, 10),
        ),
        ("a command on another namespace", increment("second", 1, 10)),
    ] {
        let transaction = registry.admit("t", 0, vec![entry]).expect("admitted");
        corrupt(
            registry
                .resolve(
                    &base,
                    &transaction,
                    &crate::EmptyLlmProfiles,
                    &crate::store::plugin_writers::PluginAdmission::default(),
                )
                .expect_err(what),
        );
    }
    assert_eq!(
        reductions.load(Ordering::SeqCst),
        0,
        "no reducer ran over the corrupt namespace"
    );

    let mut derived = base.clone();
    derived
        .plugin_config
        .insert("first", serde_json::json!({ "count": 1, "label": "root" }));
    match registry.validate_derived(&base, &derived) {
        Err(ConfigFault::RecordedCorrupt(error)) => corrupt(error),
        other => panic!("a run override over a corrupt namespace: {other:?}"),
    }
    match registry.apply_run_options(
        &base.plugin_config,
        "first",
        &crate::ProtocolTurnOptions::from_payload(serde_json::json!({ "count": 1, "limit": 9 })),
    ) {
        Err(ConfigFault::RecordedCorrupt(error)) => corrupt(error),
        other => panic!("run options over a corrupt namespace: {other:?}"),
    }
    match registry.resolve_creation(
        None,
        &PluginOptions::default(),
        Some(&base.plugin_config),
        false,
        &crate::store::plugin_writers::PluginAdmission::default(),
    ) {
        Err(ConfigFault::RecordedCorrupt(error)) => corrupt(error),
        other => panic!("a child of a corrupt parent: {other:?}"),
    }

    // A candidate the owner's own reducer produced is the candidate's
    // fault, not stored data: here, one over the owner's ceiling.
    let healthy = head(&registry, 0);
    let mut over = healthy.clone();
    over.plugin_config
        .insert("first", serde_json::json!({ "count": "many" }));
    assert!(
        matches!(
            registry.validate_derived(&healthy, &over),
            Err(ConfigFault::Refused(ConfigRefusal {
                at: RefusalSite::Candidate,
                reason: ConfigRefusalReason::Unreadable {
                    role: ConfigValueRole::Candidate,
                    ..
                },
                ..
            }))
        ),
        "an unreadable candidate over a readable base is refused"
    );
}

/// FIG-4652: a run's options are the owner's typed run options, and only
/// the owner lays them over its namespace. A field that is not a run option
/// does not decode, whatever value it states, the recorded one included.
#[test]
fn run_options_are_the_owners_typed_options_and_only_the_owner_applies_them() {
    let (registry, _, _) = counters();
    let base = head(&registry, 0);
    let options = |payload: serde_json::Value| crate::ProtocolTurnOptions::from_payload(payload);
    let apply =
        |payload| registry.apply_run_options(&base.plugin_config, "first", &options(payload));

    assert_eq!(
        apply(serde_json::json!({ "count": 7, "limit": 9 })).expect("applied"),
        serde_json::json!({ "count": 7, "label": "root" }),
        "the owner applied its run options over the recorded namespace"
    );

    for restated in [
        serde_json::json!({ "count": 7, "limit": 9, "label": "root" }),
        serde_json::json!({ "count": 7, "limit": 9, "label": "other" }),
    ] {
        let refused = apply(restated).expect_err("the label is no run option");
        assert!(
            matches!(
                &refused,
                ConfigFault::Refused(ConfigRefusal {
                    owner,
                    at: RefusalSite::Candidate,
                    reason: ConfigRefusalReason::Unreadable {
                        role: ConfigValueRole::RunOptions,
                        ..
                    },
                }) if owner == "first"
            ),
            "{refused:?}"
        );
    }

    let ConfigFault::Refused(refusal) =
        apply(serde_json::json!({ "count": 12, "limit": 9 })).expect_err("past the limit")
    else {
        panic!("the owner refuses");
    };
    assert_eq!(refusal.at, RefusalSite::Candidate);
    assert_eq!(
        refusal.owner_refusal::<CounterRefusal>(),
        Some(CounterRefusal::PastLimit { limit: 9 })
    );

    assert_eq!(
        registry.apply_run_options(
            &base.plugin_config,
            "nobody",
            &options(serde_json::json!({})),
        ),
        Err(ConfigFault::Refused(ConfigRefusal {
            owner: "nobody".to_string(),
            at: RefusalSite::Candidate,
            reason: ConfigRefusalReason::UnknownOwner,
        }))
    );
    assert!(
        matches!(
            registry.apply_run_options(
                &base.plugin_config,
                CORE_CONFIG_OWNER,
                &options(serde_json::json!({})),
            ),
            Err(ConfigFault::Refused(ConfigRefusal {
                reason: ConfigRefusalReason::UnrecordedNamespace,
                ..
            }))
        ),
        "the core share is no namespace a run's options apply to"
    );
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
fn catalog(entries: &[(&str, usize, &[&str])]) -> crate::LlmProfileRegistry {
    let provider = crate::testing::TestProvider::builder()
        .kind("config-tests")
        .build()
        .into_handle();
    entries
        .iter()
        .try_fold(
            crate::LlmProfileRegistry::new(),
            |registry, (key, context_window_tokens, efforts)| {
                registry.register(
                    *key,
                    crate::RegisteredLlmProfile::new(
                        metadata(key, *context_window_tokens, efforts),
                        provider.clone(),
                    ),
                )
            },
        )
        .expect("every key registers once")
}

fn metadata(
    key: &str,
    context_window_tokens: usize,
    efforts: &[&str],
) -> crate::LlmProfileMetadata {
    let metadata = crate::LlmProfileMetadata::builder(key)
        .context_window_tokens(context_window_tokens)
        .build()
        .expect("model metadata");
    if efforts.is_empty() {
        return metadata;
    }
    metadata.with_capability(crate::LlmProfileCapability {
        reasoning: Some(crate::ReasoningCapability {
            efforts: efforts.iter().map(|effort| (*effort).to_string()).collect(),
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn recorded(key: &str, context_window_tokens: usize, efforts: &[&str]) -> crate::LlmProfileConfig {
    crate::LlmProfileConfig::new(crate::RecordedLlmProfile::mint(
        crate::LlmProfileKey::new(key),
        metadata(key, context_window_tokens, efforts),
    ))
}

fn resolve_core(
    registry: &ConfigRegistry,
    base: &crate::PersistedSessionConfig,
    models: &dyn crate::LlmProfiles,
    transaction: ConfigTransaction,
) -> ConfigResolution {
    let entries = registry.entries(&transaction).expect("entries");
    let transaction = registry.admit("t", 0, entries).expect("admitted");
    registry
        .resolve(
            base,
            &transaction,
            models,
            &crate::store::plugin_writers::PluginAdmission::default(),
        )
        .expect("the recorded config reads")
}

/// The core owner's refusal of `resolution`, and the index of the command
/// it refused (`None` for the final candidate).
fn core_refusal(resolution: &ConfigResolution) -> (Option<usize>, core::CoreConfigRefusal) {
    let ConfigResolutionDecision::Refused { refusal } = &resolution.result else {
        panic!("the core owner refuses: {resolution:?}");
    };
    assert_eq!(refusal.owner, CORE_CONFIG_OWNER);
    let index = match &refusal.at {
        RefusalSite::Command { index, .. } => Some(*index),
        RefusalSite::Candidate => None,
        RefusalSite::Creation => panic!("a transaction is not a creation: {refusal:?}"),
    };
    (
        index,
        refusal
            .owner_refusal()
            .expect("the core owner's typed refusal"),
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
            .then(core::SetLlmProfile {
                model: crate::LlmProfileKey::new("next-model"),
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
fn a_profile_command_naming_the_recorded_key_mints_it_again() {
    let (registry, _, _) = counters();
    let mut base = head(&registry, 0);
    base.model = Some(recorded("model", 1000, &[]));
    let models = catalog(&[("model", 4000, &[])]);
    let resolution = resolve_core(
        &registry,
        &base,
        &models,
        ConfigTransaction::of(core::SetLlmProfile {
            model: crate::LlmProfileKey::new("model"),
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
fn a_profile_command_naming_an_unregistered_key_is_refused_typed() {
    let (registry, _, _) = counters();
    let mut base = head(&registry, 0);
    base.model = Some(recorded("model", 1000, &[]));
    let resolution = resolve_core(
        &registry,
        &base,
        &catalog(&[("model", 1000, &[])]),
        ConfigTransaction::of(core::SetLlmProfile {
            model: crate::LlmProfileKey::new("missing"),
        }),
    );
    assert_eq!(
        core_refusal(&resolution),
        (
            Some(0),
            core::CoreConfigRefusal::UnknownLlmProfile {
                key: crate::LlmProfileKey::new("missing"),
            }
        )
    );
}

/// A reasoning is judged against the model the final candidate records: an
/// effort the recorded model does not declare is refused, the same effort
/// applies beside a model change to one that declares it, whatever the
/// order, and a session with no model takes no reasoning.
#[test]
fn a_reasoning_command_is_judged_against_the_final_recorded_llm_profile() {
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
        .then(core::SetLlmProfile {
            model: crate::LlmProfileKey::new("deep-model"),
        }),
        ConfigTransaction::of(core::SetLlmProfile {
            model: crate::LlmProfileKey::new("deep-model"),
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
    let without_llm_profile = resolve_core(
        &registry,
        &unselected,
        &models,
        ConfigTransaction::of(core::SetReasoning {
            reasoning: deep.clone(),
        }),
    );
    assert_eq!(
        core_refusal(&without_llm_profile),
        (
            Some(0),
            core::CoreConfigRefusal::ReasoningWithoutLlmProfile { reasoning: deep }
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
    assert!(
        catalog
            .commands
            .iter()
            .any(|descriptor| descriptor.owner == CORE_CONFIG_OWNER
                && descriptor.command == "set_llm_profile")
    );
}

#[test]
fn registrations_that_cannot_stand_are_refused() {
    struct Reserved;
    impl PluginFactory for Reserved {
        fn id(&self) -> &'static str {
            CORE_CONFIG_OWNER
        }

        fn declaration(&self) -> crate::plugin::PluginDeclaration {
            crate::plugin::PluginDeclaration::initial(self.id())
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

/// A counter plugin that reads format 2 natively and still writes format 1,
/// which spells the count `tally`.
struct FormattedCounter(CounterFactory);

impl FormattedCounter {
    const ID: &'static str = "formatted";

    fn rename(mut value: serde_json::Value, from: &str, to: &str) -> serde_json::Value {
        if let Some(object) = value.as_object_mut()
            && let Some(count) = object.remove(from)
        {
            object.insert(to.to_string(), count);
        }
        value
    }
}

impl PluginFactory for FormattedCounter {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn declaration(&self) -> crate::plugin::PluginDeclaration {
        let mut declaration = crate::plugin::PluginDeclaration::initial(Self::ID);
        declaration.format_version = crate::FormatVersion::new(2).expect("a format version");
        declaration.writable_formats = vec![crate::FormatVersion::ONE, declaration.format_version];
        declaration
    }

    fn migrate_format(
        &self,
        from: crate::FormatVersion,
        _namespace: crate::FormatNamespace,
        value: serde_json::Value,
    ) -> Result<serde_json::Value, crate::FormatRefusal> {
        Ok(if from == crate::FormatVersion::ONE {
            Self::rename(value, "tally", "count")
        } else {
            value
        })
    }

    fn encode_format(
        &self,
        to: crate::FormatVersion,
        _namespace: crate::FormatNamespace,
        value: &serde_json::Value,
    ) -> Result<serde_json::Value, crate::FormatRefusal> {
        Ok(if to == crate::FormatVersion::ONE {
            Self::rename(value.clone(), "count", "tally")
        } else {
            value.clone()
        })
    }

    fn build(
        &self,
        ctx: &crate::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn crate::plugin::SessionPlugin>, crate::PluginError> {
        self.0.build(ctx)
    }

    fn register_config(&self, reg: &mut ConfigRegistrar) -> Result<(), ConfigRegistrationError> {
        self.0.register_config(reg)
    }
}

/// The admission of [`FormattedCounter`] that chose `writer`.
fn formatted_admission(writer: u32) -> crate::store::plugin_writers::PluginAdmission {
    crate::store::plugin_writers::PluginAdmission::from_plugins(vec![
        crate::store::plugin_writers::AdmittedPlugin {
            plugin: FormattedCounter::ID.to_string(),
            behavior_revision: crate::plugin::BehaviorRevision::ONE,
            writer: crate::FormatVersion::new(writer).expect("a format version"),
        },
    ])
}

/// FIG-4747: creation and a transaction write a namespace in the format the
/// admission recorded for its plugin, and a later admission's wider choice
/// changes what it writes, never what the earlier one wrote.
#[test]
fn config_namespaces_are_written_in_the_admissions_recorded_format() {
    let registry = registry(vec![Arc::new(FormattedCounter(CounterFactory::new(
        FormattedCounter::ID,
    )))]);
    let stamp = |config: &crate::PluginConfig| {
        let namespace = config
            .namespace(FormattedCounter::ID)
            .expect("the namespace is recorded");
        (namespace.format_version.get(), namespace.value.clone())
    };
    let create = |writers: &crate::store::plugin_writers::PluginAdmission| {
        registry
            .resolve_creation(None, &PluginOptions::default(), None, true, writers)
            .expect("creation config")
    };

    // Inside the window the admission chose format 1.
    let window = formatted_admission(1);
    let created = create(&window);
    assert_eq!(
        stamp(&created),
        (1, serde_json::json!({ "tally": 0, "label": "root" }))
    );
    // An admission that names no writer for the plugin writes its native one.
    assert_eq!(
        stamp(&create(
            &crate::store::plugin_writers::PluginAdmission::default()
        )),
        (2, serde_json::json!({ "count": 0, "label": "root" }))
    );

    let mut base = crate::PersistedSessionConfig::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    );
    base.plugin_config = created;
    base.config_revision = 7;
    let transaction = registry
        .admit("t", 7, vec![increment(FormattedCounter::ID, 2, 10)])
        .expect("admitted");
    let publish = |writers: &crate::store::plugin_writers::PluginAdmission| {
        let resolution = registry
            .resolve(&base, &transaction, &crate::EmptyLlmProfiles, writers)
            .expect("the recorded config reads");
        let mut published = base.clone();
        assert!(matches!(
            resolution.publish(&mut published),
            ConfigTransactionOutcome::Applied { .. }
        ));
        (
            stamp(&published.plugin_config),
            serde_json::to_vec(&resolution).expect("encode the resolution"),
        )
    };
    let (recorded, first) = publish(&window);
    assert_eq!(
        recorded,
        (1, serde_json::json!({ "tally": 2, "label": "root" })),
        "the format 1 base is migrated, reduced and written back in format 1"
    );
    // The same recorded admission resolves to the same bytes again.
    assert_eq!(publish(&window).1, first);
    // An admission made after the range widened writes the native format.
    assert_eq!(
        publish(&formatted_admission(2)).0,
        (2, serde_json::json!({ "count": 2, "label": "root" }))
    );
}
