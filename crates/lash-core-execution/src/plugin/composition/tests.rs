use super::*;
use crate::plugin::prompt::{PromptInput, PromptSectionSpec, SectionText};
use crate::prompt_sections::{PromptPlacement, PromptSectionKey};
use std::collections::BTreeSet;

const KEYED_TOOL: &str = "tool:keyed";

/// REGISTRATION-IS-INVENTORY: a turn callback is part of installed code.
#[test]
fn registration_is_inventory() {
    use super::super::prompt::{
        OfferedTools, PromptFamilySection, PromptSectionFamilySpec, PromptSectionSource,
        PromptWrapSpec, PromptWrapTarget,
    };
    use crate::prompt_sections::PromptWrapKey;
    #[derive(Clone)]
    struct Owner(Arc<std::sync::atomic::AtomicUsize>);
    impl SessionPlugin for Owner {
        fn id(&self) -> &'static str {
            "test_protocol"
        }
    }
    #[async_trait::async_trait]
    impl CodeExecutorPlugin for Owner {
        async fn execute_code(
            &self,
            _: crate::RuntimeExecutionContext<'_>,
            _: crate::ExecRequest,
        ) -> Result<crate::ExecResponse, crate::SessionError> {
            Err(crate::SessionError::Protocol(
                "inventory fixture executes no code".into(),
            ))
        }
        async fn frame_switch_carries(
            &self,
            _: ProtocolSessionContext<'_>,
            _: &crate::FrameNodeId,
            _: &[crate::SessionAppendNode],
        ) -> Result<Vec<crate::ArtifactName>, crate::SessionError> {
            Ok(Vec::new())
        }
    }
    impl AssistantProseProjectorPlugin for Owner {
        fn project_assistant_prose(&self, text: &str) -> String {
            text.to_owned()
        }
    }
    #[async_trait::async_trait]
    impl ContextPressureHook for Owner {
        fn id(&self) -> &'static str {
            "pressure"
        }
        async fn decide(
            &self,
            _: &ContextPressureContext<'_>,
        ) -> Result<ContextPressureDecision, ContextError> {
            Ok(ContextPressureDecision::Continue)
        }
    }
    #[async_trait::async_trait]
    impl ContextCompactor for Owner {
        fn id(&self) -> &'static str {
            "compact"
        }
        async fn compact(
            &self,
            _: &CompactionContext<'_>,
        ) -> Result<Option<ContextCompaction>, ContextError> {
            Ok(None)
        }
    }
    impl AttachmentOmissionPolicy for Owner {
        fn id(&self) -> &'static str {
            "omit"
        }
        fn omissions(
            &self,
            _: &AttachmentOmissionContext,
            _: &[crate::Message],
        ) -> Result<BTreeSet<HistoryPartId>, PluginError> {
            Ok(BTreeSet::new())
        }
    }
    impl PromptSectionSource for Owner {
        fn sections(&self, _: &OfferedTools) -> Vec<PromptFamilySection> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Vec::new()
        }
    }
    struct Config;
    impl ConfigOwner for Config {
        type Create = serde_json::Value;
        type Recorded = serde_json::Value;
        type Refusal = String;
        type RunOptions = NoRunOptions;
        fn create(
            &self,
            input: Option<Self::Create>,
        ) -> Result<Option<Self::Recorded>, Self::Refusal> {
            Ok(input)
        }
        fn validate(
            &self,
            _: &Self::Recorded,
            _: Option<&Self::Recorded>,
            _: &CandidateFacts<'_>,
        ) -> Result<(), Self::Refusal> {
            Ok(())
        }
        fn apply_run_options(
            &self,
            _: &Self::Recorded,
            options: Self::RunOptions,
        ) -> Result<Self::Recorded, Self::Refusal> {
            match options {}
        }
    }
    #[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
    struct Command;
    impl ConfigCommand for Command {
        type Owner = Config;
        type Output = ();
        const NAME: &'static str = "set";
    }
    let constructions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let expansions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut reg = PluginRegistrar::<Owner>::new("test_protocol");
    crate::testing::register_inventory_protocol(&mut reg).unwrap();
    reg.turn()
        .before(crate::hook_key!("before"), |_, _| {
            Ok(Arc::new(|_| Box::pin(async { Ok(Default::default()) })))
        })
        .unwrap();
    reg.turn()
        .after(crate::hook_key!("after"), |_, _| {
            Ok(Arc::new(|_| Box::pin(async { Ok(Default::default()) })))
        })
        .unwrap();
    reg.turn()
        .checkpoint(crate::hook_key!("checkpoint"), |_, _| {
            Ok(Arc::new(|_| Box::pin(async { Ok(Default::default()) })))
        })
        .unwrap();
    reg.output()
        .stream(crate::hook_key!("stream"), |_, _| {
            Ok(Arc::new(|ctx| {
                Box::pin(async move {
                    Ok(AssistantStreamTransform {
                        chunk: ctx.chunk,
                        reasoning_deltas: Vec::new(),
                        events: Vec::new(),
                        abort_stream: false,
                    })
                })
            }))
        })
        .unwrap();
    reg.output()
        .stream_finished(crate::hook_key!("stream"), |_, _| {
            Ok(Arc::new(|_| Box::pin(async { Ok(None) })))
        })
        .unwrap();
    reg.output()
        .response(
            crate::hook_key!("response"),
            Some(crate::hook_key!("stream")),
            |_, _| {
                Ok(Arc::new(|ctx| {
                    Box::pin(async move {
                        Ok(AssistantResponseTransform {
                            response: ctx.response,
                            events: Vec::new(),
                        })
                    })
                }))
            },
        )
        .unwrap();
    reg.output()
        .assistant_prose_projector(|owner, _| Ok(Arc::new(owner.clone())))
        .unwrap();
    reg.execution()
        .code_executor(|owner, _| Ok(Arc::new(owner.clone())))
        .unwrap();
    reg.context()
        .pressure(crate::hook_key!("pressure"), 3, |owner, _| {
            Ok(Arc::new(owner.clone()))
        })
        .unwrap();
    reg.context()
        .compact(crate::hook_key!("compact"), 2, |owner, _| {
            Ok(Arc::new(owner.clone()))
        })
        .unwrap();
    reg.context()
        .attachment_omissions(crate::hook_key!("omit"), 1, |owner, _| {
            Ok(Arc::new(owner.clone()))
        })
        .unwrap();
    reg.session()
        .on_event(crate::hook_key!("event"), |_, _| {
            Ok(Arc::new(|_| Box::pin(async { Ok(()) })))
        })
        .unwrap();
    let provider_handle = reg
        .tools()
        .provider(crate::tool_provider_key!("tools"), |_, _| {
            Ok(Arc::new(crate::testing::EmptyToolProvider))
        })
        .unwrap();
    assert!(
        matches!(
            reg.tools()
                .provider(crate::tool_provider_key!("tools"), |_, _| Ok(Arc::new(
                    crate::testing::EmptyToolProvider
                ))),
            Err(PluginError::Registration(_))
        ),
        "declared provider addresses are unique"
    );
    struct NoReads;
    #[async_trait::async_trait]
    impl crate::ToolReads for NoReads {
        async fn read(
            &self,
            _: crate::ReadCall<'_>,
        ) -> Result<serde_json::Value, crate::ReadError> {
            Ok(serde_json::Value::Null)
        }
    }
    reg.tools()
        .provider_with_reads(crate::tool_provider_key!("reads"), |_, _| {
            Ok(
                crate::ToolProviderRegistration::new(Arc::new(crate::testing::EmptyToolProvider))
                    .with_reads(Arc::new(NoReads)),
            )
        })
        .unwrap();
    reg.tool_catalog()
        .contribute(crate::hook_key!("catalog"), |_, _| {
            Ok(Arc::new(|_| Ok(Default::default())))
        })
        .unwrap();
    reg.tool_calls()
        .transform_args(crate::hook_key!("args"), |_, _| {
            Ok(Arc::new(|input| Box::pin(async move { Ok(input.current) })))
        })
        .unwrap();
    reg.tool_calls()
        .check_args(crate::hook_key!("args"), |_, _| {
            Ok(Arc::new(|_| {
                Box::pin(async { Ok(BeforeToolDecision::Allow) })
            }))
        })
        .unwrap();
    reg.tool_calls()
        .transform_result(crate::hook_key!("result"), |_, _| {
            Ok(Arc::new(|input| Box::pin(async move { Ok(input.current) })))
        })
        .unwrap();
    reg.tool_calls()
        .check_result(crate::hook_key!("result"), |_, _| {
            Ok(Arc::new(|_| {
                Box::pin(async { Ok(AfterToolContributions::default()) })
            }))
        })
        .unwrap();
    reg.tool_results()
        .presenter(|_, _| {
            Ok(Arc::new(|input| {
                Box::pin(async move { Ok(input.previous) })
            }))
        })
        .unwrap();
    reg.tool_results()
        .presentation_step(crate::hook_key!("step"), |_, _| {
            Ok(Arc::new(|input| {
                Box::pin(async move { Ok(input.previous) })
            }))
        })
        .unwrap();
    reg.operations()
        .query(
            PluginOperationSpec {
                name: "query".into(),
                description: "fixture".into(),
                session_param: SessionParam::Optional,
                input_schema: serde_json::json!({}),
                output_schema: serde_json::json!({}),
                error_type: "fixture".into(),
                error_version: FormatVersion::ONE,
                error_schema: serde_json::json!({}),
            },
            |_, _| Ok(Arc::new(|_, value| Box::pin(async { Ok(value) }))),
        )
        .unwrap();
    let section = PromptSectionKey::new("section").unwrap();
    reg.prompt()
        .section(
            PromptSectionSpec::new(section.clone(), PromptPlacement::InitialInstructions),
            |_, _| {
                Ok(Arc::new(|_: &PromptInput<'_>| {
                    Ok::<_, super::super::prompt::PromptRenderError>(SectionText::text("section"))
                }))
            },
        )
        .unwrap();
    reg.prompt()
        .family(
            PromptSectionFamilySpec::new(
                PromptSectionKey::new("family").unwrap(),
                PromptPlacement::CurrentContext,
            ),
            |owner, _| Ok(Arc::new(owner.clone())),
        )
        .unwrap();
    reg.prompt()
        .wrap(
            PromptWrapSpec::new(
                PromptWrapKey::new("wrap").unwrap(),
                super::super::prompt::PromptSectionId::new("test_protocol", section),
            ),
            |_, _| {
                Ok(Arc::new(
                    |_: &PromptInput<'_>, _: PromptWrapTarget<'_>, text: SectionText| {
                        Ok::<_, super::super::prompt::PromptRenderError>(text)
                    },
                ))
            },
        )
        .unwrap();
    reg.state_reducer("reduce", |_, _| {
        Ok(Arc::new(|reduction| Ok(reduction.current.cloned())))
    })
    .unwrap();
    reg.extension("owner-extension", |_, _| Ok(serde_json::Value::Null))
        .unwrap();
    let count = Arc::clone(&constructions);
    let family = Arc::clone(&expansions);
    let mut recipe = PluginRecipe::new("test_protocol", reg, move |_, _| {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Arc::new(Owner(Arc::clone(&family))))
    });
    let mut config = ConfigRegistrar::new("test_protocol");
    config.owner(Config).unwrap();
    config
        .command::<Command>(|recorded, _| {
            Ok(OwnerChange {
                recorded: recorded.clone(),
                output: (),
            })
        })
        .unwrap();
    recipe = recipe
        .with_configuration(config)
        .with_factory_extension(PluginExtensionContribution::from_value(
            "factory-extension",
            serde_json::Value::Null,
        ))
        .with_process_engine("testing-fixture", |_| {
            Ok(crate::ProcessEngineRegistration::accepting(Arc::new(
                crate::testing::FixtureProcessEngine,
            )))
        });
    let replaced = recipe
        .clone()
        .with_configuration(ConfigRegistrar::new("test_protocol"));
    assert!(
        replaced.entries.iter().all(|entry| !matches!(
            entry.kind,
            ContributionKind::ConfigNamespace | ContributionKind::ConfigCommand
        )),
        "REGISTRATION-IS-INVENTORY: replacement removes the old configuration descriptors"
    );
    let mut replacement = ConfigRegistrar::new("test_protocol");
    replacement.owner(Config).unwrap();
    let replaced = recipe.clone().with_configuration(replacement);
    assert_eq!(
        replaced
            .entries
            .iter()
            .filter(|entry| entry.kind == ContributionKind::ConfigNamespace)
            .count(),
        1
    );
    assert!(
        !replaced
            .entries
            .iter()
            .any(|entry| entry.kind == ContributionKind::ConfigCommand)
    );
    assert!(
        replaced
            .entries
            .iter()
            .enumerate()
            .all(|(index, entry)| entry.registration_order == u32::try_from(index).unwrap())
    );
    let factory = PluginRecipeFactory::new(PluginDeclaration::initial("test_protocol"), recipe);
    let host = PluginHost::new(
        vec![Arc::new(factory)],
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(Arc::new(crate::SystemClock)),
    );
    let declared = host.installed_inventory().unwrap();
    assert_eq!(constructions.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(expansions.load(std::sync::atomic::Ordering::SeqCst), 0);
    let kinds = declared
        .entries
        .iter()
        .map(|entry| entry.kind.clone())
        .collect::<BTreeSet<_>>();
    for slot in CallbackSlot::ALL {
        assert!(
            kinds.contains(&ContributionKind::Callback(*slot)),
            "{slot:?} missing from inventory"
        );
    }
    for kind in [
        ContributionKind::PromptSection,
        ContributionKind::PromptFamily,
        ContributionKind::PromptWrapper,
        ContributionKind::StateReducer,
        ContributionKind::ConfigNamespace,
        ContributionKind::ConfigCommand,
        ContributionKind::Extension,
        ContributionKind::ProcessEngine,
    ] {
        assert!(kinds.contains(&kind), "{kind:?} missing from inventory");
    }
    let session = host
        .build_session(PluginSessionRequest::creation(
            "test_protocol",
            SessionAuthorityContext::fixture(),
        ))
        .unwrap();
    assert_eq!(constructions.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        expansions.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "family members are not expanded to describe an installation"
    );
    assert_eq!(
        declared.entries.len(),
        session.installed_inventory().unwrap().entries.len()
    );
    assert!(
        session
            .installed_inventory()
            .unwrap()
            .callbacks()
            .any(|key| *key == provider_handle.callback_identity())
    );
    assert!(session.tool_registry().serves(&provider_handle));
    let before = &session.capabilities().contributions.before_turn_hooks[0];
    assert!(
        session
            .installed_inventory()
            .unwrap()
            .callbacks()
            .any(|key| key == &before.identity)
    );
    assert_eq!(before.identity.key, "before_turn:before");
    assert_eq!(
        session.session_extensions().payloads("owner-extension"),
        &[serde_json::Value::Null]
    );
    assert_eq!(
        session.extensions().payloads("factory-extension"),
        &[serde_json::Value::Null]
    );
}

