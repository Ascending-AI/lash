use super::*;
use crate::plugin::registrar::PluginContributions;
use lash_sansio::sync::MutexExt;

mod composer;

type Register = Box<dyn Fn(&mut PluginRegistrar) -> Result<(), PluginError>>;

/// Register `plugins` in order, as a session build does.
fn catalog(plugins: Vec<(&'static str, Register)>) -> Result<PromptCatalog, PluginError> {
    let mut contributions = PluginContributions::default();
    for (id, register) in plugins {
        let mut reg = PluginRegistrar::new(PluginRevision::new(
            id,
            crate::plugin::BehaviorRevision::ONE,
        ));
        reg.contributions = contributions;
        register(&mut reg)?;
        contributions = reg.contributions;
    }
    Ok(PromptCatalog::new(contributions.prompt))
}

fn key(key: &str) -> PromptSectionKey {
    PromptSectionKey::new(key).expect("valid section key")
}

fn wrap_key(key: &str) -> PromptWrapKey {
    PromptWrapKey::new(key).expect("valid wrap key")
}

fn id(owner: &str, local: &str) -> PromptSectionId {
    PromptSectionId::new(owner, key(local))
}

fn cut(namespaces: BTreeMap<String, CommittedPluginNamespace>) -> PromptCut {
    PromptCut::new(PromptCutParts {
        call: PromptCall {
            session_id: crate::SessionId::from("prompt-laws"),
            frame: None,
            run: None,
            turn: None,
            iteration: 0,
            call: 0,
            purpose: PromptPurpose::Turn,
        },
        config: crate::AdmittedPluginConfig::default(),
        session: None,
        offered: OfferedTools::default(),
        model: PromptModel::default(),
        history: ProjectedHistoryStats::default(),
        namespaces,
    })
}

fn fixed(text: &'static str) -> Arc<dyn PromptSection> {
    Arc::new(move |_: &PromptInput<'_>| Ok(SectionText::text(text)))
}

fn section(local: &'static str, placement: PromptPlacement, text: &'static str) -> Register {
    Box::new(move |reg| {
        reg.prompt()
            .section(PromptSectionSpec::new(key(local), placement), fixed(text))
    })
}

/// A wrapper that encloses the text so far in `name(...)`.
fn enclose(name: &'static str) -> Arc<dyn PromptSectionWrap> {
    Arc::new(
        move |_: &PromptInput<'_>, _: PromptWrapTarget<'_>, previous: SectionText| {
            Ok(SectionText::Text(format!(
                "{name}({})",
                previous.as_text().unwrap_or("")
            )))
        },
    )
}

fn compose_one(catalog: &PromptCatalog, plan: &PromptPlan) -> ComposedSection {
    let composition = catalog
        .resolve(plan, &PromptPurpose::Turn, &OfferedTools::default())
        .expect("plan resolves");
    composition
        .compose_section(0, &cut(BTreeMap::new()))
        .expect("section composes")
}

/// KEYS: a section is owned as (plugin id, local key). One plugin cannot
/// register a key twice; another plugin may use the same local key; and any
/// plugin may wrap any section, a protocol's included, replacing its text.
#[test]
fn a_section_key_registers_once_per_plugin_and_any_plugin_wraps_any_section() {
    let duplicate = catalog(vec![(
        "lash.protocol",
        Box::new(|reg| {
            reg.prompt().section(
                PromptSectionSpec::new(key("intro"), PromptPlacement::InitialInstructions),
                fixed("one"),
            )?;
            reg.prompt().section(
                PromptSectionSpec::new(key("intro"), PromptPlacement::InitialInstructions),
                fixed("two"),
            )
        }),
    )]);
    assert!(
        matches!(&duplicate, Err(PluginError::Registration(message)) if message.contains("lash.protocol/intro")),
        "a duplicate base identity is refused"
    );
    let duplicate_wrap = catalog(vec![(
        "wrapper",
        Box::new(|reg| {
            let spec = PromptWrapSpec::new(wrap_key("w"), id("lash.protocol", "intro"));
            reg.prompt().wrap(spec.clone(), enclose("A"))?;
            reg.prompt().wrap(spec, enclose("B"))
        }),
    )]);
    assert!(matches!(duplicate_wrap, Err(PluginError::Registration(_))));

    let catalog = catalog(vec![
        (
            "lash.protocol",
            section(
                "intro",
                PromptPlacement::InitialInstructions,
                "protocol intro",
            ),
        ),
        (
            "other",
            section("intro", PromptPlacement::InitialInstructions, "other intro"),
        ),
        (
            "wrapper",
            Box::new(|reg| {
                reg.prompt().wrap(
                    PromptWrapSpec::new(wrap_key("replace"), id("lash.protocol", "intro")),
                    Arc::new(
                        |_: &PromptInput<'_>, _: PromptWrapTarget<'_>, _: SectionText| {
                            Ok(SectionText::text("replaced by a trusted wrapper"))
                        },
                    ),
                )
            }),
        ),
    ])
    .expect("distinct owners may share a local key");
    let sections = catalog
        .sections()
        .into_iter()
        .map(|info| info.section)
        .collect::<Vec<_>>();
    assert_eq!(
        sections,
        vec![id("lash.protocol", "intro"), id("other", "intro")]
    );
    let composition = catalog
        .resolve(
            &PromptPlan::default(),
            &PromptPurpose::Turn,
            &OfferedTools::default(),
        )
        .expect("plan resolves");
    let protocol = composition
        .compose_section(0, &cut(BTreeMap::new()))
        .expect("protocol section composes");
    assert_eq!(protocol.base, SectionText::text("protocol intro"));
    assert_eq!(
        protocol.value,
        SectionText::text("replaced by a trusted wrapper")
    );
    assert_eq!(
        composition.record().sections[0].wraps[0].owner.plugin,
        "wrapper"
    );
    let other = composition
        .compose_section(1, &cut(BTreeMap::new()))
        .expect("other section composes");
    assert_eq!(other.value, SectionText::text("other intro"));
}

/// ORDER: a section's wrapper chain runs in plugin registration order, then
/// declaration order within a plugin: wrappers A then B yield B(A(base)).
#[test]
fn wrappers_compose_in_plugin_registration_then_declaration_order() {
    let target = || id("base", "s");
    let plugins = || -> Vec<(&'static str, Register)> {
        vec![
            (
                "base",
                section("s", PromptPlacement::InitialInstructions, "R"),
            ),
            (
                "a",
                Box::new(move |reg| {
                    reg.prompt()
                        .wrap(PromptWrapSpec::new(wrap_key("a"), target()), enclose("A"))
                }),
            ),
            (
                "b",
                Box::new(move |reg| {
                    reg.prompt()
                        .wrap(PromptWrapSpec::new(wrap_key("b1"), target()), enclose("B1"))?;
                    reg.prompt()
                        .wrap(PromptWrapSpec::new(wrap_key("b2"), target()), enclose("B2"))
                }),
            ),
        ]
    };
    let composed = compose_one(&catalog(plugins()).unwrap(), &PromptPlan::default());
    assert_eq!(composed.value, SectionText::text("B2(B1(A(R)))"));
    assert_eq!(
        composed
            .wraps
            .iter()
            .map(|(wrap, output)| (wrap.to_string(), output.as_text().unwrap().to_string()))
            .collect::<Vec<_>>(),
        vec![
            ("a/a".to_string(), "A(R)".to_string()),
            ("b/b1".to_string(), "B1(A(R))".to_string()),
            ("b/b2".to_string(), "B2(B1(A(R)))".to_string()),
        ],
        "each wrapper sees the previous one's output"
    );

    let mut reversed = plugins();
    reversed.reverse();
    let composed = compose_one(&catalog(reversed).unwrap(), &PromptPlan::default());
    assert_eq!(
        composed.value,
        SectionText::text("A(B2(B1(R)))"),
        "registration order, not anything else, decides the chain"
    );
}

/// HOST-PLACEMENT: the host's placement wins over each plugin default, the
/// plan's order comes first, and the resolved record keeps both, so a call
/// resumed from it never consults a deployment's defaults.
#[test]
fn a_host_placement_overrides_the_plugin_default_and_the_record_keeps_it() {
    let plugins = |memory_default: PromptPlacement| -> Vec<(&'static str, Register)> {
        vec![(
            "p",
            Box::new(move |reg| {
                reg.prompt().section(
                    PromptSectionSpec::new(key("intro"), PromptPlacement::InitialInstructions),
                    fixed("intro"),
                )?;
                reg.prompt().section(
                    PromptSectionSpec::new(key("memory"), memory_default),
                    fixed("T = 1"),
                )?;
                reg.prompt().section(
                    PromptSectionSpec::new(key("late"), PromptPlacement::CurrentContext),
                    fixed("late"),
                )
            }),
        )]
    };
    let plan = PromptPlan {
        order: vec![id("p", "late"), id("p", "memory")],
        placements: vec![
            PromptSectionPlacement {
                section: id("p", "memory"),
                placement: PromptPlacement::InitialInstructions,
            },
            PromptSectionPlacement {
                section: id("p", "intro"),
                placement: PromptPlacement::CurrentContext,
            },
        ],
        ..PromptPlan::default()
    };
    let resolved = catalog(plugins(PromptPlacement::CurrentContext))
        .unwrap()
        .resolve(&plan, &PromptPurpose::Turn, &OfferedTools::default())
        .expect("plan resolves");
    let placed = |record: &ResolvedPromptPlan| {
        record
            .sections
            .iter()
            .map(|section| {
                (
                    section.section.key.to_string(),
                    section.placement,
                    section.placement_source,
                )
            })
            .collect::<Vec<_>>()
    };
    let expected = vec![
        (
            "late".to_string(),
            PromptPlacement::CurrentContext,
            PlacementSource::PluginDefault,
        ),
        (
            "memory".to_string(),
            PromptPlacement::InitialInstructions,
            PlacementSource::Host,
        ),
        (
            "intro".to_string(),
            PromptPlacement::CurrentContext,
            PlacementSource::Host,
        ),
    ];
    assert_eq!(placed(resolved.record()), expected);

    let stored = serde_json::to_value(resolved.record()).expect("record encodes");
    let restored: ResolvedPromptPlan = serde_json::from_value(stored).expect("record decodes");
    assert_eq!(
        placed(&restored),
        expected,
        "the record carries the placements"
    );

    let redeployed = catalog(plugins(PromptPlacement::InitialInstructions))
        .unwrap()
        .resolve(&plan, &PromptPurpose::Turn, &OfferedTools::default())
        .expect("plan resolves");
    assert_eq!(
        placed(redeployed.record()),
        expected,
        "a changed plugin default does not move a host-placed section"
    );

    let unknown = PromptPlan {
        placements: vec![PromptSectionPlacement {
            section: id("p", "absent"),
            placement: PromptPlacement::CurrentContext,
        }],
        ..PromptPlan::default()
    };
    assert!(matches!(
        catalog(plugins(PromptPlacement::CurrentContext))
            .unwrap()
            .resolve(&unknown, &PromptPurpose::Turn, &OfferedTools::default()),
        Err(PromptPlanError::UnknownSection { .. })
    ));
}

/// READ-CUT: a renderer reads its namespace frozen at one generation. A
/// publication that lands while it renders is invisible to it, and it sees
/// no other plugin's namespace.
#[test]
fn a_renderer_reads_one_namespace_generation() {
    let registry = Arc::new(std::sync::Mutex::new(
        crate::plugin::state::PluginStateRegistry::default(),
    ));
    {
        let mut registry = registry.lock_recover();
        for (plugin, key, value) in [("mem", "t", 1), ("neighbor", "secret", 7)] {
            let namespace = registry.data.plugins.entry(plugin.into()).or_default();
            namespace.generation = 1;
            namespace
                .values
                .insert(key.into(), serde_json::json!(value));
        }
    }
    let owner = crate::RuntimeOwner::Session(crate::SessionId::from("prompt-laws"));
    let view = crate::plugin::PluginStateView::bind(&owner, "mem", Arc::clone(&registry));
    let neighbor = crate::plugin::PluginStateView::bind(&owner, "neighbor", Arc::clone(&registry));
    let frozen = cut(BTreeMap::from([
        ("mem".to_string(), view.committed()),
        ("neighbor".to_string(), neighbor.committed()),
    ]));

    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let renderer = {
        let registry = Arc::clone(&registry);
        let observed = Arc::clone(&observed);
        Arc::new(move |input: &PromptInput<'_>| {
            let before = (input.state().generation(), input.state().get("t").cloned());
            {
                let mut registry = registry.lock_recover();
                let namespace = registry.data.plugins.get_mut("mem").expect("namespace");
                namespace.generation = 2;
                namespace.values.insert("t".into(), serde_json::json!(2));
            }
            let after = (input.state().generation(), input.state().get("t").cloned());
            observed.lock_recover().push((before, after));
            Ok(SectionText::text(format!(
                "keys: {}",
                input.state().keys().collect::<Vec<_>>().join(",")
            )))
        })
    };
    let catalog = catalog(vec![(
        "mem",
        Box::new(move |reg| {
            reg.prompt().section(
                PromptSectionSpec::new(key("current"), PromptPlacement::CurrentContext),
                renderer.clone(),
            )
        }),
    )])
    .unwrap();
    let composed = catalog
        .resolve(
            &PromptPlan::default(),
            &PromptPurpose::Turn,
            &OfferedTools::default(),
        )
        .unwrap()
        .compose_section(0, &frozen)
        .unwrap();

    let observed = observed.lock_recover().clone();
    assert_eq!(
        observed,
        vec![(
            (1, Some(serde_json::json!(1))),
            (1, Some(serde_json::json!(1)))
        )],
        "both reads see generation 1, though generation 2 published between them"
    );
    assert_eq!(view.generation(), 2, "the live namespace did move");
    assert_eq!(composed.value, SectionText::text("keys: t"));
}

/// EXCLUDE: the plan is the host's, so the host can drop any section. A
/// section the plan places `Excluded` is recorded with that placement, as the
/// host's choice; neither its renderer nor a wrapper over it runs, the
/// wrapper is recorded as absent, and the section composes to an omission.
#[test]
fn a_host_excluded_section_is_recorded_and_its_renderer_never_runs() {
    let rendered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let wrapped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counting_renderer = {
        let rendered = Arc::clone(&rendered);
        Arc::new(move |_: &PromptInput<'_>| {
            rendered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(SectionText::text("protocol intro"))
        }) as Arc<dyn PromptSection>
    };
    let counting_wrapper = {
        let wrapped = Arc::clone(&wrapped);
        Arc::new(
            move |_: &PromptInput<'_>, _: PromptWrapTarget<'_>, previous: SectionText| {
                wrapped.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(previous)
            },
        ) as Arc<dyn PromptSectionWrap>
    };
    let catalog = catalog(vec![
        (
            "protocol",
            Box::new(move |reg| {
                reg.prompt().section(
                    PromptSectionSpec::new(key("intro"), PromptPlacement::InitialInstructions),
                    counting_renderer.clone(),
                )
            }),
        ),
        (
            "addon",
            Box::new(move |reg| {
                reg.prompt().wrap(
                    PromptWrapSpec::new(wrap_key("tag"), id("protocol", "intro")),
                    counting_wrapper.clone(),
                )
            }),
        ),
    ])
    .unwrap();
    let plan = PromptPlan {
        placements: vec![PromptSectionPlacement {
            section: id("protocol", "intro"),
            placement: PromptPlacement::Excluded,
        }],
        ..PromptPlan::default()
    };

    let composition = catalog
        .resolve(&plan, &PromptPurpose::Turn, &OfferedTools::default())
        .unwrap();
    let composed = composition
        .compose_section(0, &cut(BTreeMap::new()))
        .unwrap();

    let record = composition.record();
    assert_eq!(record.sections.len(), 1, "the exclusion is recorded");
    assert_eq!(record.sections[0].placement, PromptPlacement::Excluded);
    assert_eq!(record.sections[0].placement_source, PlacementSource::Host);
    assert!(record.sections[0].wraps.is_empty(), "no chain runs over it");
    assert_eq!(
        record
            .absent_targets
            .iter()
            .map(|wrap| wrap.target.clone())
            .collect::<Vec<_>>(),
        vec![id("protocol", "intro")],
        "its wrapper is recorded as absent"
    );
    assert_eq!(composed.base, SectionText::Omit);
    assert_eq!(composed.value, SectionText::Omit);
    assert_eq!(
        rendered.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the renderer never runs"
    );
    assert_eq!(
        wrapped.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the wrapper never runs"
    );
}

