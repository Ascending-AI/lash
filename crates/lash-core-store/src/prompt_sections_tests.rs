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
            absent_overrides: Vec::new(),
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

#[test]
fn chunked_response_mode_is_recorded_only_outside_the_body() {
    use lash_sansio::llm::types::{
        LiveRequestBody, ProviderRouteIdentity, RecordedRequestTemplate,
    };
    let route = ProviderRouteIdentity {
        provider: "fixture".into(),
        endpoint: "https://fixture.test".into(),
        model: "model".into(),
    };
    for (body, outside) in [(" { \"stream\" : true } ", None), ("{}", Some(true))] {
        let template = RecordedRequestTemplate::literal(route.clone(), true, None, body).unwrap();
        let (chunked, texts) = ChunkedRequestTemplate::chunk(&template);
        assert_eq!(chunked.transport_stream, outside);
        let encoded = serde_json::to_value(&chunked).unwrap();
        assert!(encoded.get("stream").is_none());
        let mut decoded: ChunkedRequestTemplate = serde_json::from_value(encoded).unwrap();
        let read = |reference: &BlobRef| {
            texts
                .iter()
                .find(|(blob, _)| blob == reference)
                .map(|(_, text)| text.as_str())
        };
        let assembled = decoded.assemble(read).unwrap();
        assert_eq!(assembled, template);
        let live = LiveRequestBody::fill(std::sync::Arc::new(assembled), vec![]).unwrap();
        assert_eq!(live.wire(), body);
        assert!(live.stream());
        decoded.transport_stream = if outside.is_none() { Some(false) } else { None };
        assert!(decoded.assemble(read).is_err());
    }
}

/// PREFIX-DELTA (FIG-5956): a growing wire history relists neither its
/// fragment bytes nor its index prefix. Count newly addressed bodies and
/// the admission's encoded index, independently of backend deduplication.
#[test]
fn growing_requests_submit_new_fragments_and_logarithmic_index_paths() {
    use lash_sansio::llm::types::{ProviderRouteIdentity, RecordedRequestTemplate};
    let route = ProviderRouteIdentity {
        provider: "fixture".into(),
        endpoint: "https://fixture.test".into(),
        model: "model".into(),
    };
    for rendered in [false, true] {
        let mut held = std::collections::BTreeSet::new();
        let mut messages = Vec::new();
        for ordinal in 0..256 {
            messages.push(format!(
                "{{\"role\":\"user\",\"content\":\"{ordinal}:{}\"}}",
                "x".repeat(2048)
            ));
            let wire = if rendered {
                serde_json::json!({"messages": [{"role": "user", "content": messages.join("\n")}], "stream": true}).to_string()
            } else {
                format!("{{\"messages\":[{}],\"stream\":true}}", messages.join(","))
            };
            let template =
                RecordedRequestTemplate::literal(route.clone(), true, None, wire.as_str()).unwrap();
            let (index, texts) = ChunkedRequestTemplate::chunk(&template);
            let fresh: Vec<_> = texts
                .iter()
                .filter(|(hash, _)| !held.contains(hash))
                .collect();
            let logarithm = (ordinal + 1usize).ilog2() as usize + 1;
            let submitted = fresh.iter().map(|(_, text)| text.len()).sum::<usize>();
            assert!(
                submitted <= 8192 + 4096 * logarithm,
                "call {ordinal} submitted {submitted} bytes for one new message"
            );
            let metadata = serde_json::to_string(&index).unwrap().len();
            assert!(
                metadata <= 1024,
                "call {ordinal} relisted {metadata} bytes of fragment index"
            );
            let read = |reference: &BlobRef| {
                texts
                    .iter()
                    .find(|(hash, _)| hash == reference)
                    .map(|(_, text)| text.as_str())
            };
            assert_eq!(index.assemble(read).unwrap(), template);
            held.extend(texts.into_iter().map(|(hash, _)| hash));
        }
    }
}