/// PORT-PREFLIGHT-PRECEDES-BIND: missing ports construct no owners.
#[test]
fn port_preflight_precedes_bind() {
    let binds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&binds);
    let factory = PluginRecipeFactory::new(
        PluginDeclaration::initial("test_protocol"),
        PluginRecipe::empty("test_protocol", move |_, _| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }),
    );
    let result = PluginHost::new(
        vec![Arc::new(factory)],
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(Arc::new(crate::SystemClock)),
    )
    .build_session(PluginSessionRequest::creation(
        "missing-port",
        SessionAuthorityContext::fixture(),
    ));
    assert!(matches!(
        result,
        Err(PluginError::Preflight(PreflightIssue::MissingPort {
            port: RequiredPort::ProtocolSession
        }))
    ));
    assert_eq!(binds.load(std::sync::atomic::Ordering::SeqCst), 0);
    let counter = Arc::clone(&binds);
    let mut recipe = PluginRecipe::empty("missing-service", move |_, _| {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    });
    recipe
        .requirements
        .services
        .insert(ServiceKind::CurrentSessionSnapshot);
    let factory = PluginRecipeFactory::new(PluginDeclaration::initial("missing-service"), recipe);
    let result = crate::testing::test_plugin_host(vec![Arc::new(factory)]).build_session(
        PluginSessionRequest::creation("missing-service", SessionAuthorityContext::fixture()),
    );
    assert!(
        matches!(result, Err(PluginError::Preflight(PreflightIssue::MissingService { owner, service: ServiceKind::CurrentSessionSnapshot })) if owner == "missing-service")
    );
    assert_eq!(binds.load(std::sync::atomic::Ordering::SeqCst), 0);
    struct Duplicate;
    impl SessionPlugin for Duplicate {
        fn id(&self) -> &'static str {
            "duplicate-port"
        }
    }
    let mut reg = PluginRegistrar::<Duplicate>::new("duplicate-port");
    reg.protocol()
        .session(|_, _| {
            Err(PluginError::Invoke(
                "a refused binder cannot execute".into(),
            ))
        })
        .unwrap();
    let counter = Arc::clone(&binds);
    let recipe = PluginRecipe::new("duplicate-port", reg, move |_, _| {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Arc::new(Duplicate))
    });
    let factory = PluginRecipeFactory::new(PluginDeclaration::initial("duplicate-port"), recipe);
    let result = crate::testing::test_plugin_host(vec![Arc::new(factory)]).build_session(
        PluginSessionRequest::creation("duplicate-port", SessionAuthorityContext::fixture()),
    );
    assert!(matches!(
        result,
        Err(PluginError::Preflight(PreflightIssue::DuplicatePort {
            port: RequiredPort::ProtocolSession,
            ..
        }))
    ));
    assert_eq!(binds.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// REGISTRATION-IS-INVENTORY includes a config-only process component.
#[test]
fn process_configuration_is_inventory() {
    let factory =
        StaticPluginFactory::new(PluginDeclaration::initial("config-only"), PluginSpec::new());
    let mut registry = ProcessComponentRegistry::new();
    registry
        .register(
            Arc::new(factory.as_component(FormatVersion::ONE).unwrap()),
            crate::ProcessConfigKey::<()>::new("config-only", FormatVersion::ONE),
        )
        .unwrap();
    let inventory = registry.installed_inventory();
    assert_eq!(inventory.entries.len(), 1);
    assert_eq!(inventory.entries[0].kind, ContributionKind::ConfigNamespace);
    assert_eq!(inventory.entries[0].registry, RegistryKind::Process);
    assert_eq!(inventory.entries[0].key.owner(), "config-only");
    assert_eq!(
        inventory.served_tokens().len(),
        0,
        "config codecs are not callbacks"
    );
    assert_eq!(
        registry.served_tokens().len(),
        1,
        "config formats remain served"
    );
}

/// REGISTRATION-IS-INVENTORY includes a pure, owner-attributed decoder.
#[test]
fn transcript_decoder_is_inventory() {
    struct Decoder;
    impl TranscriptDecoderPlugin for Decoder {
        fn decode_event(
            &self,
            _: &crate::ProtocolEvent,
        ) -> Result<
            Option<lash_core_store::transcript::TranscriptItem>,
            lash_core_store::runtime_error::StoredDataCorruption,
        > {
            Ok(None)
        }
    }
    struct Factory;
    impl PluginMetadata for Factory {
        fn plugin_declaration(&self) -> PluginDeclaration {
            PluginDeclaration::initial("decoder")
        }
    }
    impl PluginFactory for Factory {
        fn id(&self) -> &'static str {
            "decoder"
        }
        fn recipe(self: Arc<Self>) -> Result<PluginRecipe, PluginError> {
            Ok(PluginRecipe::empty("decoder", |_, _| {
                Err(PluginError::Invoke("metadata cannot bind".into()))
            })
            .with_transcript_decoder(Arc::new(Decoder)))
        }
    }
    let host = crate::testing::test_plugin_host(vec![Arc::new(Factory)]);
    let inventory = host.installed_inventory().unwrap();
    assert!(
        inventory
            .readiness
            .iter()
            .all(|ready| *ready == ContributionReadiness::NotValidated)
    );
    assert!(
        inventory.served_tokens().is_empty(),
        "unvalidated declarations serve no callbacks"
    );
    assert!(
        inventory
            .entries
            .iter()
            .any(|entry| entry.key.owner() == "decoder"
                && entry.kind == ContributionKind::TranscriptDecoder)
    );
    assert!(!inventory.callbacks().any(|key| key.owner == "decoder"));
    assert!(format!("{:?}", host.transcript_decoders().unwrap()).contains("decoders: 1"));
    let host = host.with_protocol_plugin(Arc::new(Factory));
    assert!(format!("{:?}", host.transcript_decoders().unwrap()).contains("decoders: 1"));
}