/// A family source contributing one section per offered tool module, as
/// tool guidance does.
struct ModuleGuidance;

impl PromptSectionSource for ModuleGuidance {
    fn sections(&self, offered: &OfferedTools) -> Vec<PromptFamilySection> {
        let modules = offered
            .manifests()
            .filter_map(|manifest| manifest.module.as_deref())
            .map(|module| module.name.clone())
            .collect::<BTreeSet<_>>();
        modules
            .into_iter()
            .map(|module| PromptFamilySection {
                suffix: key(&module),
                renderer: Arc::new(move |_: &PromptInput<'_>| {
                    Ok(SectionText::Text(format!("guidance for {module}")))
                }),
            })
            .collect()
    }
}

fn module_tool(name: &str, module: &str) -> crate::ToolDefinition {
    let mut tool = crate::ToolDefinition::raw(
        name,
        name,
        "",
        serde_json::json!({"type": "object"}),
        serde_json::json!({}),
    )
    .expect("tool schema admits");
    tool.manifest.module = Some(Arc::new(crate::ToolModule {
        name: module.into(),
    }));
    tool
}

/// A call offering `tools` natively, from their pinned catalog.
fn offering(tools: Vec<crate::ToolDefinition>) -> OfferedTools {
    OfferedTools {
        native: tools
            .iter()
            .map(|tool| tool.manifest.name.clone())
            .collect(),
        callable: Vec::new(),
        catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(tools)),
    }
}

