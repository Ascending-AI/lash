use super::*;

fn id(owner: &str, key: &str) -> PromptSectionId {
    PromptSectionId::new(owner, PromptSectionKey::new(key).expect("valid key"))
}

#[test]
fn a_prompt_key_outside_its_alphabet_does_not_decode() {
    for refused in ["", "Upper", "-lead", "has/slash", &"k".repeat(65)] {
        assert!(PromptSectionKey::new(refused).is_err(), "{refused:?}");
        assert!(serde_json::from_value::<PromptWrapKey>(serde_json::json!(refused)).is_err());
    }
    assert_eq!(
        PromptSectionKey::new("current.memory_v2").unwrap().as_str(),
        "current.memory_v2"
    );
}

#[test]
fn a_plan_orders_and_places_each_section_once() {
    let memory = id("optmem", "current");
    let ordered_twice = PromptPlan {
        order: vec![memory.clone(), memory.clone()],
        ..PromptPlan::default()
    };
    assert_eq!(
        ordered_twice.validate(),
        Err(PromptPlanError::DuplicateOrder {
            section: memory.clone()
        })
    );
    let placed_twice = PromptPlan {
        placements: vec![
            PromptSectionPlacement {
                section: memory.clone(),
                placement: PromptPlacement::CurrentContext,
            },
            PromptSectionPlacement {
                section: memory.clone(),
                placement: PromptPlacement::InitialInstructions,
            },
        ],
        ..PromptPlan::default()
    };
    assert_eq!(
        placed_twice.validate(),
        Err(PromptPlanError::DuplicatePlacement { section: memory })
    );
    let inverted = PromptPlan {
        limits: PromptLimits {
            max_section_bytes: NonZeroU32::new(10).unwrap(),
            max_total_bytes: NonZeroU32::new(9).unwrap(),
            ..PromptLimits::DEFAULT
        },
        ..PromptPlan::default()
    };
    assert!(matches!(
        inverted.validate(),
        Err(PromptPlanError::SectionLimitAboveTotal { .. })
    ));
    assert_eq!(PromptPlan::default().validate(), Ok(()));
}

#[test]
fn a_prompt_snapshot_decodes_only_at_version_one() {
    let section = id("lash.standard", "intro");
    let snapshot = PromptSnapshot {
        version: PromptSnapshotVersion,
        plan: ResolvedPromptPlan {
            purpose: PromptPurpose::Turn,
            sections: vec![ResolvedPromptSection {
                section: section.clone(),
                owner: PluginRevision::new("lash.standard", lash_core_ids::BehaviorRevision::ONE),
                placement: PromptPlacement::InitialInstructions,
                placement_source: PlacementSource::PluginDefault,
                wraps: Vec::new(),
            }],
            absent_targets: Vec::new(),
            limits: PromptLimits::DEFAULT,
        },
        sections: vec![RenderedPromptSection {
            section,
            placement: PromptPlacement::InitialInstructions,
            base: RecordedSectionText::Text {
                text: PromptTextRef::of("You are helpful."),
            },
            wraps: Vec::new(),
            value: RecordedSectionText::Omitted,
        }],
    };
    let mut encoded = serde_json::to_value(&snapshot).expect("snapshot encodes");
    assert_eq!(encoded["version"], 1);
    assert_eq!(
        serde_json::from_value::<PromptSnapshot>(encoded.clone()).expect("version 1 decodes"),
        snapshot
    );
    encoded["version"] = serde_json::json!(2);
    assert!(serde_json::from_value::<PromptSnapshot>(encoded).is_err());
}