/// Descriptive operation readers use registration order before owner binding.
#[test]
fn operation_metadata_comes_from_frozen_inventory() {
    let mut reg = PluginRegistrar::<()>::new("operations");
    for name in ["z-first", "a-second"] {
        reg.operations()
            .register(PluginOperationRegistration::query(
                PluginOperationSpec {
                    name: name.into(),
                    description: name.into(),
                    session_param: SessionParam::Forbidden,
                    input_schema: serde_json::json!({}),
                    output_schema: serde_json::json!({}),
                    error_type: "test".into(),
                    error_version: FormatVersion::ONE,
                    error_schema: serde_json::json!({}),
                },
                Arc::new(|_, _| Box::pin(async { Ok(serde_json::Value::Null) })),
            ))
            .unwrap();
    }
    let recipe = PluginRecipe::bound("operations", reg, |_, _| {
        Err(PluginError::Invoke("metadata cannot bind".into()))
    });
    let host = crate::testing::test_plugin_host(vec![Arc::new(PluginRecipeFactory::new(
        PluginDeclaration::initial("operations"),
        recipe,
    ))]);
    let session = host
        .defer_session(PluginSessionRequest::creation(
            "metadata",
            SessionAuthorityContext::fixture(),
        ))
        .unwrap();
    assert_eq!(
        session
            .plugin_operations()
            .iter()
            .map(|op| op.name.as_str())
            .collect::<Vec<_>>(),
        ["z-first", "a-second"]
    );
    assert!(
        session.installed_inventory().is_none(),
        "reading definitions binds no owner"
    );
}