/// OFFERED: tool guidance is selected only when its surface is offered. A
/// family contributes a section for each offered module and none for a
/// module the call does not offer. The host may order, place and wrap a
/// family section by its key, and may name one this call does not offer;
/// a key outside every family is still unknown.
#[test]
fn a_family_section_is_selected_only_when_its_tools_are_offered() {
    let catalog = catalog(vec![(
        "tools",
        Box::new(|reg| {
            reg.prompt().family(
                PromptSectionFamilySpec::new(key("module"), PromptPlacement::InitialInstructions),
                Arc::new(ModuleGuidance),
            )
        }),
    )])
    .unwrap();
    let plan = PromptPlan {
        placements: vec![
            PromptSectionPlacement {
                section: id("tools", "module.github"),
                placement: PromptPlacement::CurrentContext,
            },
            PromptSectionPlacement {
                section: id("tools", "module.slack"),
                placement: PromptPlacement::Excluded,
            },
        ],
        ..PromptPlan::default()
    };

    let none = catalog
        .resolve(&plan, &PromptPurpose::Turn, &OfferedTools::default())
        .unwrap();
    assert!(
        none.record().sections.is_empty(),
        "no offered tools, no guidance"
    );

    let offered = offering(vec![module_tool("search", "github")]);
    let github = catalog
        .resolve(&plan, &PromptPurpose::Turn, &offered)
        .unwrap();
    let record = &github.record().sections;
    assert_eq!(
        record
            .iter()
            .map(|section| (section.section.clone(), section.placement))
            .collect::<Vec<_>>(),
        vec![(
            id("tools", "module.github"),
            PromptPlacement::CurrentContext
        )],
        "only the offered module's section, placed by the host"
    );
    assert_eq!(
        github
            .compose_section(0, &cut(BTreeMap::new()))
            .unwrap()
            .value,
        SectionText::text("guidance for github")
    );

    let unknown = PromptPlan {
        order: vec![id("tools", "other")],
        ..PromptPlan::default()
    };
    assert!(matches!(
        catalog.resolve(&unknown, &PromptPurpose::Turn, &offered),
        Err(PromptPlanError::UnknownSection { .. })
    ));
}