/// Frozen inventory owns descriptive prompt metadata after execution storage is partitioned.
#[test]
fn prompt_metadata_comes_from_frozen_inventory() {
    let mut reg = PluginRegistrar::<()>::new("prompt-metadata");
    let spec = PromptSectionSpec::new(
        PromptSectionKey::new("instructions").unwrap(),
        PromptPlacement::InitialInstructions,
    );
    reg.prompt()
        .section(spec, |_, _| {
            panic!("descriptive reads cannot bind renderers")
        })
        .unwrap();
    reg.prompt()
        .family(
            super::super::prompt::PromptSectionFamilySpec::new(
                PromptSectionKey::new("tools").unwrap(),
                PromptPlacement::CurrentContext,
            ),
            |_, _| panic!("descriptive reads cannot bind family sources"),
        )
        .unwrap();
    reg.prompt()
        .wrap(
            super::super::prompt::PromptWrapSpec::new(
                crate::prompt_sections::PromptWrapKey::new("wrap").unwrap(),
                super::super::prompt::PromptSectionId::new(
                    "prompt-metadata",
                    PromptSectionKey::new("instructions").unwrap(),
                ),
            ),
            |_, _| panic!("descriptive reads cannot bind wrappers"),
        )
        .unwrap();
    let inventory = InstalledInventory {
        scope: InventoryScope::Deployment(RegistryKind::Session),
        entries: reg.descriptors("prompt-metadata", RegistryKind::Session),
        readiness: vec![ContributionReadiness::Ready; 3],
    };
    let expected = super::super::prompt::PromptCatalogSnapshot::from_inventory(&inventory);
    let mut contributions = PluginContributions {
        inventory: Some(Arc::new(inventory)),
        ..Default::default()
    };
    let registry = super::super::runtime_impl::build_tool_registry(&contributions).unwrap();
    let composition = ExecutionComposition::new(&mut contributions, registry);
    assert_eq!(composition.prompt_catalog().sections(), expected.sections());
    assert_eq!(composition.prompt_catalog().families(), expected.families());
    assert_eq!(composition.prompt_catalog().wraps(), expected.wraps());
}

/// A provider of one tool, `keyed`, that answers with its `label`. A
/// provider that does not `advertise` still resolves the tool by id, as
/// a grant-only provider does.
struct Keyed {
    label: &'static str,
    advertises: bool,
}

impl Keyed {
    fn definition() -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            KEYED_TOOL,
            "keyed",
            "keyed probe",
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "string" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(30))
    }
}

#[async_trait::async_trait]
impl crate::ToolProvider for Keyed {
    fn tool_manifests(
        &self,
        _config: &crate::OwnerConfigView,
    ) -> std::result::Result<Vec<crate::ToolManifest>, crate::PluginError> {
        Ok({
            if self.advertises {
                vec![Self::definition().manifest()]
            } else {
                Vec::new()
            }
        })
    }

    fn resolve_manifest_by_id(
        &self,
        id: &crate::ToolId,
        _config: &crate::OwnerConfigView,
    ) -> Option<crate::ToolManifest> {
        (id.as_str() == KEYED_TOOL).then(|| Self::definition().manifest())
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &crate::OwnerConfigView,
    ) -> Option<Arc<crate::ToolContract>> {
        (name == "keyed").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolOutcome::ok(serde_json::json!(self.label)).into()
    }
}

/// The advertising provider under `served`, then the id-only one under
/// `hidden`, or the reverse.
fn keyed_providers(served_first: bool) -> Vec<(ToolProviderKey, Arc<dyn ToolProvider>)> {
    let served = (
        crate::tool_provider_key!("served"),
        Arc::new(Keyed {
            label: "served",
            advertises: true,
        }) as Arc<dyn ToolProvider>,
    );
    let hidden = (
        crate::tool_provider_key!("hidden"),
        Arc::new(Keyed {
            label: "hidden",
            advertises: false,
        }) as Arc<dyn ToolProvider>,
    );
    if served_first {
        vec![served, hidden]
    } else {
        vec![hidden, served]
    }
}

/// What a call of `keyed` records as its binding under `composition`,
/// matched on the surface `registry` advertises, and what dispatch
/// through that surface executes.
async fn bound_and_executed(
    composition: &ExecutionComposition,
    registry: Arc<crate::ToolRegistry>,
    config: crate::OwnerConfigView,
) -> (crate::tool_run::AdmittedBinding, serde_json::Value) {
    let entries = registry
        .advertise(&config)
        .expect("the providers advertise");
    let tools = crate::SurfaceTools::of_entries(entries, registry);
    let tool = crate::ToolId::from(KEYED_TOOL);
    let provider = &tools
        .entry(&tool)
        .expect("the surface offers the tool")
        .provider;
    let binding = composition
        .tool_run_binding(&tool, Some(provider))
        .expect("the surface binds the tool");
    let crate::ToolAttemptOutcome::Done { result, .. } =
        crate::testing::run_tool(&tools, "keyed", &serde_json::json!({})).await
    else {
        panic!("the call completes inline");
    };
    let executed = crate::ToolOutcome::from_output(result.into_output()).value_for_projection();
    (binding, executed)
}

fn session_of(factories: Vec<Arc<dyn PluginFactory>>) -> Arc<PluginSession> {
    crate::testing::test_plugin_host(factories)
        .build_session(PluginSessionRequest::creation(
            "keyed-providers",
            SessionAuthorityContext::fixture(),
        ))
        .expect("materialized session")
}

fn keyed_plugin(
    id: &'static str,
    providers: Vec<(ToolProviderKey, Arc<dyn ToolProvider>)>,
) -> Arc<dyn PluginFactory> {
    let spec = providers
        .into_iter()
        .fold(PluginSpec::new(), |spec, (key, provider)| {
            spec.with_tool_provider(key, provider)
        });
    Arc::new(StaticPluginFactory::new(
        PluginDeclaration::initial(id),
        spec,
    ))
}

/// KEYED-PROVIDERS (FIG-6073): a call binds to and dispatches through one
/// declared provider identity, so reordering two provider registrations
/// (in one plugin, as builder tool plugins, or in one process component)
/// changes nothing recorded or executed, even when another provider
/// resolves the same tool id.
#[tokio::test]
async fn reordering_provider_registrations_changes_nothing_recorded_or_executed() {
    let served = PluginCallbackIdentity::tool_provider("keyed", "served");
    for served_first in [true, false] {
        let session = session_of(vec![keyed_plugin("keyed", keyed_providers(served_first))]);
        let (binding, executed) = bound_and_executed(
            &session.composition(),
            session.tool_registry(),
            crate::OwnerConfigView::default(),
        )
        .await;
        assert_eq!(binding.executable, served, "served first: {served_first}");
        assert_eq!(binding.preparation, served, "served first: {served_first}");
        assert_eq!(executed, serde_json::json!("served"));
    }

    for served_first in [true, false] {
        let plugins = keyed_providers(served_first)
            .into_iter()
            .map(|(key, provider)| keyed_plugin(key.as_str(), vec![(key, provider)]))
            .collect();
        let session = session_of(plugins);
        let (binding, executed) = bound_and_executed(
            &session.composition(),
            session.tool_registry(),
            crate::OwnerConfigView::default(),
        )
        .await;
        assert_eq!(
            binding.executable,
            PluginCallbackIdentity::tool_provider("served", "served"),
            "served first: {served_first}"
        );
        assert_eq!(executed, serde_json::json!("served"));
    }

    let key = crate::ProcessConfigKey::<()>::new("keyed-component", crate::FormatVersion::ONE);
    for served_first in [true, false] {
        let mut components = ProcessComponentRegistry::new();
        components
            .register(
                Arc::new(
                    StaticPluginFactory::new(
                        PluginDeclaration::initial("keyed-component"),
                        keyed_providers(served_first)
                            .into_iter()
                            .fold(PluginSpec::new(), |spec, (key, provider)| {
                                spec.with_tool_provider(key, provider)
                            }),
                    )
                    .as_component(key.format())
                    .expect("tool providers are a process component"),
                ),
                key,
            )
            .expect("the component registers");
        let composition = components
            .compose(
                &crate::testing::component_env("keyed-component", key.format()),
                None,
            )
            .expect("the components compose");
        let (binding, executed) = bound_and_executed(
            &composition.composition,
            Arc::clone(&composition.registry),
            crate::OwnerConfigView::components(Arc::new(crate::testing::component_env(
                "keyed-component",
                key.format(),
            ))),
        )
        .await;
        assert_eq!(
            binding.executable,
            PluginCallbackIdentity::tool_provider("keyed-component", "served"),
            "served first: {served_first}"
        );
        assert_eq!(executed, serde_json::json!("served"));
    }
}
